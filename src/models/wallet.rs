// ! Hospital wallet + ledger DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use utoipa::ToSchema;
use uuid::Uuid;
use validator::{Validate, ValidationError};

use crate::utils::money::Naira;

/// Mirror of the `hospital_wallets` row.
///
/// Kobo, deliberately: this is the ledger side of the boundary. Every balance
/// and hold is arithmetic, and an exact minor-unit integer is the only form
/// that cannot drift. [`WalletSummary`] is the naira form that goes on the
/// wire.
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
    #[sqlx(rename = "delta_balance_kobo")]
    pub delta_balance_naira: Naira,
    #[sqlx(rename = "delta_held_kobo")]
    pub delta_held_naira: Naira,
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
    #[sqlx(rename = "amount_kobo")]
    pub amount_naira: Naira,
    pub virtual_account_number: String,
    pub virtual_bank_code: Option<String>,
    pub virtual_account_name: Option<String>,
    pub valid_until: DateTime<Utc>,
    pub external_reference: String,
    pub status: String,
    pub received_at: Option<DateTime<Utc>>,
    #[sqlx(rename = "received_amount_kobo")]
    pub received_amount_naira: Option<Naira>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// Request / response DTOs

/// Body for `POST /api/v1/wallet/deposits`.
#[derive(Debug, Clone, Deserialize, Validate, ToSchema)]
pub struct CreateDepositRequest {
    /// Amount in **naira** (e.g. 100000 = ₦100,000). Minimum ₦1,000.
    ///
    /// Validated by a custom function rather than `range`: the stored value is
    /// kobo, so a `range` bound here would be read as a naira bound by anyone
    /// editing it and as a kobo bound by `validator`. Comparing kobo to kobo
    /// leaves nothing to misread.
    #[validate(custom(function = "min_deposit"))]
    pub amount_naira: Naira,
}

/// ₦1,000, the smallest deposit SafeHaven is worth a virtual account for.
fn min_deposit(amount: &Naira) -> Result<(), ValidationError> {
    if amount.kobo() < 100_000 {
        return Err(
            ValidationError::new("min_deposit").with_message("Minimum deposit is ₦1,000".into())
        );
    }
    Ok(())
}

/// Response for `POST /api/v1/wallet/deposits` and `GET /api/v1/wallet/deposits`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct DepositResponse {
    pub deposit_id: Uuid,
    pub amount_naira: Naira,
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
            amount_naira: r.amount_naira,
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
    pub amount_naira: Naira,
    pub instructions: String,
}

/// Body for `POST /api/v1/wallet/withdraw`.
#[derive(Debug, Clone, Deserialize, Validate, ToSchema)]
pub struct WithdrawRequest {
    /// Amount in **naira** to withdraw (e.g. 100 = ₦100). Minimum ₦100.
    #[validate(custom(function = "min_withdrawal"))]
    pub amount_naira: Naira,
    /// Destination NUBAN account number (10 digits).
    #[validate(length(min = 10, max = 10, message = "account_number must be 10 digits"))]
    pub account_number: String,
    /// Destination bank code (from the SafeHaven bank list).
    #[validate(length(min = 3, message = "bank_code is required"))]
    pub bank_code: String,
    /// Optional narration shown on the transfer.
    pub narration: Option<String>,
}

/// ₦100, the smallest transfer worth the provider's fee.
fn min_withdrawal(amount: &Naira) -> Result<(), ValidationError> {
    if amount.kobo() < 10_000 {
        return Err(
            ValidationError::new("min_withdrawal").with_message("Minimum withdrawal is ₦100".into())
        );
    }
    Ok(())
}

/// Response for `POST /api/v1/wallet/withdraw`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct WithdrawResponse {
    pub withdrawal_id: Uuid,
    /// Net amount sent to the beneficiary account.
    pub amount_naira: Naira,
    /// SafeHaven transfer fee charged on top of `amount_naira` (also debited).
    pub fee_naira: Naira,
    pub account_number: String,
    /// Resolved account holder name (from name-enquiry).
    pub account_name: String,
    pub bank_code: String,
    /// "success" | "pending" | "failed".
    pub status: String,
    /// Provider payment reference (equals the withdrawal id).
    pub reference: String,
    pub message: String,
}

/// A withdrawal (`billing_transactions` row, `event_type = 'withdrawal'`).
#[derive(Debug, Clone, FromRow, Serialize, ToSchema)]
pub struct WithdrawalRow {
    pub id: Uuid,
    #[sqlx(rename = "amount_kobo")]
    pub amount_naira: Naira,
    pub status: String,
    pub provider_reference: Option<String>,
    pub provider_transaction_id: Option<String>,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// Response for `GET /api/v1/wallet`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct WalletSummary {
    pub balance_naira: Naira,
    pub held_naira: Naira,
    /// Sum of `balance_naira + held_naira` — total funds at SafeHaven we're
    pub total_naira: Naira,
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
            balance_naira: Naira::from_kobo(w.balance_kobo),
            held_naira: Naira::from_kobo(w.held_kobo),
            total_naira: Naira::from_kobo(w.balance_kobo + w.held_kobo),
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
