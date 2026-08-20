// ! Hospital wallet + ledger DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;
use validator::Validate;

/// Mirror of the `hospital_wallets` row.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow, ToSchema)]
pub struct Wallet {
    pub hospital_id: Uuid,
    pub safehaven_account_id: Option<String>,
    pub safehaven_account_number: Option<String>,
    pub safehaven_bank_code: Option<String>,
    pub safehaven_account_name: Option<String>,
    /// Unencumbered funds available for new shift escrows.
    pub balance_kobo: i64,
    /// Funds reserved for active shifts (escrow).
    pub held_kobo: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Mirror of a `wallet_ledger_entries` row.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow, ToSchema)]
pub struct WalletLedgerEntry {
    pub id: Uuid,
    pub hospital_id: Uuid,
    pub kind: String,
    pub delta_balance_kobo: i64,
    pub delta_held_kobo: i64,
    pub shift_id: Option<Uuid>,
    pub provider_reference: Option<String>,
    pub notes: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Mirror of a `wallet_deposit_requests` row.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow, ToSchema)]
pub struct WalletDepositRequest {
    pub id: Uuid,
    pub hospital_id: Uuid,
    pub amount_kobo: i64,
    pub virtual_account_number: String,
    pub virtual_bank_code: Option<String>,
    pub virtual_account_name: Option<String>,
    pub valid_until: DateTime<Utc>,
    pub external_reference: String,
    pub status: String,
    pub received_at: Option<DateTime<Utc>>,
    pub received_amount_kobo: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// Request / response DTOs

/// Body for `POST /api/v1/wallet/deposits`.
#[derive(Debug, Clone, Deserialize, Validate, ToSchema)]
pub struct CreateDepositRequest {
    /// Amount in kobo (e.g. 10_000_000 = ₦100,000). Minimum ₦1,000.
    #[validate(range(min = 100_000, message = "Minimum deposit is ₦1,000"))]
    pub amount_kobo: i64,
}

/// Response for `POST /api/v1/wallet/deposits` and `GET /api/v1/wallet/deposits`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DepositResponse {
    pub deposit_id: Uuid,
    pub amount_kobo: i64,
    pub virtual_account_number: String,
    pub virtual_bank_code: Option<String>,
    pub virtual_account_name: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub status: String,
}

impl From<WalletDepositRequest> for DepositResponse {
    fn from(r: WalletDepositRequest) -> Self {
        Self {
            deposit_id: r.id,
            amount_kobo: r.amount_kobo,
            virtual_account_number: r.virtual_account_number,
            virtual_bank_code: r.virtual_bank_code,
            virtual_account_name: r.virtual_account_name,
            expires_at: r.valid_until,
            status: r.status,
        }
    }
}

/// Response for `POST /api/v1/wallet/deposits` — the hospital funds its wallet
/// by transferring into its dedicated SafeHaven sub-account.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DepositInstructions {
    /// Account number to transfer into.
    pub account_number: String,
    /// Human-readable bank name for display.
    pub bank_name: String,
    /// SafeHaven bank code.
    pub bank_code: Option<String>,
    /// Registered account name.
    pub account_name: Option<String>,
    /// The amount the hospital intends to deposit (echoed; informational —
    /// crediting is based on whatever is actually received).
    pub amount_kobo: i64,
    pub instructions: String,
}

/// Response for `GET /api/v1/wallet`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct WalletSummary {
    pub balance_kobo: i64,
    pub held_kobo: i64,
    /// Sum of `balance_kobo + held_kobo` — total funds at SafeHaven we're
    pub total_kobo: i64,
    pub safehaven_account_number: Option<String>,
    pub safehaven_bank_code: Option<String>,
    pub safehaven_account_name: Option<String>,
    /// Human-readable bank name (for display) when a sub-account exists.
    pub bank_name: Option<String>,
    /// Whether a SafeHaven sub-account has been provisioned (drives the
    /// "Create Wallet" vs "show account details" state on the dashboard).
    pub has_sub_account: bool,
}

impl From<&Wallet> for WalletSummary {
    fn from(w: &Wallet) -> Self {
        let has_sub_account = w
            .safehaven_account_number
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        Self {
            balance_kobo: w.balance_kobo,
            held_kobo: w.held_kobo,
            total_kobo: w.balance_kobo + w.held_kobo,
            safehaven_account_number: w.safehaven_account_number.clone(),
            safehaven_bank_code: w.safehaven_bank_code.clone(),
            safehaven_account_name: w.safehaven_account_name.clone(),
            bank_name: if has_sub_account {
                Some("Safe Haven MFB".to_string())
            } else {
                None
            },
            has_sub_account,
        }
    }
}
