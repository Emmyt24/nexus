// ! Payout pipeline. Splits `gross_kobo` into `(gross, fee, net)` with a 10%

use std::sync::Arc;

use sqlx::PgPool;
use uuid::Uuid;

use crate::repositories::clinician::ClinicianRepository;
use crate::repositories::wallet::WalletRepository;
use crate::services::encryption::EncryptionService;
use crate::services::safehaven::{SafeHavenClient, SafeHavenError, TransferStatus};

pub const PLATFORM_FEE_NUMERATOR: i64 = 1;
pub const PLATFORM_FEE_DENOMINATOR: i64 = 10;

/// Default minimum net payout (₦5,000). Overridable at runtime with the
/// `MIN_PAYOUT_KOBO` env var (kobo) — e.g. to lower the threshold in test/staging.
pub const DEFAULT_MIN_PAYOUT_KOBO: i64 = 500_000;

/// Minimum net payout in kobo — `MIN_PAYOUT_KOBO` env if set (and >= 0), else default.
pub fn min_payout_kobo() -> i64 {
    std::env::var("MIN_PAYOUT_KOBO")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| *n >= 0)
        .unwrap_or(DEFAULT_MIN_PAYOUT_KOBO)
}

/// `(gross, fee, net)` such that `gross == fee + net`

pub fn split_payout(gross_kobo: i64) -> (i64, i64, i64) {
    let fee = gross_kobo * PLATFORM_FEE_NUMERATOR / PLATFORM_FEE_DENOMINATOR;
    let net = gross_kobo - fee;
    (gross_kobo, fee, net)
}

#[derive(Debug, thiserror::Error)]
pub enum PayoutServiceError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("encryption error: {0}")]
    Encryption(String),
    #[error("SafeHaven error: {0}")]
    SafeHaven(#[from] SafeHavenError),
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct PayableShift {
    shift_id: Uuid,
    hospital_id: Uuid,
    clinician_id: Uuid,
    grand_total_kobo: Option<i64>,
    role_title: String,
}

pub struct PayoutService {
    pool: PgPool,
    wallet_repo: Arc<WalletRepository>,
    clinician_repo: Arc<ClinicianRepository>,
    safehaven: Arc<SafeHavenClient>,
    encryption: Arc<EncryptionService>,
}

impl PayoutService {
    pub fn new(
        pool: PgPool,
        wallet_repo: Arc<WalletRepository>,
        clinician_repo: Arc<ClinicianRepository>,
        safehaven: Arc<SafeHavenClient>,
        encryption: Arc<EncryptionService>,
    ) -> Self {
        Self {
            pool,
            wallet_repo,
            clinician_repo,
            safehaven,
            encryption,
        }
    }

    /// One scheduler tick. Returns the number of transfers kicked off

    pub async fn run_tick(&self) -> Result<usize, PayoutServiceError> {
        let candidates = self.find_payable_shifts().await?;
        let mut started = 0usize;
        for s in candidates {
            match self.process_one(&s).await {
                Ok(true) => started += 1,
                Ok(false) => {}
                Err(e) => {
                    tracing::error!("Payout failed for shift {}: {}", s.shift_id, e);
                }
            }
        }
        Ok(started)
    }

    async fn find_payable_shifts(&self) -> Result<Vec<PayableShift>, PayoutServiceError> {
        let rows = sqlx::query_as::<_, PayableShift>(
            r#"
            SELECT s.id          AS shift_id,
                   s.hospital_id,
                   s.assigned_clinician_id AS clinician_id,
                   s.grand_total_kobo,
                   s.role_title
            FROM shifts s
            JOIN shift_handovers h ON h.shift_id = s.id
            WHERE s.status = 'completed'
              AND s.assigned_clinician_id IS NOT NULL
              AND s.grand_total_kobo IS NOT NULL
              AND h.hospital_approved_at IS NOT NULL
              -- payable if there is no in-flight/successful payout yet ...
              AND NOT EXISTS (
                  SELECT 1 FROM billing_transactions bt
                  WHERE bt.shift_id   = s.id
                    AND bt.event_type = 'payout'
                    AND bt.status <> 'failed'
              )
              -- ... and we have not already exhausted the retry budget (3 failures)
              AND (
                  SELECT COUNT(*) FROM billing_transactions bt
                  WHERE bt.shift_id   = s.id
                    AND bt.event_type = 'payout'
                    AND bt.status     = 'failed'
              ) < 3
            ORDER BY h.hospital_approved_at ASC
            LIMIT 50
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn process_one(&self, p: &PayableShift) -> Result<bool, PayoutServiceError> {
        let gross = p.grand_total_kobo.unwrap_or(0);
        let (gross, fee, net) = split_payout(gross);

        let min_payout = min_payout_kobo();
        if net < min_payout {
            self.record_failed_payout(
                p,
                gross,
                fee,
                net,
                &format!(
                    "below minimum payout threshold (₦{})",
                    min_payout / 100
                ),
            )
            .await?;
            return Ok(false);
        }

        let bank = match self.clinician_repo.get_bank_account(p.clinician_id).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                self.record_failed_payout(
                    p,
                    gross,
                    fee,
                    net,
                    "clinician has no stored bank account",
                )
                .await?;
                return Ok(false);
            }
            Err(e) => {
                return Err(PayoutServiceError::Database(sqlx::Error::Configuration(
                    Box::<dyn std::error::Error + Send + Sync>::from(e.to_string()),
                )))
            }
        };
        let account_number = self
            .encryption
            .decrypt_token(&bank.account_number)
            .map_err(|e| PayoutServiceError::Encryption(e.to_string()))?;

        // Pay the worker FROM the hospital's own funding sub-account (where its
        // deposits sit), not the platform account — the wallet funds the payout.
        let debit_account: Option<String> = sqlx::query_scalar(
            "SELECT safehaven_account_number FROM hospital_wallets WHERE hospital_id = $1",
        )
        .bind(p.hospital_id)
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        let debit_account = match debit_account.filter(|s| !s.trim().is_empty()) {
            Some(a) => a,
            None => {
                self.record_failed_payout(
                    p,
                    gross,
                    fee,
                    net,
                    "hospital wallet has no funding sub-account",
                )
                .await?;
                return Ok(false);
            }
        };

        let mut tx = self.pool.begin().await?;
        let payout_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO billing_transactions (
                hospital_id, event_type, amount_kobo, currency, status,
                provider, shift_id, description
            )
            VALUES ($1, 'payout', $2, 'NGN', 'pending', 'safehaven', $3, $4)
            RETURNING id
            "#,
        )
        .bind(p.hospital_id)
        .bind(net)
        .bind(p.shift_id)
        .bind(format!("Net pay for shift {}", p.role_title))
        .fetch_one(&mut *tx)
        .await?;

        // Debit held by the full gross; fee is recognised separately in
        self.wallet_repo
            .insert_ledger_entry_in_tx(
                &mut tx,
                p.hospital_id,
                "payout_debit",
                0,
                -gross,
                Some(p.shift_id),
                Some(&payout_id.to_string()),
                Some("net + fee debit on payout"),
            )
            .await
            .map_err(|e| match e {
                crate::repositories::wallet::WalletRepoError::Database(db) => {
                    PayoutServiceError::Database(db)
                }
                _ => PayoutServiceError::Database(sqlx::Error::Protocol(
                    "wallet ledger error".to_string(),
                )),
            })?;

        sqlx::query(
            r#"
            UPDATE hospital_wallets
               SET held_kobo  = held_kobo - $2,
                   updated_at = NOW()
             WHERE hospital_id = $1
            "#,
        )
        .bind(p.hospital_id)
        .bind(gross)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            INSERT INTO platform_revenue_ledger (shift_id, hospital_id, gross_kobo, fee_kobo, net_kobo)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (shift_id) DO NOTHING
            "#,
        )
        .bind(p.shift_id)
        .bind(p.hospital_id)
        .bind(gross)
        .bind(fee)
        .bind(net)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        match self
            .safehaven
            .transfer(
                &bank.bank_code,
                &account_number,
                net / 100,
                &format!("NexusCare shift {}", p.shift_id),
                &payout_id.to_string(),
                Some(&debit_account),
            )
            .await
        {
            Ok(receipt) => {
                sqlx::query(
                    r#"
                    UPDATE billing_transactions
                       SET status                  = 'success',
                           provider_reference      = $2,
                           provider_transaction_id = $3,
                           completed_at            = NOW(), updated_at              = NOW()
                     WHERE id = $1
                    "#,
                )
                .bind(payout_id)
                .bind(&receipt.payment_reference)
                .bind(&receipt.session_id)
                .execute(&self.pool)
                .await?;
                tracing::info!(
                    "Payout {} for shift {} -> ₦{} (net) sent to SafeHaven",
                    payout_id,
                    p.shift_id,
                    net / 100
                );
                Ok(true)
            }
            Err(e) => {
                // Transfer rejected synchronously. Reverse the escrow debit so the
                // shift becomes payable again (find_payable_shifts retries failed
                // rows up to 3 times), then mark this attempt failed.
                self.refund_payout(p, payout_id, gross, &e.to_string())
                    .await?;
                tracing::error!(
                    "Payout {} for shift {} failed at SafeHaven (refunded escrow): {}",
                    payout_id,
                    p.shift_id,
                    e
                );
                Ok(false)
            }
        }
    }

    /// Reverse a committed payout debit and mark the billing row failed, in one
    /// tx. Re-credits `held_kobo` by `gross`, writes a `payout_reversal` ledger
    /// entry, and drops the platform-revenue row so a retry re-inserts cleanly.
    async fn refund_payout(
        &self,
        p: &PayableShift,
        payout_id: Uuid,
        gross: i64,
        error: &str,
    ) -> Result<(), PayoutServiceError> {
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
        .bind(payout_id)
        .bind(error)
        .execute(&mut *tx)
        .await?;

        // The original payout debited `held_kobo` (consuming the shift escrow).
        // On failure, return the gross to AVAILABLE balance so it is spendable
        // again — not back into held, which would strand it in escrow.
        self.wallet_repo
            .insert_ledger_entry_in_tx(
                &mut tx,
                p.hospital_id,
                "payout_reversal",
                gross,
                0,
                Some(p.shift_id),
                Some(&payout_id.to_string()),
                Some("escrow re-credited to balance after failed transfer"),
            )
            .await
            .map_err(|e| match e {
                crate::repositories::wallet::WalletRepoError::Database(db) => {
                    PayoutServiceError::Database(db)
                }
                _ => PayoutServiceError::Database(sqlx::Error::Protocol(
                    "wallet ledger error".to_string(),
                )),
            })?;

        sqlx::query(
            r#"
            UPDATE hospital_wallets
               SET balance_kobo = balance_kobo + $2,
                   updated_at   = NOW()
             WHERE hospital_id = $1
            "#,
        )
        .bind(p.hospital_id)
        .bind(gross)
        .execute(&mut *tx)
        .await?;

        sqlx::query("DELETE FROM platform_revenue_ledger WHERE shift_id = $1")
            .bind(p.shift_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn record_failed_payout(
        &self,
        p: &PayableShift,
        _gross: i64,
        _fee: i64,
        net: i64,
        reason: &str,
    ) -> Result<(), PayoutServiceError> {
        sqlx::query(
            r#"
            INSERT INTO billing_transactions
                (hospital_id, event_type, amount_kobo, currency, status,
                 provider, shift_id, description)
            VALUES ($1, 'payout', $2, 'NGN', 'failed', 'safehaven', $3, $4)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(p.hospital_id)
        .bind(net)
        .bind(p.shift_id)
        .bind(format!("Payout failed: {reason}"))
        .execute(&self.pool)
        .await?;
        tracing::warn!("Payout for shift {} not initiated: {}", p.shift_id, reason);
        Ok(())
    }

    /// Settle payouts that are still `pending` at SafeHaven by polling
    /// `transfer_status` for each. Completed → success; Failed/Cancelled →
    /// failed + escrow refund (so the shift can retry). Best-effort per row.
    pub async fn poll_pending_transfers(&self) -> Result<usize, PayoutServiceError> {
        let rows = sqlx::query_as::<_, PendingTransfer>(
            r#"
            SELECT bt.id, bt.shift_id, bt.hospital_id, bt.provider_reference,
                   s.assigned_clinician_id AS clinician_id,
                   s.grand_total_kobo,
                   s.role_title
            FROM billing_transactions bt
            JOIN shifts s ON s.id = bt.shift_id
            WHERE bt.event_type = 'payout'
              AND bt.status = 'pending'
              AND bt.provider_reference IS NOT NULL
            LIMIT 100
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        let mut settled = 0usize;
        for r in rows {
            let reference = match &r.provider_reference {
                Some(x) => x,
                None => continue,
            };
            let status = match self.safehaven.transfer_status(reference).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("transfer_status poll failed for payout {}: {}", r.id, e);
                    continue;
                }
            };
            match status {
                TransferStatus::Completed => {
                    sqlx::query(
                        r#"UPDATE billing_transactions
                              SET status = 'success', completed_at = NOW(), updated_at = NOW()
                            WHERE id = $1"#,
                    )
                    .bind(r.id)
                    .execute(&self.pool)
                    .await?;
                    settled += 1;
                }
                TransferStatus::Failed | TransferStatus::Cancelled => {
                    let p = PayableShift {
                        shift_id: r.shift_id,
                        hospital_id: r.hospital_id,
                        clinician_id: r.clinician_id.unwrap_or_default(),
                        grand_total_kobo: r.grand_total_kobo,
                        role_title: r.role_title.clone(),
                    };
                    let gross = r.grand_total_kobo.unwrap_or(0);
                    let (gross, _fee, _net) = split_payout(gross);
                    self.refund_payout(&p, r.id, gross, "transfer reported failed by SafeHaven")
                        .await?;
                    settled += 1;
                }
                // Created/Initiated/Processing/Unknown → still in flight, leave as pending.
                _ => {}
            }
        }
        Ok(settled)
    }

    /// List a hospital's payout rows (newest first).
    pub async fn list_payouts(
        &self,
        hospital_id: Uuid,
        page: i64,
        page_size: i64,
    ) -> Result<(Vec<PayoutRow>, i64), PayoutServiceError> {
        let page = page.max(1);
        let page_size = page_size.clamp(1, 200);
        let offset = (page - 1) * page_size;

        let rows = sqlx::query_as::<_, PayoutRow>(
            r#"
            SELECT id, shift_id, amount_kobo, status::text AS status,
                   provider_reference, provider_transaction_id,
                   description, created_at, completed_at
            FROM billing_transactions
            WHERE hospital_id = $1 AND event_type = 'payout'
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
            "SELECT COUNT(*) FROM billing_transactions WHERE hospital_id = $1 AND event_type = 'payout'",
        )
        .bind(hospital_id)
        .fetch_one(&self.pool)
        .await?;

        Ok((rows, total))
    }

    /// Refresh a single payout's status from SafeHaven if it is still pending,
    /// settling it (success, or failed + refund) as appropriate. Returns the
    /// current stored status string.
    pub async fn refresh_payout_status(
        &self,
        payout_id: Uuid,
    ) -> Result<String, PayoutServiceError> {
        let row = sqlx::query_as::<_, PendingTransfer>(
            r#"
            SELECT bt.id, bt.shift_id, bt.hospital_id, bt.provider_reference,
                   s.assigned_clinician_id AS clinician_id,
                   s.grand_total_kobo, s.role_title
            FROM billing_transactions bt
            JOIN shifts s ON s.id = bt.shift_id
            WHERE bt.id = $1 AND bt.event_type = 'payout'
            "#,
        )
        .bind(payout_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(r) = row else {
            return Ok("not_found".to_string());
        };
        let Some(reference) = r.provider_reference.clone() else {
            // No transfer was ever sent (e.g. failed pre-flight). Return stored status.
            let st: String =
                sqlx::query_scalar("SELECT status::text FROM billing_transactions WHERE id = $1")
                    .bind(payout_id)
                    .fetch_one(&self.pool)
                    .await?;
            return Ok(st);
        };

        match self.safehaven.transfer_status(&reference).await? {
            TransferStatus::Completed => {
                sqlx::query(
                    r#"UPDATE billing_transactions
                          SET status = 'success', completed_at = NOW(), updated_at = NOW()
                        WHERE id = $1 AND status = 'pending'"#,
                )
                .bind(payout_id)
                .execute(&self.pool)
                .await?;
                Ok("success".to_string())
            }
            TransferStatus::Failed | TransferStatus::Cancelled => {
                let p = PayableShift {
                    shift_id: r.shift_id,
                    hospital_id: r.hospital_id,
                    clinician_id: r.clinician_id.unwrap_or_default(),
                    grand_total_kobo: r.grand_total_kobo,
                    role_title: r.role_title.clone(),
                };
                let (gross, _f, _n) = split_payout(r.grand_total_kobo.unwrap_or(0));
                self.refund_payout(
                    &p,
                    payout_id,
                    gross,
                    "transfer reported failed by SafeHaven",
                )
                .await?;
                Ok("failed".to_string())
            }
            _ => Ok("processing".to_string()),
        }
    }

    /// Manually reprocess a shift's payout (SuperAdmin override). Only valid if
    /// the shift currently has no in-flight/successful payout. Returns whether a
    /// transfer was initiated.
    pub async fn retry_payout(&self, shift_id: Uuid) -> Result<bool, PayoutServiceError> {
        let candidate = sqlx::query_as::<_, PayableShift>(
            r#"
            SELECT s.id AS shift_id, s.hospital_id,
                   s.assigned_clinician_id AS clinician_id,
                   s.grand_total_kobo, s.role_title
            FROM shifts s
            JOIN shift_handovers h ON h.shift_id = s.id
            WHERE s.id = $1
              AND s.status = 'completed'
              AND s.assigned_clinician_id IS NOT NULL
              AND s.grand_total_kobo IS NOT NULL
              AND h.hospital_approved_at IS NOT NULL
              AND NOT EXISTS (
                  SELECT 1 FROM billing_transactions bt
                  WHERE bt.shift_id = s.id AND bt.event_type = 'payout'
                    AND bt.status <> 'failed'
              )
            "#,
        )
        .bind(shift_id)
        .fetch_optional(&self.pool)
        .await?;

        match candidate {
            Some(p) => self.process_one(&p).await,
            None => Ok(false),
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct PendingTransfer {
    id: Uuid,
    shift_id: Uuid,
    hospital_id: Uuid,
    provider_reference: Option<String>,
    clinician_id: Option<Uuid>,
    grand_total_kobo: Option<i64>,
    role_title: String,
}

/// A payout row surfaced to the wallet API.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, utoipa::ToSchema)]
pub struct PayoutRow {
    pub id: Uuid,
    pub shift_id: Option<Uuid>,
    pub amount_kobo: i64,
    pub status: String,
    pub provider_reference: Option<String>,
    pub provider_transaction_id: Option<String>,
    pub description: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ₦69,664 gross → ₦62,697.60 net after 10% fee.
    #[test]
    fn frs_example_payout_split() {
        let gross_kobo = 6_966_400_i64;
        let (gross, fee, net) = split_payout(gross_kobo);
        assert_eq!(gross, 6_966_400);
        assert_eq!(fee, 696_640);
        assert_eq!(net, 6_269_760);
        assert_eq!(gross, fee + net);
    }

    #[test]
    fn zero_gross_yields_zero_fee_and_net() {
        let (g, f, n) = split_payout(0);
        assert_eq!((g, f, n), (0, 0, 0));
    }

    #[test]
    fn integer_truncation_keeps_invariants() {
        let (g, f, n) = split_payout(9);
        assert_eq!(g, 9);
        assert_eq!(f, 0);
        assert_eq!(n, 9);
        assert_eq!(g, f + n);
    }
}
