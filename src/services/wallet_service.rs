// ! Hospital wallet — sub-account provisioning, deposits, escrow, webhooks.

use std::sync::Arc;

use chrono::Duration;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::models::wallet::{
    DepositInstructions, Wallet, WalletDepositRequest, WalletLedgerEntry, WithdrawResponse,
    WithdrawalRow,
};
use crate::repositories::wallet::{WalletRepoError, WalletRepository};
use crate::services::safehaven::{SafeHavenClient, SafeHavenError, TransferStatus};

#[derive(Debug, thiserror::Error)]
pub enum WalletServiceError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("wallet repository error: {0}")]
    Repo(#[from] WalletRepoError),

    #[error("SafeHaven error: {0}")]
    SafeHaven(#[from] SafeHavenError),

    #[error("wallet not found for hospital {0}")]
    WalletNotFound(Uuid),

    #[error("validation error: {0}")]
    Validation(String),
}

pub struct WalletService {
    repo: Arc<WalletRepository>,
    safehaven: Arc<SafeHavenClient>,
    pool: PgPool,
    callback_url: String,
    deposit_validity: Duration,
}

impl WalletService {
    pub fn new(repo: Arc<WalletRepository>, safehaven: Arc<SafeHavenClient>, pool: PgPool) -> Self {
        let callback_url = std::env::var("SAFEHAVEN_CALLBACK_URL").unwrap_or_default();
        Self {
            repo,
            safehaven,
            pool,
            callback_url,
            deposit_validity: Duration::hours(24),
        }
    }

    /// Ensure a wallet row exists for the hospital (no SafeHaven call).
    pub async fn ensure_wallet(&self, hospital_id: Uuid) -> Result<(), WalletServiceError> {
        self.repo.ensure_wallet_row(hospital_id).await?;
        Ok(())
    }

    /// Whether the hospital already has a provisioned SafeHaven sub-account.
    pub async fn has_sub_account(&self, hospital_id: Uuid) -> Result<bool, WalletServiceError> {
        Ok(self
            .repo
            .find_wallet(hospital_id)
            .await?
            .map(|w| w.safehaven_account_id.is_some())
            .unwrap_or(false))
    }

    /// Step 1 of sub-account provisioning: ask SafeHaven to initiate a fresh BVN
    /// verification (sends an OTP to the registered phone) and stash the returned
    /// identityId + BVN so the provision step can complete it. `bvn` is the
    /// hospital admin's verified BVN (caller supplies the decrypted value).
    pub async fn initiate_sub_account(
        &self,
        hospital_id: Uuid,
        bvn: &str,
    ) -> Result<(), WalletServiceError> {
        self.repo.ensure_wallet_row(hospital_id).await?;
        if self.has_sub_account(hospital_id).await? {
            return Err(WalletServiceError::Validation(
                "Sub-account already provisioned".to_string(),
            ));
        }

        let identity_id = self
            .safehaven
            .initiate_identity_verification("BVN", bvn)
            .await?;

        self.repo
            .save_provisioning_state(hospital_id, &identity_id, bvn)
            .await?;
        Ok(())
    }

    /// Step 2: complete provisioning with the OTP the admin received. Calls
    /// SafeHaven /accounts/v2/subaccount with the stashed identityId + BVN + otp.
    pub async fn provision_sub_account(
        &self,
        hospital_id: Uuid,
        phone_number: &str,
        email: &str,
        otp: &str,
    ) -> Result<(), WalletServiceError> {
        if self.has_sub_account(hospital_id).await? {
            return Ok(());
        }

        let (identity_id, bvn) = self
            .repo
            .get_provisioning_state(hospital_id)
            .await?
            .ok_or_else(|| {
                WalletServiceError::Validation(
                    "No sub-account provisioning in progress; call initiate first".to_string(),
                )
            })?;

        let callback = (!self.callback_url.trim().is_empty()).then_some(self.callback_url.as_str());

        let sub = self
            .safehaven
            .create_sub_account(
                phone_number,
                email,
                &hospital_id.to_string(),
                "BVN",
                Some(&bvn),
                Some(&identity_id),
                Some(otp),
                callback,
            )
            .await?;

        self.repo
            .save_sub_account(
                hospital_id,
                &sub.id,
                &sub.account_number,
                sub.bank_code.as_deref(),
                sub.account_name.as_deref(),
            )
            .await?;

        tracing::info!(
            "Provisioned SafeHaven sub-account for hospital {}: {} ({})",
            hospital_id,
            sub.account_number,
            sub.id
        );
        Ok(())
    }

    pub async fn get_wallet(&self, hospital_id: Uuid) -> Result<Wallet, WalletServiceError> {
        self.repo.ensure_wallet_row(hospital_id).await?;
        self.repo
            .find_wallet(hospital_id)
            .await?
            .ok_or(WalletServiceError::WalletNotFound(hospital_id))
    }

    pub async fn list_ledger(
        &self,
        hospital_id: Uuid,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<WalletLedgerEntry>, i64), WalletServiceError> {
        Ok(self.repo.list_ledger(hospital_id, page, page_size).await?)
    }

    pub async fn list_deposits(
        &self,
        hospital_id: Uuid,
        limit: i64,
    ) -> Result<Vec<WalletDepositRequest>, WalletServiceError> {
        Ok(self.repo.list_deposit_requests(hospital_id, limit).await?)
    }

    /// Mint a one-shot virtual account at SafeHaven and record a pending deposit

    /// Return the hospital's sub-account funding instructions. Hospitals fund
    /// their wallet by transferring into their dedicated SafeHaven sub-account;
    /// the inbound `account.credit` webhook then credits `balance_kobo`.
    ///
    /// (Per-deposit virtual accounts are unavailable while the SafeHaven account
    /// is KYC-restricted, so this replaces the old `create_virtual_account` path.)
    pub async fn deposit_instructions(
        &self,
        hospital_id: Uuid,
        amount_kobo: i64,
    ) -> Result<DepositInstructions, WalletServiceError> {
        if amount_kobo < 100_000 {
            return Err(WalletServiceError::Validation(
                "Minimum deposit is ₦1,000".to_string(),
            ));
        }

        self.repo.ensure_wallet_row(hospital_id).await?;
        let wallet = self
            .repo
            .find_wallet(hospital_id)
            .await?
            .ok_or(WalletServiceError::WalletNotFound(hospital_id))?;

        let account_number = match wallet
            .safehaven_account_number
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            Some(a) => a.to_string(),
            None => {
                return Err(WalletServiceError::Validation(
                    "No wallet account yet. Create your wallet first.".to_string(),
                ))
            }
        };

        Ok(DepositInstructions {
            account_number,
            bank_name: "Safe Haven MFB".to_string(),
            bank_code: wallet.safehaven_bank_code.clone(),
            account_name: wallet.safehaven_account_name.clone(),
            amount_kobo,
            instructions:
                "Transfer the amount into the account above to fund your wallet. \
                 Your balance updates automatically once the transfer is received."
                    .to_string(),
        })
    }

    /// Withdraw available wallet funds to an external bank account. Validates the
    /// destination via name-enquiry, atomically debits `balance_kobo` and records
    /// a pending `withdrawal` billing row + ledger entry, then instructs SafeHaven
    /// to transfer OUT OF the hospital's own sub-account (where its deposits sit).
    /// A synchronous transfer rejection fully refunds the debit.
    pub async fn withdraw(
        &self,
        hospital_id: Uuid,
        amount_kobo: i64,
        bank_code: &str,
        account_number: &str,
        narration: Option<&str>,
    ) -> Result<WithdrawResponse, WalletServiceError> {
        if amount_kobo < 10_000 {
            return Err(WalletServiceError::Validation(
                "Minimum withdrawal is ₦100".to_string(),
            ));
        }

        self.repo.ensure_wallet_row(hospital_id).await?;
        let wallet = self
            .repo
            .find_wallet(hospital_id)
            .await?
            .ok_or(WalletServiceError::WalletNotFound(hospital_id))?;

        // Funds are physically held in the hospital's SafeHaven sub-account; a
        // withdrawal debits that account. No sub-account => nothing to withdraw.
        let debit_source = wallet
            .safehaven_account_number
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                WalletServiceError::Validation(
                    "No wallet account yet. Create your wallet first.".to_string(),
                )
            })?;

        if wallet.balance_kobo < amount_kobo {
            return Err(WalletServiceError::Repo(
                WalletRepoError::InsufficientBalance {
                    required: amount_kobo,
                    available: wallet.balance_kobo,
                },
            ));
        }

        // Validate the destination up front (also yields the account holder name).
        let resolved = self.safehaven.name_enquiry(bank_code, account_number).await?;

        // Atomically debit available balance + record the pending withdrawal. The
        // guarded UPDATE also protects against a concurrent debit racing this one.
        let mut tx = self.pool.begin().await?;
        let debited = sqlx::query(
            r#"
            UPDATE hospital_wallets
               SET balance_kobo = balance_kobo - $2,
                   updated_at   = NOW()
             WHERE hospital_id = $1 AND balance_kobo >= $2
            "#,
        )
        .bind(hospital_id)
        .bind(amount_kobo)
        .execute(&mut *tx)
        .await?;
        if debited.rows_affected() != 1 {
            tx.rollback().await.ok();
            return Err(WalletServiceError::Repo(
                WalletRepoError::InsufficientBalance {
                    required: amount_kobo,
                    available: wallet.balance_kobo,
                },
            ));
        }

        let description = format!("Wallet withdrawal to {bank_code}/{account_number}");
        let withdrawal_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO billing_transactions (
                hospital_id, event_type, amount_kobo, currency, status,
                provider, description
            )
            VALUES ($1, 'withdrawal', $2, 'NGN', 'pending', 'safehaven', $3)
            RETURNING id
            "#,
        )
        .bind(hospital_id)
        .bind(amount_kobo)
        .bind(&description)
        .fetch_one(&mut *tx)
        .await?;

        self.repo
            .insert_ledger_entry_in_tx(
                &mut tx,
                hospital_id,
                "withdrawal_debit",
                -amount_kobo,
                0,
                None,
                Some(&withdrawal_id.to_string()),
                Some("wallet withdrawal to bank"),
            )
            .await?;
        tx.commit().await?;

        // Instruct SafeHaven to pay out FROM the hospital's sub-account.
        let payment_reference = withdrawal_id.to_string();
        let narr = narration.unwrap_or("NexusCare wallet withdrawal");

        match self
            .safehaven
            .transfer(
                bank_code,
                account_number,
                amount_kobo / 100,
                narr,
                &payment_reference,
                Some(&debit_source),
            )
            .await
        {
            Ok(receipt) => {
                let raw_status = receipt
                    .raw
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let completed =
                    matches!(raw_status.as_str(), "completed" | "success" | "successful");
                let status = if completed { "success" } else { "pending" };

                sqlx::query(
                    r#"
                    UPDATE billing_transactions
                       SET status                  = $2::transaction_status,
                           provider_reference      = $3,
                           provider_transaction_id = $4,
                           completed_at            = CASE WHEN $2 = 'success' THEN NOW() ELSE completed_at END,
                           updated_at              = NOW()
                     WHERE id = $1
                    "#,
                )
                .bind(withdrawal_id)
                .bind(status)
                .bind(&receipt.payment_reference)
                .bind(&receipt.session_id)
                .execute(&self.pool)
                .await?;

                tracing::info!(
                    "Withdrawal {} for hospital {} -> ₦{} to {}/{} ({})",
                    withdrawal_id,
                    hospital_id,
                    amount_kobo / 100,
                    bank_code,
                    account_number,
                    status
                );

                Ok(WithdrawResponse {
                    withdrawal_id,
                    amount_kobo,
                    account_number: account_number.to_string(),
                    account_name: resolved.account_name,
                    bank_code: bank_code.to_string(),
                    status: status.to_string(),
                    reference: receipt.payment_reference,
                    message: if completed {
                        "Withdrawal completed.".to_string()
                    } else {
                        "Withdrawal is processing; funds will arrive shortly.".to_string()
                    },
                })
            }
            Err(e) => {
                // Synchronous rejection — refund the debit so the balance is restored.
                self.refund_withdrawal(hospital_id, withdrawal_id, amount_kobo, &e.to_string())
                    .await?;
                tracing::error!(
                    "Withdrawal {} for hospital {} failed at SafeHaven (refunded): {}",
                    withdrawal_id,
                    hospital_id,
                    e
                );
                // SafeHaven debits its transfer fee from the same account, so a
                // withdrawal of the full balance fails for lack of fee headroom.
                // Surface a clear, actionable message instead of the raw provider
                // error (balance is already refunded above).
                if e.to_string().to_lowercase().contains("sufficient fund") {
                    return Err(WalletServiceError::Validation(format!(
                        "Withdrawal declined: ₦{} plus SafeHaven's transfer fee exceeds your \
                         available balance. Try a slightly lower amount.",
                        amount_kobo / 100
                    )));
                }
                Err(WalletServiceError::SafeHaven(e))
            }
        }
    }

    /// Reverse a committed withdrawal debit and mark the billing row failed, in
    /// one tx: re-credit `balance_kobo` and write a `withdrawal_reversal` ledger
    /// entry.
    async fn refund_withdrawal(
        &self,
        hospital_id: Uuid,
        withdrawal_id: Uuid,
        amount_kobo: i64,
        error: &str,
    ) -> Result<(), WalletServiceError> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            r#"
            UPDATE billing_transactions
               SET status      = 'failed',
                   description = COALESCE(description, '') || E'\nError: ' || $2,
                   updated_at  = NOW()
             WHERE id = $1
            "#,
        )
        .bind(withdrawal_id)
        .bind(error)
        .execute(&mut *tx)
        .await?;

        self.repo
            .insert_ledger_entry_in_tx(
                &mut tx,
                hospital_id,
                "withdrawal_reversal",
                amount_kobo,
                0,
                None,
                Some(&withdrawal_id.to_string()),
                Some("withdrawal re-credited after failed transfer"),
            )
            .await?;

        sqlx::query(
            r#"
            UPDATE hospital_wallets
               SET balance_kobo = balance_kobo + $2,
                   updated_at   = NOW()
             WHERE hospital_id = $1
            "#,
        )
        .bind(hospital_id)
        .bind(amount_kobo)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// List a hospital's withdrawals (newest first).
    pub async fn list_withdrawals(
        &self,
        hospital_id: Uuid,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<WithdrawalRow>, i64), WalletServiceError> {
        let page = page.max(1);
        let page_size = page_size.clamp(1, 200);
        let offset = (page - 1) * page_size;

        let rows = sqlx::query_as::<_, WithdrawalRow>(
            r#"
            SELECT id, amount_kobo, status::text AS status, provider_reference,
                   provider_transaction_id, description, created_at, completed_at
            FROM billing_transactions
            WHERE hospital_id = $1 AND event_type = 'withdrawal'
            ORDER BY created_at DESC
            LIMIT $2 OFFSET $3
            "#,
        )
        .bind(hospital_id)
        .bind(page_size)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM billing_transactions WHERE hospital_id = $1 AND event_type = 'withdrawal'",
        )
        .bind(hospital_id)
        .fetch_one(&self.pool)
        .await?;

        Ok((rows, total))
    }

    /// Refresh a pending withdrawal's status from SafeHaven, settling it
    /// (success, or failed + refund) as appropriate. Scoped to the caller's
    /// hospital. Returns the current stored status string.
    pub async fn refresh_withdrawal_status(
        &self,
        hospital_id: Uuid,
        withdrawal_id: Uuid,
    ) -> Result<String, WalletServiceError> {
        let row: Option<(String, Option<String>)> = sqlx::query_as(
            r#"SELECT status::text, provider_reference
               FROM billing_transactions
               WHERE id = $1 AND hospital_id = $2 AND event_type = 'withdrawal'"#,
        )
        .bind(withdrawal_id)
        .bind(hospital_id)
        .fetch_optional(&self.pool)
        .await?;

        let (status, provider_reference) = match row {
            Some(r) => r,
            None => return Ok("not_found".to_string()),
        };
        if status != "pending" {
            return Ok(status);
        }
        // We always send the withdrawal id as SafeHaven's paymentReference, so
        // fall back to it if provider_reference wasn't persisted.
        let reference = provider_reference
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| withdrawal_id.to_string());

        match self.safehaven.transfer_status(&reference).await? {
            TransferStatus::Completed => {
                sqlx::query(
                    r#"UPDATE billing_transactions
                          SET status = 'success', completed_at = NOW(), updated_at = NOW()
                        WHERE id = $1 AND status = 'pending'"#,
                )
                .bind(withdrawal_id)
                .execute(&self.pool)
                .await?;
                Ok("success".to_string())
            }
            TransferStatus::Failed | TransferStatus::Cancelled => {
                let amount_kobo: i64 = sqlx::query_scalar(
                    "SELECT amount_kobo FROM billing_transactions WHERE id = $1",
                )
                .bind(withdrawal_id)
                .fetch_one(&self.pool)
                .await?;
                self.refund_withdrawal(
                    hospital_id,
                    withdrawal_id,
                    amount_kobo,
                    "transfer reported failed by SafeHaven",
                )
                .await?;
                Ok("failed".to_string())
            }
            // Created/Initiated/Processing/Unknown → still in flight.
            _ => Ok("pending".to_string()),
        }
    }

    /// Reconcile the hospital's wallet against SafeHaven: pull the sub-account's
    /// transaction history and credit any inbound transfer that was never
    /// received via webhook (e.g. a webhook posted to a stale callback URL).
    /// Idempotent — transfers already credited (matched by reference, or by the
    /// webhook-event dedup inside `process_webhook`) are skipped.
    pub async fn reconcile_deposits(
        &self,
        hospital_id: Uuid,
    ) -> Result<ReconcileResult, WalletServiceError> {
        self.repo.ensure_wallet_row(hospital_id).await?;
        let wallet = self
            .repo
            .find_wallet(hospital_id)
            .await?
            .ok_or(WalletServiceError::WalletNotFound(hospital_id))?;

        let account_id = wallet
            .safehaven_account_id
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                WalletServiceError::Validation(
                    "No wallet account yet. Create your wallet first.".to_string(),
                )
            })?
            .to_string();
        let sub_account_number = wallet.safehaven_account_number.clone().unwrap_or_default();

        let body = self
            .safehaven
            .list_transfers(&account_id, 0, 100, None)
            .await?;
        let txns = body
            .get("data")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut scanned = 0i64;
        let mut credited = 0i64;
        let mut credited_kobo = 0i64;

        for tx in txns {
            // Only inbound (credit) transfers.
            let dir = tx.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if !dir.eq_ignore_ascii_case("Inwards") {
                continue;
            }
            scanned += 1;

            // Skip transfers already credited (matched on any known reference).
            let refs: Vec<String> = ["paymentReference", "sessionId", "_id", "reference"]
                .iter()
                .filter_map(|k| tx.get(*k).and_then(|v| v.as_str()))
                .map(str::to_string)
                .collect();
            if !refs.is_empty() {
                let exists: Option<i32> = sqlx::query_scalar(
                    "SELECT 1 FROM wallet_deposit_requests WHERE external_reference = ANY($1) LIMIT 1",
                )
                .bind(&refs)
                .fetch_optional(&self.pool)
                .await?;
                if exists.is_some() {
                    continue;
                }
            }

            // Replay through the webhook pipeline (idempotent via webhook_events).
            // Inject the credited account so the inflow maps back to this hospital.
            let mut data_obj = tx.clone();
            if let Some(obj) = data_obj.as_object_mut() {
                obj.insert(
                    "creditAccountNumber".to_string(),
                    serde_json::Value::String(sub_account_number.clone()),
                );
            }
            let payload = serde_json::json!({ "type": "transfer", "data": data_obj });
            match self.process_webhook(&payload).await {
                Ok(WebhookOutcome::DepositCredited { amount_kobo, .. }) => {
                    credited += 1;
                    credited_kobo += amount_kobo;
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("reconcile: failed to credit a transfer: {e}"),
            }
        }

        let balance_kobo = self
            .repo
            .find_wallet(hospital_id)
            .await?
            .map(|w| w.balance_kobo)
            .unwrap_or(0);

        tracing::info!(
            "Reconcile hospital {}: scanned {} inbound, credited {} (₦{})",
            hospital_id,
            scanned,
            credited,
            credited_kobo / 100
        );

        Ok(ReconcileResult {
            transactions_scanned: scanned,
            deposits_credited: credited,
            amount_credited_kobo: credited_kobo,
            balance_kobo,
        })
    }

    pub async fn try_hold_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hospital_id: Uuid,
        shift_id: Option<Uuid>,
        amount_kobo: i64,
    ) -> Result<(), WalletServiceError> {
        self.repo
            .try_hold_in_tx(tx, hospital_id, shift_id, amount_kobo)
            .await?;
        Ok(())
    }

    pub async fn release_hold_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hospital_id: Uuid,
        shift_id: Option<Uuid>,
        amount_kobo: i64,
    ) -> Result<(), WalletServiceError> {
        self.repo
            .release_hold_in_tx(tx, hospital_id, shift_id, amount_kobo)
            .await?;
        Ok(())
    }

    /// Process a SafeHaven webhook. Idempotent via `webhook_events.provider_event_id`

    pub async fn process_webhook(
        &self,
        payload: &serde_json::Value,
    ) -> Result<WebhookOutcome, WalletServiceError> {
        // Idempotency id: prefer the transaction id / sessionId / paymentReference,
        // looked up at top level or under `data` (SafeHaven nests transaction
        // details under `data`).
        let event_id = first_str(
            payload,
            &["_id", "sessionId", "paymentReference"],
        )
        .or_else(|| {
            payload
                .get("data")
                .and_then(|d| first_str(d, &["_id", "sessionId", "paymentReference"]))
        });
        // SafeHaven sends the event under `eventType`; accept `type`/`event` too.
        let event_type = payload
            .get("eventType")
            .or_else(|| payload.get("type"))
            .or_else(|| payload.get("event"))
            .and_then(|v| v.as_str());

        let inserted = self
            .repo
            .insert_webhook_event_if_new(event_id, event_type, payload)
            .await?;
        let webhook_event_id = match inserted {
            Some(id) => id,
            None => return Ok(WebhookOutcome::AlreadySeen),
        };

        // SafeHaven's real inbound-credit event is `type: "transfer"` with
        // `data.type: "Inwards"` (confirmed from a live capture); `account.credit`
        // / `virtualAccount.transfer` / `subaccount.inflow` are kept defensively.
        let data_dir = payload
            .get("data")
            .and_then(|d| d.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let result = match event_type {
            Some("virtualAccount.transfer") | Some("transfer.inflow") => {
                self.handle_virtual_account_transfer(payload).await
            }
            Some("account.credit") | Some("subaccount.inflow") => {
                self.handle_subaccount_inflow(payload).await
            }
            // Generic transfer: only credit inbound ("Inwards") transfers.
            Some("transfer") if data_dir.eq_ignore_ascii_case("Inwards") => {
                self.handle_subaccount_inflow(payload).await
            }
            _ => {
                tracing::info!(
                    "SafeHaven webhook ignored (event_type={:?}, data.type={:?})",
                    event_type,
                    data_dir
                );
                Ok(WebhookOutcome::Ignored)
            }
        };

        match &result {
            Ok(_) => {
                self.repo
                    .mark_webhook_processed(webhook_event_id, None)
                    .await?;
            }
            Err(e) => {
                let _ = self
                    .repo
                    .mark_webhook_processed(webhook_event_id, Some(&e.to_string()))
                    .await;
            }
        }
        result
    }

    async fn handle_virtual_account_transfer(
        &self,
        payload: &serde_json::Value,
    ) -> Result<WebhookOutcome, WalletServiceError> {
        let data = payload
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let status = data.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let success = matches!(
            status.to_ascii_lowercase().as_str(),
            "completed" | "success"
        );
        if !success {
            return Ok(WebhookOutcome::Ignored);
        }

        let external_reference = data
            .get("externalReference")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let credit_account_number = data
            .get("creditAccountNumber")
            .or_else(|| data.get("destinationAccountNumber"))
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let deposit = match &external_reference {
            Some(ext) => self.repo.find_deposit_by_external_ref(ext).await?,
            None => None,
        };
        let deposit = match deposit {
            Some(d) => Some(d),
            None => match &credit_account_number {
                Some(acct) => {
                    self.repo
                        .find_pending_deposit_by_account_number(acct)
                        .await?
                }
                None => None,
            },
        };
        let deposit = match deposit {
            Some(d) => d,
            None => return Ok(WebhookOutcome::Ignored),
        };

        // SafeHaven returns amounts in NGN; convert to kobo.
        let received_amount_kobo = data
            .get("amount")
            .and_then(|v| v.as_i64())
            .map(|n| n * 100)
            .unwrap_or(deposit.amount_kobo);

        let provider_reference = data
            .get("paymentReference")
            .or_else(|| data.get("reference"))
            .and_then(|v| v.as_str());

        let mut tx = self.pool.begin().await?;
        let (hospital_id, _ledger_id) = self
            .repo
            .complete_deposit_in_tx(
                &mut tx,
                deposit.id,
                received_amount_kobo,
                provider_reference,
                payload,
            )
            .await?;
        tx.commit().await?;

        tracing::info!(
            "Wallet credited: hospital {} <- ₦{} (deposit {})",
            hospital_id,
            received_amount_kobo / 100,
            deposit.id
        );
        Ok(WebhookOutcome::DepositCredited {
            deposit_id: deposit.id,
            hospital_id,
            amount_kobo: received_amount_kobo,
        })
    }

    /// Hospital wired straight to its sub-account, bypassing the virtual-account flow

    async fn handle_subaccount_inflow(
        &self,
        payload: &serde_json::Value,
    ) -> Result<WebhookOutcome, WalletServiceError> {
        // Only credit on a successful/completed transfer.
        let status = nested_or_top(payload, &["status"])
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(status.as_str(), "completed" | "success" | "successful") {
            return Ok(WebhookOutcome::Ignored);
        }

        let dest_account = nested_or_top(
            payload,
            &["creditAccountNumber", "destinationAccountNumber", "accountNumber"],
        )
        .and_then(|v| v.as_str())
        .map(str::to_string);

        let amount_kobo = nested_or_top(payload, &["amount"])
            .map(amount_to_kobo)
            .unwrap_or(0);

        if amount_kobo <= 0 || dest_account.is_none() {
            return Ok(WebhookOutcome::Ignored);
        }
        let dest_account = dest_account.unwrap();

        let hospital_id: Option<Uuid> = sqlx::query_scalar(
            r#"SELECT hospital_id FROM hospital_wallets
               WHERE safehaven_account_number = $1 LIMIT 1"#,
        )
        .bind(&dest_account)
        .fetch_optional(&self.pool)
        .await?;

        let hospital_id = match hospital_id {
            Some(h) => h,
            None => return Ok(WebhookOutcome::Ignored),
        };

        let provider_reference =
            nested_or_top(payload, &["paymentReference", "reference", "sessionId"])
                .and_then(|v| v.as_str());
        let sender_name =
            nested_or_top(payload, &["debitAccountName", "senderName", "originatorName"])
                .and_then(|v| v.as_str());

        let mut tx = self.pool.begin().await?;

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
        .bind(amount_kobo)
        .execute(&mut *tx)
        .await?;

        self.repo
            .insert_ledger_entry_in_tx(
                &mut tx,
                hospital_id,
                "deposit_credit",
                amount_kobo,
                0,
                None,
                provider_reference,
                Some("sub-account inflow"),
            )
            .await?;

        // Record a deposit-history row so GET /wallet/deposits shows top-ups.
        // External reference must be unique; use the provider ref (fallback uuid).
        let external_reference = provider_reference
            .map(str::to_string)
            .unwrap_or_else(|| format!("inflow_{}", Uuid::new_v4()));
        let deposit_id = self
            .repo
            .insert_received_deposit(
                &mut tx,
                hospital_id,
                amount_kobo,
                &dest_account,
                sender_name,
                &external_reference,
                payload,
            )
            .await?;

        tx.commit().await?;

        tracing::info!(
            "Wallet credited: hospital {} <- ₦{} (sub-account inflow {})",
            hospital_id,
            amount_kobo / 100,
            external_reference
        );

        Ok(WebhookOutcome::DepositCredited {
            deposit_id,
            hospital_id,
            amount_kobo,
        })
    }
}

/// Result of a deposit reconcile against SafeHaven's transaction history.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct ReconcileResult {
    /// Inbound transactions examined.
    pub transactions_scanned: i64,
    /// Previously-missed deposits credited during this run.
    pub deposits_credited: i64,
    /// Total kobo credited during this run.
    pub amount_credited_kobo: i64,
    /// Wallet balance after reconciling.
    pub balance_kobo: i64,
}

#[derive(Debug, Clone)]
pub enum WebhookOutcome {
    AlreadySeen,
    Ignored,
    DepositCredited {
        deposit_id: Uuid,
        hospital_id: Uuid,
        amount_kobo: i64,
    },
}

/// Return the first present, non-empty string among `keys` on a JSON object.
fn first_str<'a>(v: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    for &k in keys {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
    }
    None
}

/// Look up a field on the payload, checking `data.<key>` first then top level.
fn nested_or_top<'a>(
    payload: &'a serde_json::Value,
    keys: &[&str],
) -> Option<&'a serde_json::Value> {
    if let Some(data) = payload.get("data") {
        for &k in keys {
            if let Some(val) = data.get(k) {
                if !val.is_null() {
                    return Some(val);
                }
            }
        }
    }
    for &k in keys {
        if let Some(val) = payload.get(k) {
            if !val.is_null() {
                return Some(val);
            }
        }
    }
    None
}

/// Convert a SafeHaven amount to kobo. SafeHaven sends transfer amounts in
/// naira (possibly a decimal like `50` or `50.00`), so multiply by 100. Accepts
/// numeric or string JSON.
fn amount_to_kobo(v: &serde_json::Value) -> i64 {
    let naira = match v {
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
        serde_json::Value::String(s) => s.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    };
    (naira * 100.0).round() as i64
}
