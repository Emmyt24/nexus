use axum::{
    middleware::from_fn,
    routing::{delete, get, patch, post},
    Router,
};

use crate::middlewares::require_role;
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
    admin, auth, clinician_registration, distance, earnings, health, here_maps, hospitals,
    identity, location, patients, pipeline, registration, shifts, wallet, webhooks,
};
use crate::repositories::{
    audit::AuditRepository, billing::BillingRepository, clinician::ClinicianRepository,
    hospital::HospitalRepository, identity_verification::IdentityVerificationRepository,
    location::LocationRepository, patient::PatientRepository,
    patient_prediction::PatientPredictionRepository, shift::ShiftRepository,
    wallet::WalletRepository,
};
use crate::services::{
    audit_service::AuditService, auth_service::AuthService,
    clinician_registration_service::ClinicianRegistrationService,
    distance_service::DistanceService, email_outbox_service::EmailOutboxService,
    encryption::EncryptionService, geocoding::GeocodingClient, here_maps::HereMapsClient,
    identity_verification_service::IdentityVerificationService, location_service::LocationService,
    ml_client::MlClient, notification_service::NotificationService,
    patient_prediction_service::PatientPredictionService, payout_service::PayoutService,
    registration_service::RegistrationService, safehaven::SafeHavenClient,
    shift_service::ShiftService, wallet_service::WalletService,
};

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
    pub identity_service: Arc<IdentityVerificationService>,
    pub safehaven: Arc<SafeHavenClient>,
    pub here_maps_client: Arc<HereMapsClient>,
    pub distance_service: Arc<DistanceService>,
    pub patient_repo: Arc<PatientRepository>,
    pub patient_prediction_service: Arc<PatientPredictionService>,
    pub pipeline_events: Arc<tokio::sync::broadcast::Sender<crate::models::patient_prediction::PipelineEvent>>,
}

#[derive(OpenApi)]
#[openapi(
    paths(
        crate::handlers::health::health_check,
        crate::handlers::health::db_health_check,
        // Auth
        crate::handlers::auth::email_otp_send,
        crate::handlers::auth::email_otp_verify,
        crate::handlers::auth::me,
        crate::handlers::auth::refresh_token,
        crate::handlers::auth::logout,
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
        // Patients / ML pipeline
        crate::handlers::patients::ingest_patient,
        crate::handlers::patients::get_patient,
        crate::handlers::pipeline::pipeline_events,
        // Wallet
        crate::handlers::wallet::get_wallet,
        crate::handlers::wallet::get_ledger,
        crate::handlers::wallet::create_deposit,
        crate::handlers::wallet::list_deposits,
        crate::handlers::wallet::initiate_sub_account,
        crate::handlers::wallet::provision_sub_account,
        crate::handlers::wallet::list_payouts,
        crate::handlers::wallet::get_payout_status,
        crate::handlers::wallet::get_statement,
        crate::handlers::wallet::retry_payout,
        // Webhooks
        crate::handlers::webhooks::safehaven_webhook,
        // Earnings
        crate::handlers::earnings::get_earnings,
    ),
    components(
        schemas(
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
            crate::models::shift::MyApplicationEntry,
            crate::models::shift::ClockinApprovalRequest,
            crate::models::shift::ClockinApprovalDecisionRequest,
            crate::models::shift::ClockinApprovalRecord,
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
            crate::handlers::wallet::LedgerPage,
            crate::handlers::wallet::PayoutPage,
            crate::handlers::wallet::PayoutStatusResponse,
            crate::handlers::wallet::ProvisionSubAccountRequest,
            crate::handlers::wallet::SubAccountStatusResponse,
            crate::handlers::wallet::PayoutRetryResponse,
            crate::services::payout_service::PayoutRow,
            crate::handlers::earnings::EarningsSummary,
            crate::handlers::earnings::EarningsTransaction,
            // Admin
            crate::handlers::admin::ClinicianListResponse,
            crate::handlers::admin::PaginationMetadata,
            crate::handlers::admin::ListCliniciansQuery,
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
            crate::models::user::LoginResponse,
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
        (url = "http://localhost:8080", description = "Local development"),
        (url = "https://api.nexuscare.com", description = "Production")
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
        (name = "webhooks", description = "Inbound webhooks from external providers (SafeHaven)"),
        (name = "earnings", description = "Worker earnings — totals + transaction history"),
        (name = "identity", description = "BVN/NIN identity verification and bank list"),
        (name = "patients", description = "Patient intake and async ML triage pipeline (diagnosis, risk, recommendation, routing)")
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

    let shift_service = Arc::new(ShiftService::new(
        shift_repo,
        pool.clone(),
        notification_service.clone(),
        email_outbox_service.clone(),
        wallet_service.clone(),
    ));

    // Initialize payout service. Borrows the wallet repo so it can
    let payout_service = Arc::new(PayoutService::new(
        pool.clone(),
        wallet_repo.clone(),
        clinician_repo.clone(),
        safehaven_client.clone(),
        encryption_service.clone(),
    ));

    // ML pipeline: patient intake -> ml-service -> SSE. An empty
    // ML_SERVICE_URL flips MlClient into mock mode (mirrors SafeHavenClient).
    let ml_client = Arc::new(MlClient::from_env());
    let (pipeline_tx, _pipeline_rx) =
        tokio::sync::broadcast::channel::<crate::models::patient_prediction::PipelineEvent>(256);
    let pipeline_events = Arc::new(pipeline_tx);
    let patient_prediction_service = Arc::new(PatientPredictionService::new(
        pool.clone(),
        patient_repo.clone(),
        patient_prediction_repo,
        ml_client,
        pipeline_events.clone(),
    ));

    let state = AppState {
        pool: pool.clone(),
        registration_service,
        clinician_registration_service,
        auth_service,
        shift_service,
        wallet_service,
        payout_service,
        clinician_repo: clinician_repo.clone(),
        identity_service,
        safehaven: safehaven_client.clone(),
        here_maps_client,
        distance_service,
        patient_repo,
        patient_prediction_service,
        pipeline_events,
    };

    let api_router = Router::new()
        .route("/health", get(health::health_check))
        .route("/health/db", get(health::db_health_check))
        // Auth (OTP-only).
        .route("/api/v1/auth/otp/send", post(auth::email_otp_send))
        .route("/api/v1/auth/otp/verify", post(auth::email_otp_verify))
        .route("/api/v1/auth/refresh", post(auth::refresh_token))
        .route("/api/v1/auth/logout", post(auth::logout))
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
        // Admin endpoints
        .route(
            "/api/v1/admin/hospitals/{hospital_id}/approve",
            post(registration::approve_hospital),
        )
        .route(
            "/api/v1/admin/hospitals/{hospital_id}/reject",
            post(registration::reject_hospital),
        )
        .route("/api/v1/admin/hospitals", get(admin::list_hospitals_admin))
        .route(
            "/api/v1/admin/clinicians",
            get(admin::list_clinicians_admin),
        )
        // Existing Hospitals endpoints (legacy - for backward compatibility)
        .route("/api/v1/hospitals/create", post(hospitals::create_hospital))
        .route("/api/v1/hospitals/{id}", get(hospitals::get_hospital))
        .route("/api/v1/hospitals/{id}", patch(hospitals::update_hospital))
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
        .route(
            "/api/v1/clinicians/{clinician_id}/profile",
            axum::routing::put(clinician_registration::complete_profile),
        )
        .route(
            "/api/v1/clinicians/{clinician_id}/bank-account",
            post(clinician_registration::add_bank_account),
        )
        // Identity verification (BVN/NIN) + bank list
        .route(
            "/api/v1/hospitals/{hospital_id}/identity/initiate",
            post(identity::hospital_initiate),
        )
        .route(
            "/api/v1/hospitals/{hospital_id}/identity/validate",
            post(identity::hospital_validate),
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
            post(wallet::retry_payout).route_layer(from_fn(require_role(&[UserRole::SuperAdmin]))),
        )
        // ---- Webhooks — authenticated by HMAC signature, not JWT.
        .route(
            "/api/v1/webhooks/safehaven",
            post(webhooks::safehaven_webhook),
        )
        // ---- Worker earnings — HealthWorker only.
        .route(
            "/api/v1/worker/earnings",
            get(earnings::get_earnings)
                .route_layer(from_fn(require_role(&[UserRole::HealthWorker]))),
        )
        // ---- Patient intake + ML pipeline — HealthWorker and HospitalAdmin.
        .route(
            "/api/v1/ingest/patient",
            post(patients::ingest_patient).route_layer(from_fn(require_role(&[
                UserRole::HealthWorker,
                UserRole::HospitalAdmin,
            ]))),
        )
        .route(
            "/api/v1/patients/{id}",
            get(patients::get_patient).route_layer(from_fn(require_role(&[
                UserRole::HealthWorker,
                UserRole::HospitalAdmin,
            ]))),
        )
        .route(
            "/api/v1/pipeline/events",
            get(pipeline::pipeline_events).route_layer(from_fn(require_role(&[
                UserRole::HealthWorker,
                UserRole::HospitalAdmin,
            ]))),
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
