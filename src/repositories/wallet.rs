// ! Wallet persistence. Schema in `migrations/20240032_hospital_wallet.sql`.

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::models::wallet::{Wallet, WalletDepositRequest, WalletLedgerEntry};

#[derive(Debug, thiserror::Error)]
pub enum WalletRepoError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("insufficient wallet balance: required {required}, available {available}")]
    InsufficientBalance { required: i64, available: i64 },

    #[error("no funds to release: shift {0} has no matching hold")]
    NothingToRelease(Uuid),
}

pub struct WalletRepository {
    pool: PgPool,
}

impl WalletRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Idempotently create the wallet row for a hospital

    pub async fn ensure_wallet_row(&self, hospital_id: Uuid) -> Result<(), WalletRepoError> {
        sqlx::query(
            r#"
            INSERT INTO hospital_wallets (hospital_id)
            VALUES ($1)
            ON CONFLICT (hospital_id) DO NOTHING
            "#,
        )
        .bind(hospital_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn find_wallet(&self, hospital_id: Uuid) -> Result<Option<Wallet>, WalletRepoError> {
        let w = sqlx::query_as::<_, Wallet>(
            r#"
            SELECT hospital_id, safehaven_account_id, safehaven_account_number,
                   safehaven_bank_code, safehaven_account_name,
                   balance_kobo, held_kobo, created_at, updated_at
            FROM hospital_wallets
            WHERE hospital_id = $1
            "#,
        )
        .bind(hospital_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(w)
    }

    /// Persist SafeHaven sub-account details. COALESCE keeps any field already on file

    pub async fn save_sub_account(
        &self,
        hospital_id: Uuid,
        account_id: &str,
        account_number: &str,
        bank_code: Option<&str>,
        account_name: Option<&str>,
    ) -> Result<(), WalletRepoError> {
        sqlx::query(
            r#"
            INSERT INTO hospital_wallets
                (hospital_id, safehaven_account_id, safehaven_account_number,
                 safehaven_bank_code, safehaven_account_name)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (hospital_id) DO UPDATE
              SET safehaven_account_id     = COALESCE(hospital_wallets.safehaven_account_id,     EXCLUDED.safehaven_account_id),
                  safehaven_account_number = COALESCE(hospital_wallets.safehaven_account_number, EXCLUDED.safehaven_account_number),
                  safehaven_bank_code      = COALESCE(hospital_wallets.safehaven_bank_code,      EXCLUDED.safehaven_bank_code),
                  safehaven_account_name   = COALESCE(hospital_wallets.safehaven_account_name,   EXCLUDED.safehaven_account_name),
                  updated_at               = NOW()
            "#,
        )
        .bind(hospital_id)
        .bind(account_id)
        .bind(account_number)
        .bind(bank_code)
        .bind(account_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stash the fresh verification id + BVN from a sub-account provisioning
    /// initiate, so the provision step can pass identityId + otp.
    pub async fn save_provisioning_state(
        &self,
        hospital_id: Uuid,
        identity_id: &str,
        bvn: &str,
    ) -> Result<(), WalletRepoError> {
        sqlx::query(
            r#"
            INSERT INTO hospital_wallets
                (hospital_id, provisioning_identity_id, provisioning_bvn)
            VALUES ($1, $2, $3)
            ON CONFLICT (hospital_id) DO UPDATE
              SET provisioning_identity_id = EXCLUDED.provisioning_identity_id,
                  provisioning_bvn         = EXCLUDED.provisioning_bvn,
                  updated_at               = NOW()
            "#,
        )
        .bind(hospital_id)
        .bind(identity_id)
        .bind(bvn)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Read the pending provisioning state (identity_id, bvn).
    pub async fn get_provisioning_state(
        &self,
        hospital_id: Uuid,
    ) -> Result<Option<(String, String)>, WalletRepoError> {
        let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(
            r#"
            SELECT provisioning_identity_id, provisioning_bvn
            FROM hospital_wallets
            WHERE hospital_id = $1
            "#,
        )
        .bind(hospital_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(id, bvn)| match (id, bvn) {
            (Some(id), Some(bvn)) => Some((id, bvn)),
            _ => None,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_ledger_entry_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hospital_id: Uuid,
        kind: &str,
        delta_balance_kobo: i64,
        delta_held_kobo: i64,
        shift_id: Option<Uuid>,
        provider_reference: Option<&str>,
        notes: Option<&str>,
    ) -> Result<Uuid, WalletRepoError> {
        let id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO wallet_ledger_entries
                (hospital_id, kind, delta_balance_kobo, delta_held_kobo,
                 shift_id, provider_reference, notes)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING id
            "#,
        )
        .bind(hospital_id)
        .bind(kind)
        .bind(delta_balance_kobo)
        .bind(delta_held_kobo)
        .bind(shift_id)
        .bind(provider_reference)
        .bind(notes)
        .fetch_one(&mut **tx)
        .await?;
        Ok(id)
    }

    pub async fn list_ledger(
        &self,
        hospital_id: Uuid,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<WalletLedgerEntry>, i64), WalletRepoError> {
        let page = page.max(1);
        let page_size = page_size.clamp(1, 200);
        let offset = (page - 1) * page_size;

        let rows = sqlx::query_as::<_, WalletLedgerEntry>(
            r#"
            SELECT id, hospital_id, kind, delta_balance_kobo, delta_held_kobo,
                   shift_id, provider_reference, notes, created_at
            FROM wallet_ledger_entries
            WHERE hospital_id = $1
            ORDER BY created_at DESC, id DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(hospital_id)
        .bind(page_size)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM wallet_ledger_entries WHERE hospital_id = $1")
                .bind(hospital_id)
                .fetch_one(&self.pool)
                .await?;

        Ok((rows, total))
    }

    /// Move funds from `balance_kobo` into `held_kobo`. `SELECT … FOR UPDATE`

    pub async fn try_hold_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hospital_id: Uuid,
        shift_id: Option<Uuid>,
        amount_kobo: i64,
    ) -> Result<i64, WalletRepoError> {
        if amount_kobo <= 0 {
            return Ok(0);
        }

        let row: Option<(i64,)> = sqlx::query_as(
            r#"
            SELECT balance_kobo
            FROM hospital_wallets
            WHERE hospital_id = $1
            FOR UPDATE
            "#,
        )
        .bind(hospital_id)
        .fetch_optional(&mut **tx)
        .await?;

        let available = row.map(|(b,)| b).unwrap_or(0);
        if available < amount_kobo {
            return Err(WalletRepoError::InsufficientBalance {
                required: amount_kobo,
                available,
            });
        }

        sqlx::query(
            r#"
            UPDATE hospital_wallets
               SET balance_kobo = balance_kobo - $2,
                   held_kobo    = held_kobo    + $2,
                   updated_at   = NOW()
             WHERE hospital_id = $1
            "#,
        )
        .bind(hospital_id)
        .bind(amount_kobo)
        .execute(&mut **tx)
        .await?;

        self.insert_ledger_entry_in_tx(
            tx,
            hospital_id,
            "shift_hold",
            -amount_kobo,
            amount_kobo,
            shift_id,
            None,
            None,
        )
        .await?;

        Ok(amount_kobo)
    }

    /// Inverse of `try_hold_in_tx` — escrowed funds go back to the available balance

    pub async fn release_hold_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hospital_id: Uuid,
        shift_id: Option<Uuid>,
        amount_kobo: i64,
    ) -> Result<(), WalletRepoError> {
        if amount_kobo <= 0 {
            return Ok(());
        }

        let row: Option<(i64,)> = sqlx::query_as(
            r#"
            SELECT held_kobo
            FROM hospital_wallets
            WHERE hospital_id = $1
            FOR UPDATE
            "#,
        )
        .bind(hospital_id)
        .fetch_optional(&mut **tx)
        .await?;

        let held = row.map(|(h,)| h).unwrap_or(0);
        if held < amount_kobo {
            return Err(WalletRepoError::NothingToRelease(
                shift_id.unwrap_or_else(Uuid::nil),
            ));
        }

        sqlx::query(
            r#"
            UPDATE hospital_wallets
               SET balance_kobo = balance_kobo + $2,
                   held_kobo    = held_kobo    - $2,
                   updated_at   = NOW()
             WHERE hospital_id = $1
            "#,
        )
        .bind(hospital_id)
        .bind(amount_kobo)
        .execute(&mut **tx)
        .await?;

        self.insert_ledger_entry_in_tx(
            tx,
            hospital_id,
            "shift_release",
            amount_kobo,
            -amount_kobo,
            shift_id,
            None,
            Some("shift cancelled or expired"),
        )
        .await?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn insert_deposit_request(
        &self,
        hospital_id: Uuid,
        amount_kobo: i64,
        virtual_account_number: &str,
        virtual_bank_code: Option<&str>,
        virtual_account_name: Option<&str>,
        valid_until: DateTime<Utc>,
        external_reference: &str,
    ) -> Result<WalletDepositRequest, WalletRepoError> {
        let row = sqlx::query_as::<_, WalletDepositRequest>(
            r#"
            INSERT INTO wallet_deposit_requests
                (hospital_id, amount_kobo, virtual_account_number,
                 virtual_bank_code, virtual_account_name, valid_until,
                 external_reference)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            RETURNING id, hospital_id, amount_kobo, virtual_account_number,
                      virtual_bank_code, virtual_account_name, valid_until,
                      external_reference, status::text AS status, received_at,
                      received_amount_kobo, created_at, updated_at
            "#,
        )
        .bind(hospital_id)
        .bind(amount_kobo)
        .bind(virtual_account_number)
        .bind(virtual_bank_code)
        .bind(virtual_account_name)
        .bind(valid_until)
        .bind(external_reference)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Record an already-received deposit (e.g. a direct sub-account inflow that
    /// has no pre-created request). Inserted as `received` in the given tx.
    /// Idempotent on `external_reference`; returns the row id (existing or new).
    pub async fn insert_received_deposit(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hospital_id: Uuid,
        amount_kobo: i64,
        account_number: &str,
        account_name: Option<&str>,
        external_reference: &str,
        payload: &serde_json::Value,
    ) -> Result<Uuid, WalletRepoError> {
        let id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO wallet_deposit_requests
                (hospital_id, amount_kobo, virtual_account_number,
                 virtual_account_name, valid_until, external_reference,
                 status, received_at, received_amount_kobo, safehaven_payload)
            VALUES ($1, $2, $3, $4, NOW(), $5, 'received', NOW(), $2, $6)
            ON CONFLICT (external_reference) DO UPDATE
              SET external_reference = wallet_deposit_requests.external_reference
            RETURNING id
            "#,
        )
        .bind(hospital_id)
        .bind(amount_kobo)
        .bind(account_number)
        .bind(account_name)
        .bind(external_reference)
        .bind(payload)
        .fetch_one(&mut **tx)
        .await?;
        Ok(id)
    }

    pub async fn list_deposit_requests(
        &self,
        hospital_id: Uuid,
        limit: i64,
    ) -> Result<Vec<WalletDepositRequest>, WalletRepoError> {
        let rows = sqlx::query_as::<_, WalletDepositRequest>(
            r#"
            SELECT id, hospital_id, amount_kobo, virtual_account_number,
                   virtual_bank_code, virtual_account_name, valid_until,
                   external_reference, status::text AS status, received_at,
                   received_amount_kobo, created_at, updated_at
            FROM wallet_deposit_requests
            WHERE hospital_id = $1
            ORDER BY created_at DESC
            LIMIT $2
            "#,
        )
        .bind(hospital_id)
        .bind(limit.clamp(1, 100))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn find_deposit_by_external_ref(
        &self,
        external_reference: &str,
    ) -> Result<Option<WalletDepositRequest>, WalletRepoError> {
        let row = sqlx::query_as::<_, WalletDepositRequest>(
            r#"
            SELECT id, hospital_id, amount_kobo, virtual_account_number,
                   virtual_bank_code, virtual_account_name, valid_until,
                   external_reference, status::text AS status, received_at,
                   received_amount_kobo, created_at, updated_at
            FROM wallet_deposit_requests
            WHERE external_reference = $1
            "#,
        )
        .bind(external_reference)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn find_pending_deposit_by_account_number(
        &self,
        account_number: &str,
    ) -> Result<Option<WalletDepositRequest>, WalletRepoError> {
        let row = sqlx::query_as::<_, WalletDepositRequest>(
            r#"
            SELECT id, hospital_id, amount_kobo, virtual_account_number,
                   virtual_bank_code, virtual_account_name, valid_until,
                   external_reference, status::text AS status, received_at,
                   received_amount_kobo, created_at, updated_at
            FROM wallet_deposit_requests
            WHERE virtual_account_number = $1
              AND status = 'pending'
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(account_number)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Mark a pending deposit as received, credit the wallet, append the ledger row

    pub async fn complete_deposit_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        deposit_id: Uuid,
        received_amount_kobo: i64,
        provider_reference: Option<&str>,
        payload: &serde_json::Value,
    ) -> Result<(Uuid, Uuid), WalletRepoError> {
        let hospital_id: Uuid = sqlx::query_scalar(
            r#"
            UPDATE wallet_deposit_requests
               SET status               = 'received',
                   received_at          = NOW(),received_amount_kobo = $2,
                   safehaven_payload    = $3,
                   updated_at           = NOW()
             WHERE id = $1
               AND status = 'pending'
             RETURNING hospital_id
            "#,
        )
        .bind(deposit_id)
        .bind(received_amount_kobo)
        .bind(payload)
        .fetch_one(&mut **tx)
        .await?;

        sqlx::query(
            r#"
            INSERT INTO hospital_wallets (hospital_id, balance_kobo)
            VALUES ($1, $2)
            ON CONFLICT (hospital_id) DO UPDATE
              SET balance_kobo = hospital_wallets.balance_kobo + EXCLUDED.balance_kobo,
                  updated_at   = NOW()
            "#,
        )
        .bind(hospital_id)
        .bind(received_amount_kobo)
        .execute(&mut **tx)
        .await?;

        let ledger_id = self
            .insert_ledger_entry_in_tx(
                tx,
                hospital_id,
                "deposit_credit",
                received_amount_kobo,
                0,
                None,
                provider_reference,
                None,
            )
            .await?;

        Ok((hospital_id, ledger_id))
    }

    /// Idempotent webhook insert. Returns `Some(id)` for a new event, `None` for a duplicate

    pub async fn insert_webhook_event_if_new(
        &self,
        provider_event_id: Option<&str>,
        event_type: Option<&str>,
        payload: &serde_json::Value,
    ) -> Result<Option<Uuid>, WalletRepoError> {
        let res = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO webhook_events
                (provider, provider_event_id, event_type, raw_payload)
            VALUES ('safehaven', $1, $2, $3)
            RETURNING id
            "#,
        )
        .bind(provider_event_id)
        .bind(event_type)
        .bind(payload)
        .fetch_one(&self.pool)
        .await;

        match res {
            Ok(id) => Ok(Some(id)),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Ok(None),
            Err(e) => Err(WalletRepoError::Database(e)),
        }
    }

    pub async fn mark_webhook_processed(
        &self,
        event_id: Uuid,
        error_message: Option<&str>,
    ) -> Result<(), WalletRepoError> {
        sqlx::query(
            r#"
            UPDATE webhook_events
               SET processed     = ($2 IS NULL),
                   processed_at  = NOW(),error_message = $2
             WHERE id = $1
            "#,
        )
        .bind(event_id)
        .bind(error_message)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
