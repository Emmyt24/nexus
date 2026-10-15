use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::utils::money::Naira;

// Enums

/// Lifecycle state of a billing transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "transaction_status", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum TransactionStatus {
    /// Initiated, awaiting upstream confirmation (SafeHaven webhook).
    Pending,
    /// Confirmed successful by the provider.
    Success,
    /// Declined or otherwise rejected by the provider.
    Failed,
    /// Reversed / refunded.
    Reversed,
}

/// What the charge was for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "billing_event_type", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum BillingEventType {
    /// Per-shift clinician fee (legacy direct-charge label retained for any
    ShiftFee,
    /// Hospital deposit credited to the wallet via SafeHaven.
    Deposit,
    /// Net pay disbursed to a clinician after handover approval.
    Payout,
    /// Platform fee (10% of gross) deducted from the held escrow.
    PlatformFee,
    /// Refund (e.g. shift cancelled with funds previously held).
    Refund,
}

// Billing transactions

/// A single billing row on a hospital's account. With the SafeHaven cutover
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct BillingTransaction {
    pub id: Uuid,
    pub hospital_id: Uuid,

    pub event_type: BillingEventType,
    /// Kobo, as the column stores it (e.g. 500000 = ₦5,000). The wire form is
    /// [`BillingTransactionResponse::amount_naira`].
    pub amount_kobo: i64,
    /// ISO 4217 currency code, e.g. "NGN".
    pub currency: String,

    pub status: TransactionStatus,

    /// Which provider issued the transaction (e.g. "safehaven").
    pub provider: String,
    /// Provider-side reference, e.g. SafeHaven payment_reference or sessionId.
    pub provider_reference: Option<String>,
    /// Provider-side transaction id (returned after the call succeeds).
    pub provider_transaction_id: Option<String>,

    /// The shift this row relates to, when applicable.
    pub shift_id: Option<Uuid>,
    pub description: Option<String>,

    pub initiated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

// Response types

/// The wire form of a [`BillingTransaction`]. Naira, like every amount this API
/// returns; the row it comes from keeps kobo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingTransactionResponse {
    pub id: Uuid,
    pub hospital_id: Uuid,
    pub event_type: BillingEventType,
    pub amount_naira: Naira,
    pub currency: String,
    pub status: TransactionStatus,
    pub description: Option<String>,
    pub shift_id: Option<Uuid>,
    pub provider: String,
    pub provider_reference: Option<String>,
    pub initiated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl From<BillingTransaction> for BillingTransactionResponse {
    fn from(t: BillingTransaction) -> Self {
        Self {
            id: t.id,
            hospital_id: t.hospital_id,
            event_type: t.event_type,
            amount_naira: Naira::from_kobo(t.amount_kobo),
            currency: t.currency,
            status: t.status,
            description: t.description,
            shift_id: t.shift_id,
            provider: t.provider,
            provider_reference: t.provider_reference,
            initiated_at: t.initiated_at,
            completed_at: t.completed_at,
        }
    }
}
