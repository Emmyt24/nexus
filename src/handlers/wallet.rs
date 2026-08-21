// ! Hospital wallet endpoints — gated to HospitalAdmin/SuperAdmin.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::Deserialize;
use utoipa::IntoParams;
use uuid::Uuid;
use validator::Validate;

use crate::models::wallet::{
    CreateDepositRequest, DepositInstructions, DepositResponse, WalletLedgerEntry, WalletSummary,
    WithdrawRequest, WithdrawResponse, WithdrawalRow,
};
use crate::routes::AppState;
use crate::services::wallet_service::{ReconcileResult, WalletServiceError};
use crate::utils::{
    errors::{AppError, AppResult},
    extract_claims,
};

fn hospital_id_from_claims(headers: &HeaderMap) -> Result<Uuid, AppError> {
    let claims = extract_claims(headers)?;
    claims
        .hospital_id
        .as_deref()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| AppError::Forbidden("No hospital associated with this account".to_string()))
}

fn map_wallet_error(e: WalletServiceError) -> AppError {
    match e {
        WalletServiceError::Validation(msg) => AppError::Validation(msg),
        WalletServiceError::WalletNotFound(_) => AppError::NotFound("Wallet not found".to_string()),
        WalletServiceError::Database(e) => AppError::Database(e),
        WalletServiceError::SafeHaven(e) => {
            AppError::Conflict(format!("Payment provider error: {e}"))
        }
        WalletServiceError::Repo(e) => match e {
            crate::repositories::wallet::WalletRepoError::InsufficientBalance {
                required,
                available,
            } => AppError::Conflict(format!(
                "Insufficient wallet balance: required {required} kobo, available {available} kobo"
            )),
            crate::repositories::wallet::WalletRepoError::NothingToRelease(_) => {
                AppError::Conflict("No held funds to release".to_string())
            }
            crate::repositories::wallet::WalletRepoError::Database(e) => AppError::Database(e),
        },
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/wallet",
    responses(
        (status = 200, description = "Wallet summary", body = WalletSummary),
        (status = 401, description = "Missing or invalid token", body = ErrorResponse),
        (status = 403, description = "No hospital associated with this account", body = ErrorResponse)
    ),
    tag = "wallet",
    summary = "Get the caller hospital's wallet summary",
    description = "Returns balance + held kobo plus the SafeHaven sub-account details (if provisioned)."
)]
pub async fn get_wallet(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<WalletSummary>> {
    let hospital_id = hospital_id_from_claims(&headers)?;
    let w = state
        .wallet_service
        .get_wallet(hospital_id)
        .await
        .map_err(map_wallet_error)?;
    Ok(Json((&w).into()))
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
pub struct LedgerQuery {
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct LedgerPage {
    pub entries: Vec<WalletLedgerEntry>,
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
}

#[utoipa::path(
    get,
    path = "/api/v1/wallet/ledger",
    params(LedgerQuery),
    responses(
        (status = 200, description = "Paginated ledger entries", body = LedgerPage),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse)
    ),
    tag = "wallet",
    summary = "Paginated wallet ledger (audit trail)",
    description = "Newest-first list of every wallet mutation: deposit credits, shift holds, releases, payouts, fees, refunds."
)]
pub async fn get_ledger(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LedgerQuery>,
) -> AppResult<Json<LedgerPage>> {
    let hospital_id = hospital_id_from_claims(&headers)?;
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(50).clamp(1, 200);
    let (entries, total) = state
        .wallet_service
        .list_ledger(hospital_id, page, page_size)
        .await
        .map_err(map_wallet_error)?;
    Ok(Json(LedgerPage {
        entries,
        total,
        page,
        page_size,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/wallet/deposits",
    request_body = CreateDepositRequest,
    responses(
        (status = 200, description = "Sub-account funding instructions", body = DepositInstructions),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 422, description = "No wallet yet / validation error", body = ErrorResponse)
    ),
    tag = "wallet",
    summary = "Get wallet funding instructions",
    description = "Returns the hospital's dedicated SafeHaven sub-account to transfer into. The wallet is credited automatically when SafeHaven fires the inbound credit webhook. (Create the wallet first via /wallet/sub-account/*.)"
)]
pub async fn create_deposit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateDepositRequest>,
) -> AppResult<(StatusCode, Json<DepositInstructions>)> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;
    let hospital_id = hospital_id_from_claims(&headers)?;
    let instructions = state
        .wallet_service
        .deposit_instructions(hospital_id, payload.amount_kobo)
        .await
        .map_err(map_wallet_error)?;
    Ok((StatusCode::OK, Json(instructions)))
}

#[utoipa::path(
    post,
    path = "/api/v1/wallet/reconcile",
    responses(
        (status = 200, description = "Reconcile result", body = ReconcileResult),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 422, description = "No wallet yet", body = ErrorResponse)
    ),
    tag = "wallet",
    summary = "Reconcile wallet deposits against SafeHaven",
    description = "Pulls the hospital sub-account's transaction history from SafeHaven and credits any inbound transfer missed by a webhook (e.g. delivered to a stale callback). Idempotent."
)]
pub async fn reconcile_deposits(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<ReconcileResult>> {
    let hospital_id = hospital_id_from_claims(&headers)?;
    let result = state
        .wallet_service
        .reconcile_deposits(hospital_id)
        .await
        .map_err(map_wallet_error)?;
    Ok(Json(result))
}

#[derive(Debug, Clone, Deserialize, IntoParams)]
pub struct DepositsQuery {
    pub limit: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/api/v1/wallet/deposits",
    params(DepositsQuery),
    responses(
        (status = 200, description = "Recent deposit requests", body = Vec<DepositResponse>),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse)
    ),
    tag = "wallet",
    summary = "List recent deposit requests"
)]
pub async fn list_deposits(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DepositsQuery>,
) -> AppResult<Json<Vec<DepositResponse>>> {
    let hospital_id = hospital_id_from_claims(&headers)?;
    let limit = q.limit.unwrap_or(25).clamp(1, 100);
    let rows = state
        .wallet_service
        .list_deposits(hospital_id, limit)
        .await
        .map_err(map_wallet_error)?;
    Ok(Json(rows.into_iter().map(DepositResponse::from).collect()))
}

#[utoipa::path(
    post,
    path = "/api/v1/wallet/withdraw",
    request_body = WithdrawRequest,
    responses(
        (status = 200, description = "Withdrawal initiated", body = WithdrawResponse),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 409, description = "Insufficient balance or provider rejection", body = ErrorResponse),
        (status = 422, description = "No wallet yet / validation error", body = ErrorResponse)
    ),
    tag = "wallet",
    summary = "Withdraw wallet funds to a bank account",
    description = "Debits available wallet balance and transfers it from the hospital's SafeHaven sub-account to the given bank account. The destination is validated via name-enquiry; a synchronous provider rejection refunds the balance."
)]
pub async fn withdraw(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<WithdrawRequest>,
) -> AppResult<Json<WithdrawResponse>> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;
    let hospital_id = hospital_id_from_claims(&headers)?;
    let resp = state
        .wallet_service
        .withdraw(
            hospital_id,
            payload.amount_kobo,
            &payload.bank_code,
            &payload.account_number,
            payload.narration.as_deref(),
        )
        .await
        .map_err(map_wallet_error)?;
    Ok(Json(resp))
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct WithdrawalPage {
    pub withdrawals: Vec<WithdrawalRow>,
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
}

#[utoipa::path(
    get,
    path = "/api/v1/wallet/withdrawals",
    params(LedgerQuery),
    responses(
        (status = 200, description = "Paginated withdrawal history", body = WithdrawalPage),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse)
    ),
    tag = "wallet",
    summary = "List this hospital's withdrawals"
)]
pub async fn list_withdrawals(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LedgerQuery>,
) -> AppResult<Json<WithdrawalPage>> {
    let hospital_id = hospital_id_from_claims(&headers)?;
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(50).clamp(1, 200);
    let (withdrawals, total) = state
        .wallet_service
        .list_withdrawals(hospital_id, page, page_size)
        .await
        .map_err(map_wallet_error)?;
    Ok(Json(WithdrawalPage {
        withdrawals,
        total,
        page,
        page_size,
    }))
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct WithdrawalStatusResponse {
    pub withdrawal_id: Uuid,
    pub status: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/wallet/withdrawals/{withdrawal_id}/status",
    params(("withdrawal_id" = Uuid, Path, description = "Withdrawal (billing transaction) id")),
    responses(
        (status = 200, description = "Current withdrawal status (refreshed from SafeHaven if pending)", body = WithdrawalStatusResponse),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 404, description = "Withdrawal not found", body = ErrorResponse)
    ),
    tag = "wallet",
    summary = "Get/refresh a withdrawal's transfer status"
)]
pub async fn get_withdrawal_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(withdrawal_id): axum::extract::Path<Uuid>,
) -> AppResult<Json<WithdrawalStatusResponse>> {
    let hospital_id = hospital_id_from_claims(&headers)?;
    let status = state
        .wallet_service
        .refresh_withdrawal_status(hospital_id, withdrawal_id)
        .await
        .map_err(map_wallet_error)?;
    if status == "not_found" {
        return Err(AppError::NotFound("Withdrawal not found".to_string()));
    }
    Ok(Json(WithdrawalStatusResponse {
        withdrawal_id,
        status,
    }))
}

#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct PayoutPage {
    pub payouts: Vec<crate::services::payout_service::PayoutRow>,
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct PayoutStatusResponse {
    pub payout_id: Uuid,
    pub status: String,
}

#[utoipa::path(
    get,
    path = "/api/v1/wallet/payouts",
    params(LedgerQuery),
    responses(
        (status = 200, description = "Paginated payout history", body = PayoutPage),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse)
    ),
    tag = "wallet",
    summary = "List this hospital's payouts",
    description = "Payout transactions (status, amount, shift, provider reference) newest-first."
)]
pub async fn list_payouts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LedgerQuery>,
) -> AppResult<Json<PayoutPage>> {
    let hospital_id = hospital_id_from_claims(&headers)?;
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(50).clamp(1, 200);
    let (payouts, total) = state
        .payout_service
        .list_payouts(hospital_id, page, page_size)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("{e}")))?;
    Ok(Json(PayoutPage {
        payouts,
        total,
        page,
        page_size,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/wallet/payouts/{payout_id}/status",
    params(("payout_id" = Uuid, Path, description = "Payout (billing transaction) id")),
    responses(
        (status = 200, description = "Current payout status (refreshed from SafeHaven if pending)", body = PayoutStatusResponse),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 404, description = "Payout not found", body = ErrorResponse)
    ),
    tag = "wallet",
    summary = "Get/refresh a payout's transfer status"
)]
pub async fn get_payout_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(payout_id): axum::extract::Path<Uuid>,
) -> AppResult<Json<PayoutStatusResponse>> {
    // Scope check: the payout must belong to the caller's hospital.
    let hospital_id = hospital_id_from_claims(&headers)?;
    let owner: Option<Uuid> = sqlx::query_scalar(
        "SELECT hospital_id FROM billing_transactions WHERE id = $1 AND event_type = 'payout'",
    )
    .bind(payout_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::Database)?;
    match owner {
        Some(h) if h == hospital_id => {}
        Some(_) => {
            return Err(AppError::Forbidden(
                "Payout belongs to another hospital".to_string(),
            ))
        }
        None => return Err(AppError::NotFound("Payout not found".to_string())),
    }

    let status = state
        .payout_service
        .refresh_payout_status(payout_id)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("{e}")))?;
    Ok(Json(PayoutStatusResponse { payout_id, status }))
}

#[utoipa::path(
    get,
    path = "/api/v1/wallet/statement",
    responses(
        (status = 200, description = "SafeHaven transfer history for the hospital sub-account"),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 404, description = "No sub-account provisioned yet", body = ErrorResponse)
    ),
    tag = "wallet",
    summary = "Account statement (SafeHaven transfer history)"
)]
pub async fn get_statement(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<serde_json::Value>> {
    let hospital_id = hospital_id_from_claims(&headers)?;
    let wallet = state
        .wallet_service
        .get_wallet(hospital_id)
        .await
        .map_err(map_wallet_error)?;
    let account_id = wallet.safehaven_account_id.ok_or_else(|| {
        AppError::NotFound("No SafeHaven sub-account provisioned yet".to_string())
    })?;
    let data = state
        .safehaven
        .list_transfers(&account_id, 0, 100, None)
        .await
        .map_err(|e| AppError::Conflict(format!("Payment provider error: {e}")))?;
    Ok(Json(data))
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct PayoutRetryResponse {
    pub shift_id: Uuid,
    pub initiated: bool,
    pub message: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/admin/payouts/{shift_id}/retry",
    params(("shift_id" = Uuid, Path, description = "Shift whose payout to retry")),
    responses(
        (status = 200, description = "Retry outcome", body = PayoutRetryResponse),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse)
    ),
    tag = "admin",
    summary = "Manually retry a failed payout (SuperAdmin)"
)]
pub async fn retry_payout(
    State(state): State<AppState>,
    axum::extract::Path(shift_id): axum::extract::Path<Uuid>,
) -> AppResult<Json<PayoutRetryResponse>> {
    let initiated = state
        .payout_service
        .retry_payout(shift_id)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("{e}")))?;
    let message = if initiated {
        "Payout transfer initiated".to_string()
    } else {
        "No retry performed (shift not payable, already paid/in-flight, or retry budget exhausted)"
            .to_string()
    };
    Ok(Json(PayoutRetryResponse {
        shift_id,
        initiated,
        message,
    }))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct ProvisionSubAccountRequest {
    /// OTP sent to the hospital admin's phone during sub-account initiate.
    pub otp: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct SubAccountStatusResponse {
    pub message: String,
}

#[utoipa::path(
    post,
    path = "/api/v1/wallet/sub-account/initiate",
    responses(
        (status = 200, description = "Sub-account verification initiated; OTP sent", body = SubAccountStatusResponse),
        (status = 401, body = ErrorResponse),
        (status = 403, description = "BVN not verified / no hospital", body = ErrorResponse),
        (status = 409, description = "Already provisioned", body = ErrorResponse)
    ),
    tag = "wallet",
    summary = "Initiate SafeHaven sub-account provisioning (sends OTP)"
)]
pub async fn initiate_sub_account(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> AppResult<Json<SubAccountStatusResponse>> {
    let hospital_id = hospital_id_from_claims(&headers)?;

    // Need the admin's verified BVN to drive SafeHaven's sub-account verification.
    let bvn = state
        .identity_service
        .decrypted_number(
            crate::services::identity_verification_service::IdentityOwner::Hospital,
            hospital_id,
            crate::services::identity_verification_service::IdentityKind::Bvn,
        )
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("{e}")))?
        .ok_or_else(|| {
            AppError::Forbidden("Hospital admin BVN must be verified first".to_string())
        })?;

    state
        .wallet_service
        .initiate_sub_account(hospital_id, &bvn)
        .await
        .map_err(map_wallet_error)?;

    Ok(Json(SubAccountStatusResponse {
        message: "Sub-account verification initiated. An OTP has been sent to the registered phone."
            .to_string(),
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/wallet/sub-account/provision",
    request_body = ProvisionSubAccountRequest,
    responses(
        (status = 200, description = "Sub-account provisioned", body = SubAccountStatusResponse),
        (status = 401, body = ErrorResponse),
        (status = 403, body = ErrorResponse),
        (status = 409, description = "Provider rejected / not initiated", body = ErrorResponse)
    ),
    tag = "wallet",
    summary = "Complete SafeHaven sub-account provisioning with the OTP"
)]
pub async fn provision_sub_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ProvisionSubAccountRequest>,
) -> AppResult<Json<SubAccountStatusResponse>> {
    let hospital_id = hospital_id_from_claims(&headers)?;

    // Look up the hospital's contact details for the SafeHaven payload.
    let contact: Option<(String, String)> = sqlx::query_as(
        "SELECT phone_number, email FROM hospitals WHERE id = $1",
    )
    .bind(hospital_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(AppError::Database)?;
    let (phone, email) =
        contact.ok_or_else(|| AppError::NotFound("Hospital not found".to_string()))?;

    state
        .wallet_service
        .provision_sub_account(hospital_id, &phone, &email, &req.otp)
        .await
        .map_err(map_wallet_error)?;

    Ok(Json(SubAccountStatusResponse {
        message: "SafeHaven sub-account provisioned successfully.".to_string(),
    }))
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}
