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

/// Default minimum net payout (₦100). Overridable at runtime with the
/// `MIN_PAYOUT_KOBO` env var (kobo). TEMPORARY: lowered from ₦5,000 to ₦100 to
/// match the ₦100 shift-rate floor during live testing; restore to 500_000 after.
pub const DEFAULT_MIN_PAYOUT_KOBO: i64 = 10_000;

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

/// How the platform fee is derived. Flat is the primary mode; percent is the
/// legacy one, kept because shifts created before the flat rollout settle on it.
///
/// Stored as TEXT in `platform_settings.platform_fee_mode`, not a Postgres enum
/// — see `migrations/20240064_platform_fee_mode.sql`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformFeeMode {
    Flat,
    Percent,
}

impl PlatformFeeMode {
    /// Anything unrecognised reads as `Percent`, the pre-flat behaviour: a
    /// garbled settings row must not silently drop the fee to a flat ₦0.
    pub fn from_db(s: &str) -> Self {
        match s {
            "flat" => Self::Flat,
            _ => Self::Percent,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Flat => "flat",
            Self::Percent => "percent",
        }
    }
}

/// The admin-configured fee, as stored. `flat_naira` is naira; everything else
/// in this module is kobo.
#[derive(Debug, Clone, Copy)]
pub struct PlatformFeeConfig {
    pub mode: PlatformFeeMode,
    pub flat_naira: i64,
    pub percent: f64,
    pub cap_kobo: Option<i64>,
}

impl PlatformFeeConfig {
    /// The pre-flat default: 10% of gross, uncapped. Used only when the settings
    /// row cannot be read.
    pub fn legacy_percent_default() -> Self {
        Self {
            mode: PlatformFeeMode::Percent,
            flat_naira: 0,
            percent: (PLATFORM_FEE_NUMERATOR as f64 / PLATFORM_FEE_DENOMINATOR as f64) * 100.0,
            cap_kobo: None,
        }
    }
}

/// The platform's cut of `gross_kobo`, in kobo.
///
/// This is the **only** place naira becomes kobo. `flat_naira * 100` is exact,
/// and the result is clamped into `[0, gross]` so a flat fee larger than a small
/// shift can never produce a negative net — it takes the whole gross instead.
pub fn compute_platform_fee(gross_kobo: i64, cfg: &PlatformFeeConfig) -> i64 {
    let fee = match cfg.mode {
        PlatformFeeMode::Flat => cfg.flat_naira.saturating_mul(100),
        PlatformFeeMode::Percent => {
            let pct = cfg.percent.clamp(0.0, 100.0);
            let mut fee = ((gross_kobo as f64) * pct / 100.0).round() as i64;
            if let Some(cap) = cfg.cap_kobo {
                fee = fee.min(cap.max(0));
            }
            fee
        }
    };
    fee.clamp(0, gross_kobo.max(0))
}

/// `(gross, fee, net)` for a configured fee, flat or percent.
pub fn split_payout_config(gross_kobo: i64, cfg: &PlatformFeeConfig) -> (i64, i64, i64) {
    let fee = compute_platform_fee(gross_kobo, cfg);
    (gross_kobo, fee, gross_kobo - fee)
}

/// Read the admin-configured platform fee from `platform_settings`, failing if
/// it cannot be read.
///
/// Shared by the payout pipeline and by shift creation/preview so there is one
/// definition of the fee, not two that can drift.
///
/// Use this wherever the answer is *recorded* — above all `create_shift`, which
/// snapshots the result onto `shifts.platform_fee_kobo` permanently. A single
/// statement timeout there would otherwise bake a guessed fee into a shift
/// forever: with a configured flat fee of ₦500, a lenient fallback to the legacy
/// 10% locks ₦10,000 out of a ₦100,000 shift's payout, silently and
/// irreversibly. Failing the request is recoverable; a wrong snapshot is not.
pub async fn try_read_platform_fee_config(
    pool: &PgPool,
) -> Result<PlatformFeeConfig, sqlx::Error> {
    let row: Option<(String, i64, f64, Option<i64>)> = sqlx::query_as(
        "SELECT platform_fee_mode, platform_fee_flat_naira, \
         platform_fee_percent::DOUBLE PRECISION, platform_fee_cap_kobo \
         FROM platform_settings WHERE singleton = 'global'",
    )
    .fetch_optional(pool)
    .await?;

    match row {
        Some((mode, flat_naira, percent, cap_kobo)) => Ok(PlatformFeeConfig {
            mode: PlatformFeeMode::from_db(&mode),
            flat_naira,
            percent,
            cap_kobo,
        }),
        // The row is seeded by 20240048 and the primary key pins it to one row,
        // so its absence is a broken database, not a state to paper over.
        None => Err(sqlx::Error::RowNotFound),
    }
}

/// As [`try_read_platform_fee_config`], but falls back to the legacy 10% on any
/// failure.
///
/// Only safe where the result is *used once and not stored* — that is, the
/// payout pipeline, where a bad tick is retried on the next scheduler pass and
/// charging the historical 10% is better than charging nothing. Do not use this
/// on a path that writes the fee down.
pub async fn read_platform_fee_config(pool: &PgPool) -> PlatformFeeConfig {
    match try_read_platform_fee_config(pool).await {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::error!(
                "failed to read platform fee settings, falling back to the legacy 10% fee \
                 for this payout only: {e}"
            );
            PlatformFeeConfig::legacy_percent_default()
        }
    }
}

/// `(gross, fee, net)` for a percent + optional cap. Retained for callers that
/// only know a percentage; new code should use [`split_payout_config`].
pub fn split_payout_with(gross_kobo: i64, fee_percent: f64, cap_kobo: Option<i64>) -> (i64, i64, i64) {
    split_payout_config(
        gross_kobo,
        &PlatformFeeConfig {
            mode: PlatformFeeMode::Percent,
            flat_naira: 0,
            percent: fee_percent,
            cap_kobo,
        },
    )
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
    /// Fee agreed when the shift was created. NULL on shifts predating the
    /// snapshot column, which settle on the live settings instead.
    platform_fee_kobo: Option<i64>,
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
                   s.role_title,
                   s.platform_fee_kobo
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

    /// See [`read_platform_fee_config`].
    pub async fn platform_fee_config(&self) -> PlatformFeeConfig {
        read_platform_fee_config(&self.pool).await
    }

    async fn process_one(&self, p: &PayableShift) -> Result<bool, PayoutServiceError> {
        let gross = p.grand_total_kobo.unwrap_or(0);
        // Prefer the fee snapshotted when the shift was created — that is the
        // number the hospital was shown in the preview. Only fall back to the
        // live settings for shifts created before the snapshot column existed.
        let (gross, fee, net) = match p.platform_fee_kobo {
            Some(snapshot) => {
                let fee = snapshot.clamp(0, gross.max(0));
                (gross, fee, gross - fee)
            }
            None => split_payout_config(gross, &self.platform_fee_config().await),
        };

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
                        // Refunding reverses the whole gross hold; the fee split
                        // is irrelevant here, so it is not carried.
                        platform_fee_kobo: None,
                    };
                    // A refund releases the full gross, not the net. This used to
                    // round-trip through `split_payout` to recover `gross`, which
                    // read as if the 10% model mattered here. It does not.
                    let gross = r.grand_total_kobo.unwrap_or(0);
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
                    platform_fee_kobo: None,
                };
                // As above: the refund reverses the gross hold.
                let gross = r.grand_total_kobo.unwrap_or(0);
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
                   s.grand_total_kobo, s.role_title, s.platform_fee_kobo
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

    #[test]
    fn configurable_fee_percent_and_cap() {
        // 10% of ₦10,000 = ₦1,000 fee, no cap.
        assert_eq!(split_payout_with(1_000_000, 10.0, None), (1_000_000, 100_000, 900_000));
        // Cap below the computed fee wins.
        assert_eq!(split_payout_with(1_000_000, 10.0, Some(50_000)), (1_000_000, 50_000, 950_000));
        // Cap above the computed fee is a no-op.
        assert_eq!(split_payout_with(1_000_000, 10.0, Some(500_000)), (1_000_000, 100_000, 900_000));
        // 0% fee.
        assert_eq!(split_payout_with(1_000_000, 0.0, None), (1_000_000, 0, 1_000_000));
        // Invariant: gross == fee + net for arbitrary inputs.
        let (g, f, n) = split_payout_with(777_777, 7.5, Some(40_000));
        assert_eq!(g, f + n);
    }

    fn flat(naira: i64) -> PlatformFeeConfig {
        PlatformFeeConfig {
            mode: PlatformFeeMode::Flat,
            flat_naira: naira,
            percent: 10.0,
            cap_kobo: None,
        }
    }

    /// The one that matters. A flat fee is configured in naira and charged in
    /// kobo; if the `* 100` is ever dropped or doubled this is the test that
    /// says so, and nothing else in the suite would.
    #[test]
    fn flat_fee_in_naira_is_charged_in_kobo() {
        // ₦500 flat on a ₦10,000 gross → ₦500 fee, ₦9,500 net.
        let (gross, fee, net) = split_payout_config(1_000_000, &flat(500));
        assert_eq!(gross, 1_000_000);
        assert_eq!(fee, 50_000, "a ₦500 flat fee is 50_000 kobo, not 500");
        assert_eq!(net, 950_000);
        assert_eq!(gross, fee + net);
    }

    #[test]
    fn flat_fee_is_independent_of_gross() {
        // Unlike percent, the same flat fee applies whatever the shift is worth.
        assert_eq!(compute_platform_fee(1_000_000, &flat(500)), 50_000);
        assert_eq!(compute_platform_fee(9_000_000, &flat(500)), 50_000);
    }

    #[test]
    fn flat_fee_larger_than_gross_takes_the_gross_not_more() {
        // ₦500 flat against a ₦100 shift: the fee cannot exceed the gross, or
        // net goes negative and the clinician owes the platform money.
        let (gross, fee, net) = split_payout_config(10_000, &flat(500));
        assert_eq!(gross, 10_000);
        assert_eq!(fee, 10_000);
        assert_eq!(net, 0);
        assert!(net >= 0, "net must never be negative");
    }

    #[test]
    fn flat_zero_takes_nothing() {
        // A promotional ₦0 is legitimate and must not error or misbehave.
        assert_eq!(split_payout_config(1_000_000, &flat(0)), (1_000_000, 0, 1_000_000));
    }

    #[test]
    fn flat_mode_ignores_percent_and_cap() {
        let cfg = PlatformFeeConfig {
            mode: PlatformFeeMode::Flat,
            flat_naira: 500,
            percent: 90.0,
            cap_kobo: Some(1),
        };
        // Neither the 90% nor the ₦0.01 cap applies in flat mode.
        assert_eq!(compute_platform_fee(1_000_000, &cfg), 50_000);
    }

    #[test]
    fn percent_mode_is_unchanged_by_the_flat_rollout() {
        let cfg = PlatformFeeConfig {
            mode: PlatformFeeMode::Percent,
            flat_naira: 999_999,
            percent: 10.0,
            cap_kobo: None,
        };
        // The flat amount is ignored, and the result still matches the old path.
        assert_eq!(compute_platform_fee(6_966_400, &cfg), 696_640);
        assert_eq!(split_payout_config(6_966_400, &cfg), split_payout(6_966_400));
    }

    #[test]
    fn unknown_stored_mode_falls_back_to_percent_not_free() {
        // A garbled settings value must not read as flat ₦0 and silently stop
        // charging. Percent is the safe interpretation.
        assert_eq!(PlatformFeeMode::from_db("flat"), PlatformFeeMode::Flat);
        assert_eq!(PlatformFeeMode::from_db("percent"), PlatformFeeMode::Percent);
        assert_eq!(PlatformFeeMode::from_db(""), PlatformFeeMode::Percent);
        assert_eq!(PlatformFeeMode::from_db("FLAT"), PlatformFeeMode::Percent);
        assert_eq!(PlatformFeeMode::from_db("gibberish"), PlatformFeeMode::Percent);
    }

    #[test]
    fn mode_round_trips_through_its_string_form() {
        for mode in [PlatformFeeMode::Flat, PlatformFeeMode::Percent] {
            assert_eq!(PlatformFeeMode::from_db(mode.as_str()), mode);
        }
    }

    #[test]
    fn absurd_flat_fee_cannot_overflow() {
        // saturating_mul, then clamped to gross.
        assert_eq!(compute_platform_fee(1_000_000, &flat(i64::MAX)), 1_000_000);
    }

    #[test]
    fn negative_gross_yields_no_fee() {
        // Defensive: grand_total_kobo is CHECK (>= 0), but the helper is public.
        assert_eq!(compute_platform_fee(-5, &flat(500)), 0);
    }
}
