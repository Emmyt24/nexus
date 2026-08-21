use axum::{
    middleware::from_fn,
    routing::{delete, get, patch, post},
    Router,
};

use crate::middlewares::{require_permission, require_role};
use crate::models::permission::Permission;
use crate::models::user::UserRole;
use sqlx::PgPool;
use std::sync::Arc;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use utoipa::{
    openapi::security::{HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme},
    Modify, OpenApi,
};
use utoipa_swagger_ui::SwaggerUi;

use crate::handlers::{
    admin, auth, clinician_registration, distance, earnings, emails, health, here_maps, hospitals,
    identity, location, notifications, patients, registration, shifts, uploads, video, wallet,
    webhooks,
};
use crate::models::patient_prediction::PipelineEvent;
use crate::repositories::{
    admin::AdminRepository, audit::AuditRepository, billing::BillingRepository,
    clinician::ClinicianRepository,
    hospital::HospitalRepository, identity_verification::IdentityVerificationRepository,
    location::LocationRepository, notification::NotificationRepository,
    patient::PatientRepository, patient_prediction::PatientPredictionRepository,
    shift::ShiftRepository, video_session::VideoSessionRepository, wallet::WalletRepository,
};
use crate::services::{
    admin_service::AdminService, audit_service::AuditService, auth_service::AuthService,
    clinician_registration_service::ClinicianRegistrationService,
    distance_service::DistanceService, email_outbox_service::EmailOutboxService,
    encryption::EncryptionService, fcm::FcmClient, geocoding::GeocodingClient,
    here_maps::HereMapsClient, identity_verification_service::IdentityVerificationService,
    livekit::LiveKitClient, location_service::LocationService, ml_client::MlClient,
    notification_service::NotificationService,
    patient_prediction_service::{PatientPredictionService, PatientPredictionWorker},
    payout_service::PayoutService, push_service::PushService,
    registration_service::RegistrationService, safehaven::SafeHavenClient,
    shift_service::ShiftService, video_service::VideoService, wallet_service::WalletService,
};
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub registration_service: Arc<RegistrationService>,
    pub clinician_registration_service: Arc<ClinicianRegistrationService>,
    pub auth_service: Arc<AuthService>,
    pub shift_service: Arc<ShiftService>,
    pub wallet_service: Arc<WalletService>,
    pub payout_service: Arc<PayoutService>,
    pub clinician_repo: Arc<ClinicianRepository>,
    pub admin_service: Arc<AdminService>,
    pub identity_service: Arc<IdentityVerificationService>,
    pub safehaven: Arc<SafeHavenClient>,
    pub here_maps_client: Arc<HereMapsClient>,
    pub distance_service: Arc<DistanceService>,
    pub push_service: Arc<PushService>,
    pub email_outbox: Arc<EmailOutboxService>,
    pub patient_repo: Arc<PatientRepository>,
    pub patient_prediction_service: Arc<PatientPredictionService>,
    pub video_service: Arc<VideoService>,
}

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::handlers::health::health_check,
        crate::handlers::health::db_health_check,
        // Auth
        crate::handlers::auth::email_otp_send,
        crate::handlers::auth::email_otp_verify,
        crate::handlers::auth::admin_login,
        crate::handlers::hospitals::get_hospital,
        crate::handlers::hospitals::get_hospital_location,
        crate::handlers::clinician_registration::get_worker_public,
        crate::handlers::clinician_registration::set_avatar,
        crate::handlers::uploads::upload_signature,
        crate::handlers::auth::me,
        crate::handlers::auth::refresh_token,
        crate::handlers::auth::logout,
        crate::handlers::emails::send_email,
        crate::handlers::registration::register_hospital,
        crate::handlers::registration::list_hospitals,
        crate::handlers::registration::get_registration_status,
        crate::handlers::registration::approve_hospital,
        crate::handlers::registration::reject_hospital,
        crate::handlers::clinician_registration::send_otp,
        crate::handlers::clinician_registration::verify_otp,
        crate::handlers::clinician_registration::complete_profile,
        crate::handlers::clinician_registration::add_bank_account,
        // Identity verification (BVN/NIN) + bank list
        crate::handlers::identity::hospital_initiate,
        crate::handlers::identity::hospital_validate,
        crate::handlers::identity::clinician_initiate,
        crate::handlers::identity::clinician_validate,
        crate::handlers::identity::list_banks,
        crate::handlers::identity::resolve_account,
        // Location & Distance
        crate::handlers::distance::calculate_distance,
        crate::handlers::here_maps::geocode_address,
        crate::handlers::here_maps::reverse_geocode,
        crate::handlers::location::search_nearby_facilities,
        crate::handlers::location::search_nexuscare_facilities,
        crate::handlers::location::autocomplete_address,
        crate::handlers::location::search_nearby_shifts,
        // Shifts
        crate::handlers::shifts::create_shift,
        crate::handlers::shifts::list_shifts,
        crate::handlers::shifts::preview_shift,
        crate::handlers::shifts::get_shift,
        crate::handlers::shifts::express_interest,
        crate::handlers::shifts::apply_for_shift,
        crate::handlers::shifts::list_shift_applications,
        crate::handlers::shifts::list_interested_for_shift,
        crate::handlers::shifts::offer_shift,
        crate::handlers::shifts::accept_shift,
        crate::handlers::shifts::decline_shift,
        crate::handlers::shifts::clock_in,
        crate::handlers::shifts::submit_handover,
        crate::handlers::shifts::clock_out,
        crate::handlers::shifts::request_handover_revision,
        crate::handlers::shifts::approve_handover,
        crate::handlers::shifts::rate_worker,
        crate::handlers::shifts::rate_hospital,
        crate::handlers::shifts::edit_rating,
        crate::handlers::shifts::list_nearby_shifts,
        crate::handlers::shifts::list_my_applications,
        crate::handlers::shifts::withdraw_interest,
        crate::handlers::shifts::bookmark_shift,
        crate::handlers::shifts::unbookmark_shift,
        crate::handlers::shifts::dismiss_shift,
        crate::handlers::shifts::request_clockin_approval,
        crate::handlers::shifts::approve_clockin_request,
        crate::handlers::shifts::deny_clockin_request,
        crate::handlers::shifts::assign_shift,
        crate::handlers::shifts::cancel_shift,
        crate::handlers::shifts::reschedule_shift,
        crate::handlers::admin::list_hospitals_admin,
        crate::handlers::admin::list_clinicians_admin,
        // Admin dashboard (§11)
        crate::handlers::admin::metrics_dashboard,
        crate::handlers::admin::metrics_shift_volume,
        crate::handlers::admin::metrics_geographic,
        crate::handlers::admin::metrics_worker_performance,
        crate::handlers::admin::metrics_revenue,
        crate::handlers::admin::metrics_ai_usage,
        crate::handlers::admin::hospitals_pending,
        crate::handlers::admin::workers_pending,
        crate::handlers::admin::list_disputes,
        crate::handlers::admin::resolve_dispute,
        crate::handlers::admin::payments_failed,
        crate::handlers::admin::reports_generate,
        crate::handlers::admin::get_settings,
        crate::handlers::admin::update_settings,
        crate::handlers::admin::suspend_hospital,
        crate::handlers::admin::unsuspend_hospital,
        crate::handlers::admin::verify_worker,
        crate::handlers::admin::reject_worker,
        crate::handlers::admin::suspend_worker,
        crate::handlers::admin::unsuspend_worker,
        crate::handlers::admin::list_admin_shifts,
        crate::handlers::admin::cancel_admin_shift,
        crate::handlers::admin::manual_payout,
        crate::handlers::admin::create_admin,
        crate::handlers::admin::list_admins,
        crate::handlers::admin::update_admin,
        crate::handlers::admin::get_hospital_detail,
        crate::handlers::admin::get_worker_detail,
        crate::handlers::admin::metrics_revenue_trend,
        crate::handlers::admin::recent_activities,
        crate::handlers::admin::global_search,
        // Wallet
        crate::handlers::wallet::get_wallet,
        crate::handlers::wallet::get_ledger,
        crate::handlers::wallet::create_deposit,
        crate::handlers::wallet::list_deposits,
        crate::handlers::wallet::reconcile_deposits,
        crate::handlers::wallet::withdraw,
        crate::handlers::wallet::list_withdrawals,
        crate::handlers::wallet::get_withdrawal_status,
        crate::handlers::wallet::initiate_sub_account,
        crate::handlers::wallet::provision_sub_account,
        crate::handlers::wallet::list_payouts,
        crate::handlers::wallet::get_payout_status,
        crate::handlers::wallet::get_statement,
        crate::handlers::wallet::retry_payout,
        // Patients / ML pipeline
        crate::handlers::patients::ingest_patient,
        crate::handlers::patients::get_patient,
        // Virtual consultations (LiveKit)
        crate::handlers::video::issue_join_token,
        crate::handlers::video::get_session,
        crate::handlers::video::leave_session,
        crate::handlers::video::end_session,
        // Webhooks
        crate::handlers::webhooks::safehaven_webhook,
        crate::handlers::webhooks::livekit_webhook,
        // Earnings
        crate::handlers::earnings::get_earnings,
        // Notifications & devices
        crate::handlers::notifications::register_device,
        crate::handlers::notifications::revoke_device,
        crate::handlers::notifications::list_notifications,
        crate::handlers::notifications::mark_notification_read,
    ),
    components(
        schemas(
            // Emails
            crate::handlers::emails::SendEmailRequest,
            crate::handlers::emails::SendEmailResponse,
            // Registration
            crate::handlers::registration::HospitalRegistrationResponse,
            crate::handlers::registration::StatusChangeResponse,
            crate::handlers::registration::ApprovalRequest,
            crate::handlers::registration::RejectionRequest,
            crate::handlers::registration::ErrorResponse,
            crate::handlers::registration::ListHospitalsQuery,
            // Shifts
            crate::handlers::shifts::ShiftPreviewResponse,
            crate::handlers::shifts::ErrorResponse,
            crate::handlers::shifts::ShiftListResponse,
            crate::handlers::shifts::ShiftApplicationsResponse,
            crate::handlers::shifts::PaginationMetadata,
            crate::models::shift::RankedInterestedClinician,
            crate::models::shift::ShiftOfferRequest,
            crate::models::shift::ShiftOfferResponse,
            crate::models::shift::NdprConsent,
            crate::models::shift::AcceptShiftRequest,
            crate::models::shift::DeclineShiftRequest,
            crate::models::shift::ClockinRequest,
            crate::models::shift::ClockinResponse,
            crate::models::shift::ClockinMethod,
            crate::models::shift::SubmitHandoverRequest,
            crate::models::shift::HandoverResponse,
            crate::models::shift::ClockoutResponse,
            crate::models::shift::HandoverRevisionRequest,
            crate::models::shift::HospitalRatingDimensions,
            crate::models::shift::RateWorkerRequest,
            crate::models::shift::RateHospitalRequest,
            crate::models::shift::EditRatingRequest,
            crate::models::shift::RatingResponse,
            crate::models::shift::NearbyShiftCard,
            crate::models::shift::ShiftDetailResponse,
            crate::models::shift::HospitalRatingSummary,
            crate::models::shift::QualificationMatch,
            crate::models::shift::HospitalLocation,
            crate::models::shift::MyApplicationEntry,
            crate::models::shift::ClockinApprovalRequest,
            crate::models::shift::ClockinApprovalDecisionRequest,
            crate::models::shift::ClockinApprovalRecord,
            // Virtual consultations (LiveKit). No ErrorResponse here on
            // purpose — video.rs reuses the shifts one.
            crate::models::video_session::JoinConsultRequest,
            crate::models::video_session::JoinConsultResponse,
            crate::models::video_session::ConsultSessionView,
            crate::models::video_session::ConsultParticipantView,
            crate::models::video_session::ConsultShiftSummary,
            crate::models::video_session::ConsultClockInView,
            crate::models::video_session::ConsultRecordingView,
            crate::models::video_session::EndConsultRequest,
            crate::models::video_session::EndConsultResponse,
            crate::models::video_session::LeaveConsultResponse,
            crate::models::video_session::ParticipantRole,
            crate::models::video_session::JoinMode,
            crate::models::video_session::VideoSessionStatus,
            // Patients / ML pipeline
            crate::models::patient::NewPatientRequest,
            crate::models::patient::PatientResponse,
            crate::models::patient_prediction::PredictionResponse,
            crate::handlers::patients::IngestPatientResponse,
            crate::handlers::patients::PatientDetailResponse,
            crate::handlers::patients::ErrorResponse,
            crate::handlers::patients::ErrorDetail,
            // Wallet
            crate::models::wallet::WalletSummary,
            crate::models::wallet::WalletLedgerEntry,
            crate::models::wallet::WalletDepositRequest,
            crate::models::wallet::CreateDepositRequest,
            crate::models::wallet::DepositResponse,
            crate::models::wallet::DepositInstructions,
            crate::models::wallet::WithdrawRequest,
            crate::models::wallet::WithdrawResponse,
            crate::models::wallet::WithdrawalRow,
            crate::handlers::wallet::WithdrawalPage,
            crate::handlers::wallet::WithdrawalStatusResponse,
            crate::services::wallet_service::ReconcileResult,
            crate::handlers::wallet::LedgerPage,
            crate::handlers::wallet::PayoutPage,
            crate::handlers::wallet::PayoutStatusResponse,
            crate::handlers::wallet::ProvisionSubAccountRequest,
            crate::handlers::wallet::SubAccountStatusResponse,
            crate::handlers::wallet::PayoutRetryResponse,
            crate::services::payout_service::PayoutRow,
            crate::handlers::earnings::EarningsSummary,
            crate::handlers::earnings::EarningsTransaction,
            // Notifications & devices
            crate::handlers::notifications::ErrorResponse,
            crate::models::notification::RegisterDeviceRequest,
            crate::models::notification::RevokeDeviceRequest,
            crate::models::notification::DevicePlatform,
            crate::models::notification::Notification,
            crate::models::notification::NotificationPage,
            // Admin
            crate::handlers::admin::ClinicianListResponse,
            crate::handlers::admin::PaginationMetadata,
            crate::handlers::admin::ListCliniciansQuery,
            // Admin dashboard (§11)
            crate::models::admin::DashboardMetrics,
            crate::models::admin::ShiftVolumePoint,
            crate::models::admin::GeoDistributionPoint,
            crate::models::admin::RatingBucket,
            crate::models::admin::WorkerPerformance,
            crate::models::admin::TopPerformer,
            crate::models::admin::RevenueSlice,
            crate::models::admin::RevenueBreakdown,
            crate::models::admin::AiUsageMetrics,
            crate::models::admin::LanguageCount,
            crate::models::admin::FailedPayment,
            crate::models::admin::Dispute,
            crate::models::admin::ResolveDisputeRequest,
            crate::models::admin::PlatformSettings,
            crate::models::admin::UpdatePlatformSettings,
            crate::models::admin::GenerateReportRequest,
            crate::models::admin::GenerateReportResponse,
            crate::models::admin::ReasonRequest,
            crate::models::admin::VerifyWorkerRequest,
            crate::models::admin::AdminActionResponse,
            crate::models::admin::AdminShiftRow,
            crate::models::admin::ManualPayoutRequest,
            crate::models::admin::CreateAdminRequest,
            crate::models::admin::UpdateAdminRequest,
            crate::models::admin::AdminSummary,
            crate::models::admin::HospitalDetail,
            crate::models::admin::WorkerDetail,
            crate::models::admin::RevenuePoint,
            crate::models::admin::RevenueTrend,
            crate::models::admin::ActivityItem,
            crate::models::admin::SearchHit,
            crate::models::admin::SearchResults,
            crate::handlers::admin::RevenueTrendQuery,
            crate::handlers::admin::ActivitiesQuery,
            crate::handlers::admin::SearchQuery,
            // Models
            crate::models::admin_registration::HospitalRegistrationRequest,
            crate::models::admin_registration::Address,
            crate::models::admin_registration::Coordinates,
            crate::models::admin_registration::PaymentDetails,
            crate::models::admin_registration::PaymentMethodType,
            crate::models::shift::Shift,
            crate::models::shift::CreateShiftRequest,
            crate::models::shift::ShiftStatus,
            crate::models::shift::ShiftPriority,
            crate::models::shift::ShiftType,
            crate::models::shift::RoleCategory,
            crate::models::shift::PayType,
            crate::models::shift::ShiftApplication,
            crate::models::shift::ShiftApplicationRequest,
            crate::models::shift::ShiftApplicationStatus,
            crate::models::shift::ShiftApplicationsQuery,
            crate::models::shift::ShiftListQuery,
            crate::models::shift::ShiftInterestRequest,
            crate::models::shift::ShiftAssignRequest,
            crate::models::shift::ShiftCancelRequest,
            crate::models::shift::ShiftRescheduleRequest,
            crate::models::user::UserResponse,
            crate::models::user::LoginRequest,
            crate::models::user::LoginResponse,
            crate::models::hospital::HospitalPublicDetail,
            crate::models::clinician::WorkerPublicDetail,
            crate::handlers::auth::MeResponse,
            crate::handlers::auth::ClinicianProfile,
            crate::handlers::auth::HospitalProfile,
            crate::models::user::EmailLoginRequest,
            crate::models::user::EmailOtpVerifyRequest,
            crate::models::user::RefreshTokenRequest,
            crate::models::user::LogoutRequest,
            crate::models::clinician_registration::SendOtpRequest,
            crate::models::clinician_registration::SendOtpResponse,
            crate::models::clinician_registration::VerifyOtpRequest,
            crate::models::clinician_registration::VerifyOtpResponse,
            crate::models::clinician_registration::CompleteProfileRequest,
            crate::models::clinician_registration::ProfileResponse,
            crate::models::clinician_registration::AddBankAccountRequest,
            crate::models::clinician_registration::BankAccountResponse,
            crate::models::clinician_registration::SetAvatarRequest,
            crate::handlers::hospitals::HospitalLocationResponse,
            crate::services::cloudinary::SignedUpload,
            crate::models::clinician::ClinicianAdminSummary,
            // Identity verification
            crate::handlers::identity::InitiateIdentityRequest,
            crate::handlers::identity::ValidateIdentityRequest,
            crate::handlers::identity::IdentityStatusResponse,
            crate::handlers::identity::ResolveAccountRequest,
            crate::handlers::identity::ResolveAccountResponse,
            // Services
            crate::services::registration_service::RegistrationStatusResponse,
            crate::services::registration_service::HospitalListResponse,
            crate::services::registration_service::HospitalSummary,
            crate::services::registration_service::PaginationMetadata,
            // Location & HERE Maps models
            crate::models::here_maps::FacilitySearchResponse,
            crate::models::here_maps::AddressAutocompleteResponse,
            crate::models::here_maps::Facility,
            crate::models::here_maps::AddressSuggestion,
            crate::models::here_maps::Position,
            crate::models::here_maps::ContactInfo,
            crate::handlers::location::FacilitySearchParams,
            crate::handlers::location::AutocompleteParams,
            crate::handlers::location::NearbyShiftsResponse,
            crate::handlers::location::FacilityWithShifts,
            crate::handlers::location::SimpleShift,
            // HERE Maps geocoding models
            crate::handlers::here_maps::GeocodeResponse,
            crate::handlers::here_maps::GeocodeItem,
            crate::handlers::here_maps::GeocodePosition,
            crate::handlers::here_maps::ReverseGeocodeResponse,
            crate::handlers::here_maps::ReverseGeocodeItem,
            crate::handlers::here_maps::AddressDetails,
            // Distance calculation models
            crate::models::distance::DistanceRequest,
            crate::models::distance::DistanceResponse,
            crate::models::distance::LocationInput,
            crate::models::distance::LocationType,
            crate::models::distance::LocationDetails,
            crate::models::distance::DistanceInfo,
            crate::models::distance::TimeInfo,
            crate::models::distance::RouteSummary,
        )
    ),
    info(
        title = "NexusCare Hospital Management API",
        version = "1.0.0",
        description = "Hospital management, ML pipeline, real-time SSE events",
        contact(name = "NexusCare Support", email = "support@nexuscare.com")
    ),
    servers(
        (url = "https://nexus-j2rp.onrender.com", description = "Production (Render)"),
        (url = "http://localhost:8080", description = "Local development")
    ),
    tags(
        (name = "health", description = "Health check endpoints"),
        (name = "auth", description = "Authentication and authorization endpoints"),
        (name = "hospitals", description = "Hospital management endpoints"),
        (name = "clinicians", description = "Clinician registration and management endpoints"),
        (name = "shifts", description = "Shift creation and management endpoints"),
        (name = "location", description = "Location services — nearby facilities, address autocomplete, HERE Maps integration"),
        (name = "admin", description = "Admin-only endpoints"),
        (name = "wallet", description = "Hospital wallet — balance, deposits, ledger (Tier 2)"),
        (name = "video", description = "Virtual consultations — LiveKit rooms, join tokens, and session state"),
        (name = "webhooks", description = "Inbound webhooks from external providers (SafeHaven, LiveKit)"),
        (name = "earnings", description = "Worker earnings — totals + transaction history"),
        (name = "identity", description = "BVN/NIN identity verification and bank list"),
        (name = "notifications", description = "Device push-token registration and the in-app notification center"),
        (name = "emails", description = "Generic frontend-templated transactional email relay")
    ),
    modifiers(&SecurityAddon)
)]
struct ApiDoc;

/// Swagger UI security wiring — adds the `bearerAuth` scheme so the
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        // 1. Declare the scheme so the "Authorize" button appears.
        let components = openapi
            .components
            .get_or_insert_with(utoipa::openapi::Components::new);
        components.add_security_scheme(
            "bearerAuth",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("JWT")
                    .description(Some("Paste a JWT from POST /api/v1/auth/otp/verify."))
                    .build(),
            ),
        );

        // 2. Apply the scheme to every operation by default. Endpoints that
        openapi.security = Some(vec![SecurityRequirement::new("bearerAuth", [""; 0])]);
    }
}

pub fn create_router(
    pool: PgPool,
    notification_service: Arc<NotificationService>,
    email_outbox_service: Arc<EmailOutboxService>,
) -> (Router, AppState) {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let hospital_repo = Arc::new(HospitalRepository::new(pool.clone()));
    let location_repo = Arc::new(LocationRepository::new(pool.clone()));
    // Held for wallet ledger; constructed here so the connection pool
    let _billing_repo = Arc::new(BillingRepository::new(pool.clone()));
    let audit_repo = Arc::new(AuditRepository::new(pool.clone()));
    let clinician_repo = Arc::new(ClinicianRepository::new(pool.clone()));
    let shift_repo = Arc::new(ShiftRepository::new(pool.clone()));
    let patient_repo = Arc::new(PatientRepository::new(pool.clone()));
    let patient_prediction_repo = Arc::new(PatientPredictionRepository::new(pool.clone()));

    let geocoding_client = Arc::new(GeocodingClient::new(std::env::var("GEOCODING_API_URL").ok()));

    let safehaven_client = Arc::new(SafeHavenClient::from_env());

    let encryption_service = Arc::new({
        let key_hex = std::env::var("ENCRYPTION_KEY").unwrap_or_else(|_| "0".repeat(64));
        let key_bytes = hex::decode(&key_hex).unwrap_or_else(|_| vec![0u8; 32]);
        EncryptionService::new(key_bytes).expect("Failed to create encryption service")
    });

    // Initialize business services
    let here_api_key = std::env::var("HERE_API_KEY").unwrap_or_default();
    let here_maps_client = Arc::new(HereMapsClient::new(here_api_key));
    let distance_service = Arc::new(DistanceService::new(here_maps_client.clone(), true));

    let location_service = Arc::new(LocationService::new(
        geocoding_client.clone(),
        location_repo.clone(),
    ));

    let audit_service = Arc::new(AuditService::new(audit_repo));

    // Identity verification (BVN/NIN) — shared by both registration flows
    let identity_repo = Arc::new(IdentityVerificationRepository::new(pool.clone()));
    let identity_service = Arc::new(IdentityVerificationService::new(
        safehaven_client.clone(),
        encryption_service.clone(),
        identity_repo,
    ));

    // Initialize wallet service. Threaded into registration_service
    let wallet_repo = Arc::new(WalletRepository::new(pool.clone()));
    let wallet_service = Arc::new(WalletService::new(
        wallet_repo.clone(),
        safehaven_client.clone(),
        pool.clone(),
    ));

    let registration_service = Arc::new(RegistrationService::new(
        hospital_repo,
        location_service,
        audit_service,
        email_outbox_service.clone(),
        wallet_service.clone(),
        pool.clone(),
        identity_service.clone(),
    ));

    let clinician_registration_service = Arc::new(ClinicianRegistrationService::new(
        clinician_repo.clone(),
        email_outbox_service.clone(),
        safehaven_client.clone(),
        encryption_service.clone(),
        pool.clone(),
        identity_service.clone(),
    ));

    let auth_service = Arc::new(AuthService::new(pool.clone(), email_outbox_service.clone()));

    // Push notifications: FCM client (mock unless FCM_SERVER_KEY is set),
    // notification repo, and the service tying them together.
    let notification_repo = Arc::new(NotificationRepository::new(pool.clone()));
    let fcm_client = Arc::new(FcmClient::from_env());
    let push_service = Arc::new(PushService::new(notification_repo, fcm_client));

    // Initialize shift service
    let shift_service = Arc::new(ShiftService::new(
        shift_repo.clone(),
        pool.clone(),
        notification_service.clone(),
        email_outbox_service.clone(),
        wallet_service.clone(),
        push_service.clone(),
    ));

    // Initialize payout service. Borrows the wallet repo so it can
    let payout_service = Arc::new(PayoutService::new(
        pool.clone(),
        wallet_repo.clone(),
        clinician_repo.clone(),
        safehaven_client.clone(),
        encryption_service.clone(),
    ));

    // LiveKit video consultations. Mock unless LIVEKIT_API_KEY/SECRET are set,
    // so local dev and CI need no credentials. VideoService depends on
    // ShiftService and never the reverse, which is why there is no Arc cycle.
    let livekit_client = Arc::new(LiveKitClient::from_env());
    if livekit_client.is_mock() {
        tracing::warn!("LiveKit running in MOCK mode — join tokens are fake");
    }
    let video_repo = Arc::new(VideoSessionRepository::new(pool.clone()));
    let video_service = Arc::new(VideoService::new(
        video_repo,
        shift_repo.clone(),
        shift_service.clone(),
        livekit_client,
        push_service.clone(),
    ));

    let admin_repo = Arc::new(AdminRepository::new(pool.clone()));
    let admin_service = Arc::new(AdminService::new(admin_repo, email_outbox_service.clone()));

    // Patient ML prediction pipeline: ingest queues a row; the worker polls and
    // calls the ML service, broadcasting pipeline events to any SSE subscribers.
    let ml_client = Arc::new(MlClient::from_env());
    let (patient_event_tx, _patient_event_rx) = broadcast::channel::<PipelineEvent>(256);
    let patient_event_tx = Arc::new(patient_event_tx);
    let patient_prediction_service = Arc::new(PatientPredictionService::new(
        pool.clone(),
        patient_repo.clone(),
        patient_prediction_repo.clone(),
        ml_client,
        patient_event_tx,
    ));
    let patient_prediction_worker =
        PatientPredictionWorker::new(patient_prediction_service.clone());
    tokio::spawn(patient_prediction_worker.run());

    let state = AppState {
        pool: pool.clone(),
        registration_service,
        clinician_registration_service,
        auth_service,
        shift_service,
        wallet_service,
        payout_service,
        clinician_repo: clinician_repo.clone(),
        admin_service,
        identity_service,
        safehaven: safehaven_client.clone(),
        here_maps_client,
        distance_service,
        push_service,
        email_outbox: email_outbox_service.clone(),
        patient_repo: patient_repo.clone(),
        patient_prediction_service,
        video_service,
    };

    let api_router = Router::new()
        .route("/health", get(health::health_check))
        .route("/health/db", get(health::db_health_check))
        // Auth (OTP-only).
        .route("/api/v1/auth/otp/send", post(auth::email_otp_send))
        .route("/api/v1/auth/otp/verify", post(auth::email_otp_verify))
        .route("/api/v1/auth/admin/login", post(auth::admin_login))
        .route("/api/v1/auth/refresh", post(auth::refresh_token))
        .route("/api/v1/auth/logout", post(auth::logout))
        // Generic frontend-templated email relay (authenticated).
        .route("/api/v1/emails/send", post(emails::send_email))
        .route("/api/v1/auth/me", get(auth::me))
        // Hospital Registration
        .route(
            "/api/v1/hospitals/register",
            post(registration::register_hospital),
        )
        .route("/api/v1/hospitals", get(registration::list_hospitals))
        .route(
            "/api/v1/hospitals/{hospital_id}/status",
            get(registration::get_registration_status),
        )
        // Admin dashboard (§11) — every /admin/* route is permission-gated inside
        // this group; approve/reject/list moved here to close a prior auth gap.
        .merge(admin_dashboard_routes())
        // Existing Hospitals endpoints (legacy - for backward compatibility)
        .route("/api/v1/hospitals/create", post(hospitals::create_hospital))
        .route("/api/v1/hospitals/{id}", get(hospitals::get_hospital))
        .route("/api/v1/hospitals/{id}", patch(hospitals::update_hospital))
        .route(
            "/api/v1/hospitals/{id}/location",
            get(hospitals::get_hospital_location),
        )
        .route(
            "/api/v1/hospitals/{id}/advance-step",
            patch(hospitals::advance_registration_step),
        )
        // Clinician registration
        .route(
            "/api/v1/clinicians/otp/send",
            post(clinician_registration::send_otp),
        )
        .route(
            "/api/v1/clinicians/otp/verify",
            post(clinician_registration::verify_otp),
        )
        // Public (ungated) worker profile.
        .route(
            "/api/v1/workers/{id}",
            get(clinician_registration::get_worker_public),
        )
        .route(
            "/api/v1/clinicians/{clinician_id}/profile",
            axum::routing::put(clinician_registration::complete_profile),
        )
        .route(
            "/api/v1/clinicians/{clinician_id}/bank-account",
            post(clinician_registration::add_bank_account),
        )
        .route(
            "/api/v1/clinicians/{clinician_id}/avatar",
            patch(clinician_registration::set_avatar),
        )
        // Cloudinary signed-upload signature (any authenticated user)
        .route(
            "/api/v1/uploads/signature",
            get(uploads::upload_signature),
        )
        // Identity verification (BVN/NIN) + bank list
        .route(
            "/api/v1/hospitals/{hospital_id}/identity",
            get(identity::hospital_get_identity),
        )
        .route(
            "/api/v1/hospitals/{hospital_id}/identity/initiate",
            post(identity::hospital_initiate),
        )
        .route(
            "/api/v1/hospitals/{hospital_id}/identity/validate",
            post(identity::hospital_validate),
        )
        .route(
            "/api/v1/clinicians/{clinician_id}/identity",
            get(identity::clinician_get_identity),
        )
        .route(
            "/api/v1/clinicians/{clinician_id}/identity/initiate",
            post(identity::clinician_initiate),
        )
        .route(
            "/api/v1/clinicians/{clinician_id}/identity/validate",
            post(identity::clinician_validate),
        )

        .route("/api/v1/banks", get(identity::list_banks))
        .route("/api/v1/banks/resolve", post(identity::resolve_account))
        // Location services
        .route(
            "/api/v1/distance/calculate",
            post(distance::calculate_distance),
        )
        .route("/api/v1/here/geocode", get(here_maps::geocode_address))
        .route(
            "/api/v1/here/reverse-geocode",
            get(here_maps::reverse_geocode),
        )
        .route(
            "/api/v1/location/health-facilities/search",
            get(location::search_nearby_facilities),
        )
        .route(
            "/api/v1/location/nexuscare-facilities/search",
            get(location::search_nexuscare_facilities),
        )
        .route(
            "/api/v1/location/address/autocomplete",
            get(location::autocomplete_address),
        )
        .route(
            "/api/v1/location/nearby-shifts",
            get(location::search_nearby_shifts),
        )
        // Shifts — gated per FRS v2.0 permission matrix.
        .route(
            "/api/v1/shifts",
            post(shifts::create_shift).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts",
            get(shifts::list_shifts).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts/preview",
            post(shifts::preview_shift).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route("/api/v1/shifts/{shift_id}", get(shifts::get_shift))
        .route(
            "/api/v1/shifts/{shift_id}/interest",
            post(shifts::express_interest)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/apply",
            post(shifts::apply_for_shift)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/applications",
            get(shifts::list_shift_applications).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/interested",
            get(shifts::list_interested_for_shift).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/offer",
            post(shifts::offer_shift).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/accept",
            post(shifts::accept_shift)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/decline",
            post(shifts::decline_shift)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/clockin",
            post(shifts::clock_in).route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/handover",
            post(shifts::submit_handover)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/clockout",
            post(shifts::clock_out).route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/handover/revision",
            post(shifts::request_handover_revision).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/handover/approve",
            post(shifts::approve_handover).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/ratings/worker",
            post(shifts::rate_worker).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/ratings/hospital",
            post(shifts::rate_hospital)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route("/api/v1/ratings/{rating_id}", patch(shifts::edit_rating))
        .route(
            "/api/v1/worker/shifts/nearby",
            get(shifts::list_nearby_shifts)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route(
            "/api/v1/worker/shifts/my-applications",
            get(shifts::list_my_applications)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/interest",
            delete(shifts::withdraw_interest)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/bookmark",
            post(shifts::bookmark_shift)
                .delete(shifts::unbookmark_shift)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/dismiss",
            post(shifts::dismiss_shift)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/clockin/approval-request",
            post(shifts::request_clockin_approval)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        .route(
            "/api/v1/clockin-approvals/{request_id}/approve",
            post(shifts::approve_clockin_request).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/clockin-approvals/{request_id}/deny",
            post(shifts::deny_clockin_request).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/assign",
            post(shifts::assign_shift).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/cancel",
            post(shifts::cancel_shift).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/reschedule",
            post(shifts::reschedule_shift).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        // ---- Virtual consultations (LiveKit). The role guard is coarse on
        // purpose: VideoService does the fine-grained check against
        // shifts.assigned_clinician_id / claims.hospital_id, which is the only
        // place that knows which hospital owns the shift.
        .route(
            "/api/v1/shifts/{shift_id}/consult/token",
            post(video::issue_join_token).route_layer(from_fn(require_role(&[
                UserRole::HealthWorker,
                UserRole::HospitalAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/consult/leave",
            post(video::leave_session).route_layer(from_fn(require_role(&[
                UserRole::HealthWorker,
                UserRole::HospitalAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/consult",
            get(video::get_session).route_layer(from_fn(require_role(&[
                UserRole::HealthWorker,
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
                UserRole::OperationsAdmin,
            ]))),
        )
        .route(
            "/api/v1/shifts/{shift_id}/consult/end",
            post(video::end_session).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
                UserRole::OperationsAdmin,
            ]))),
        )
        // ---- Wallet — HospitalAdmin/SuperAdmin only.
        .route(
            "/api/v1/wallet",
            get(wallet::get_wallet).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/wallet/ledger",
            get(wallet::get_ledger).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/wallet/deposits",
            post(wallet::create_deposit)
                .get(wallet::list_deposits)
                .route_layer(from_fn(require_role(&[
                    UserRole::HospitalAdmin,
                    UserRole::SuperAdmin,
                ]))),
        )
        .route(
            "/api/v1/wallet/reconcile",
            post(wallet::reconcile_deposits).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/wallet/withdraw",
            post(wallet::withdraw).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/wallet/withdrawals",
            get(wallet::list_withdrawals).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/wallet/withdrawals/{withdrawal_id}/status",
            get(wallet::get_withdrawal_status).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/wallet/sub-account/initiate",
            post(wallet::initiate_sub_account)
                .route_layer(from_fn(require_role(&[UserRole::HospitalAdmin, UserRole::SuperAdmin]))),
        )
        .route(
            "/api/v1/wallet/sub-account/provision",
            post(wallet::provision_sub_account)
                .route_layer(from_fn(require_role(&[UserRole::HospitalAdmin, UserRole::SuperAdmin]))),
        )
        .route(
            "/api/v1/wallet/payouts",
            get(wallet::list_payouts).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/wallet/payouts/{payout_id}/status",
            get(wallet::get_payout_status).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/wallet/statement",
            get(wallet::get_statement).route_layer(from_fn(require_role(&[
                UserRole::HospitalAdmin,
                UserRole::SuperAdmin,
            ]))),
        )
        .route(
            "/api/v1/admin/payouts/{shift_id}/retry",
            post(wallet::retry_payout)
                .route_layer(from_fn(require_permission(Permission::ProcessPayouts))),
        )
        // ---- Patients / ML pipeline — authenticated (claims checked in-handler).
        .route("/api/v1/ingest/patient", post(patients::ingest_patient))
        .route("/api/v1/patients/{id}", get(patients::get_patient))
        // ---- Webhooks — authenticated by HMAC signature, not JWT.
        .route(
            "/api/v1/webhooks/safehaven",
            post(webhooks::safehaven_webhook),
        )
        // Authenticated by LiveKit's own signed JWT, not ours.
        .route("/api/v1/webhooks/livekit", post(webhooks::livekit_webhook))
        // ---- Worker earnings — HealthWorker only.
        .route(
            "/api/v1/worker/earnings",
            get(earnings::get_earnings)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        // ---- Push notifications — any authenticated user.
        .route(
            "/api/v1/devices/token",
            post(notifications::register_device).delete(notifications::revoke_device),
        )
        .route(
            "/api/v1/notifications",
            get(notifications::list_notifications),
        )
        .route(
            "/api/v1/notifications/{notification_id}/read",
            post(notifications::mark_notification_read),
        )
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state.clone());

    // Merge with Swagger UI
    let router = Router::new()
        .merge(SwaggerUi::new("/api/docs").url("/api/openapi.json", ApiDoc::openapi()))
        .merge(api_router);

    (router, state)
}

/// Admin dashboard (§11) routes. Each route is guarded by the specific
/// `Permission` its matrix cell requires (Admin §1.2) via `require_permission`.
fn admin_dashboard_routes() -> Router<AppState> {
    use Permission as P;
    Router::new()
        // §1 hospital verify (approve/reject) — VerifyHospitals.
        .route(
            "/api/v1/admin/hospitals/{hospital_id}/approve",
            post(registration::approve_hospital)
                .route_layer(from_fn(require_permission(P::VerifyHospitals))),
        )
        .route(
            "/api/v1/admin/hospitals/{hospital_id}/reject",
            post(registration::reject_hospital)
                .route_layer(from_fn(require_permission(P::VerifyHospitals))),
        )
        // §1 hospital suspend / unsuspend — SuspendHospitals.
        .route(
            "/api/v1/admin/hospitals/{id}/suspend",
            post(admin::suspend_hospital)
                .route_layer(from_fn(require_permission(P::SuspendHospitals))),
        )
        .route(
            "/api/v1/admin/hospitals/{id}/unsuspend",
            post(admin::unsuspend_hospital)
                .route_layer(from_fn(require_permission(P::SuspendHospitals))),
        )
        // §1 hospital listings — ViewHospitals.
        .route(
            "/api/v1/admin/hospitals",
            get(admin::list_hospitals_admin)
                .route_layer(from_fn(require_permission(P::ViewHospitals))),
        )
        .route(
            "/api/v1/admin/hospitals/pending",
            get(admin::hospitals_pending)
                .route_layer(from_fn(require_permission(P::ViewHospitals))),
        )
        // §2 worker listings — ViewWorkers.
        .route(
            "/api/v1/admin/clinicians",
            get(admin::list_clinicians_admin)
                .route_layer(from_fn(require_permission(P::ViewWorkers))),
        )
        .route(
            "/api/v1/admin/workers/pending",
            get(admin::workers_pending)
                .route_layer(from_fn(require_permission(P::ViewWorkers))),
        )
        // §2 worker license verify / reject — VerifyWorkers.
        .route(
            "/api/v1/admin/workers/{id}/verify",
            post(admin::verify_worker)
                .route_layer(from_fn(require_permission(P::VerifyWorkers))),
        )
        .route(
            "/api/v1/admin/workers/{id}/reject",
            post(admin::reject_worker)
                .route_layer(from_fn(require_permission(P::VerifyWorkers))),
        )
        // §2 worker suspend / unsuspend — SuspendWorkers.
        .route(
            "/api/v1/admin/workers/{id}/suspend",
            post(admin::suspend_worker)
                .route_layer(from_fn(require_permission(P::SuspendWorkers))),
        )
        .route(
            "/api/v1/admin/workers/{id}/unsuspend",
            post(admin::unsuspend_worker)
                .route_layer(from_fn(require_permission(P::SuspendWorkers))),
        )
        // §3 platform-wide shifts — ViewShifts / CancelShifts.
        .route(
            "/api/v1/admin/shifts",
            get(admin::list_admin_shifts)
                .route_layer(from_fn(require_permission(P::ViewShifts))),
        )
        .route(
            "/api/v1/admin/shifts/{id}/cancel",
            post(admin::cancel_admin_shift)
                .route_layer(from_fn(require_permission(P::CancelShifts))),
        )
        // §2 metrics / analytics — ViewAnalytics.
        .route(
            "/api/v1/admin/metrics/dashboard",
            get(admin::metrics_dashboard)
                .route_layer(from_fn(require_permission(P::ViewAnalytics))),
        )
        .route(
            "/api/v1/admin/metrics/shifts/volume",
            get(admin::metrics_shift_volume)
                .route_layer(from_fn(require_permission(P::ViewAnalytics))),
        )
        .route(
            "/api/v1/admin/metrics/geographic",
            get(admin::metrics_geographic)
                .route_layer(from_fn(require_permission(P::ViewAnalytics))),
        )
        .route(
            "/api/v1/admin/metrics/workers/performance",
            get(admin::metrics_worker_performance)
                .route_layer(from_fn(require_permission(P::ViewAnalytics))),
        )
        .route(
            "/api/v1/admin/metrics/ai/usage",
            get(admin::metrics_ai_usage)
                .route_layer(from_fn(require_permission(P::ViewAnalytics))),
        )
        // §5 revenue metric — ViewEarnings (financial).
        .route(
            "/api/v1/admin/metrics/revenue",
            get(admin::metrics_revenue)
                .route_layer(from_fn(require_permission(P::ViewEarnings))),
        )
        // §4 disputes — ViewDisputes / ResolveDisputes.
        .route(
            "/api/v1/admin/disputes",
            get(admin::list_disputes)
                .route_layer(from_fn(require_permission(P::ViewDisputes))),
        )
        .route(
            "/api/v1/admin/disputes/{id}/resolve",
            post(admin::resolve_dispute)
                .route_layer(from_fn(require_permission(P::ResolveDisputes))),
        )
        // §5 payments — ViewEarnings (failed) / ProcessPayouts (manual).
        .route(
            "/api/v1/admin/payments/failed",
            get(admin::payments_failed)
                .route_layer(from_fn(require_permission(P::ViewEarnings))),
        )
        .route(
            "/api/v1/admin/payments/manual",
            post(admin::manual_payout)
                .route_layer(from_fn(require_permission(P::ProcessPayouts))),
        )
        // §7 reports — GenerateReports (financial type checked in-handler).
        .route(
            "/api/v1/admin/reports/generate",
            post(admin::reports_generate)
                .route_layer(from_fn(require_permission(P::GenerateReports))),
        )
        // §8 settings — ManageSettings (super only).
        .route(
            "/api/v1/admin/settings",
            get(admin::get_settings)
                .put(admin::update_settings)
                .route_layer(from_fn(require_permission(P::ManageSettings))),
        )
        // §1 admin management — ManageAdmins (super only).
        .route(
            "/api/v1/admin/admins",
            post(admin::create_admin)
                .get(admin::list_admins)
                .route_layer(from_fn(require_permission(P::ManageAdmins))),
        )
        .route(
            "/api/v1/admin/admins/{id}",
            axum::routing::patch(admin::update_admin)
                .route_layer(from_fn(require_permission(P::ManageAdmins))),
        )
        // Detail views — ViewHospitals / ViewWorkers.
        .route(
            "/api/v1/admin/hospitals/{hospital_id}",
            get(admin::get_hospital_detail)
                .route_layer(from_fn(require_permission(P::ViewHospitals))),
        )
        .route(
            "/api/v1/admin/workers/{clinician_id}",
            get(admin::get_worker_detail)
                .route_layer(from_fn(require_permission(P::ViewWorkers))),
        )
        // Revenue trend (time series) — ViewEarnings (financial).
        .route(
            "/api/v1/admin/metrics/revenue/trend",
            get(admin::metrics_revenue_trend)
                .route_layer(from_fn(require_permission(P::ViewEarnings))),
        )
        // Recent activity feed — ViewAnalytics.
        .route(
            "/api/v1/admin/activities",
            get(admin::recent_activities)
                .route_layer(from_fn(require_permission(P::ViewAnalytics))),
        )
        // Global search (hospitals + workers) — ViewHospitals.
        .route(
            "/api/v1/admin/search",
            get(admin::global_search)
                .route_layer(from_fn(require_permission(P::ViewHospitals))),
        )
}
