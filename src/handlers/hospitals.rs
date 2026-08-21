use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use uuid::Uuid;
use validator::Validate;

use crate::{
    models::hospital::{
        CreateHospitalRequest, Hospital, HospitalPublicDetail, HospitalResponse, RegistrationStep,
        UpdateHospitalRequest, VerificationStatus,
    },
    routes::AppState,
    utils::errors::{AppError, AppResult},
};

/// POST /api/v1/hospitals

pub async fn create_hospital(
    State(state): State<AppState>,
    Json(payload): Json<CreateHospitalRequest>,
) -> AppResult<(StatusCode, Json<HospitalResponse>)> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    // Check for duplicate registration number or email
    let existing: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM hospitals WHERE registration_number = $1 OR email = $2")
            .bind(&payload.registration_number)
            .bind(&payload.email)
            .fetch_optional(&state.pool)
            .await?;

    if existing.is_some() {
        return Err(AppError::Conflict(
            "A hospital with this registration number or email already exists".to_string(),
        ));
    }

    let hospital: Hospital = sqlx::query_as(
        r#"
        INSERT INTO hospitals (id, name, registration_number, email, address, phone_number,
                               verification_status, registration_step)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING id, name, registration_number, email, address, phone_number,
                  verification_status, registration_step, admin_registration_status,
                  approved_by, approved_at, rejection_reason, admin_user_id,
                  legal_submitted_at, setup_progress_percent, logo_url,
                  created_at, updated_at
        "#,
    )
    .bind(Uuid::new_v4())
    .bind(&payload.name)
    .bind(&payload.registration_number)
    .bind(&payload.email)
    .bind(&payload.address)
    .bind(&payload.phone_number)
    .bind(VerificationStatus::Pending)
    .bind(RegistrationStep::ProfileSetup)
    .fetch_one(&state.pool)
    .await?;

    Ok((StatusCode::CREATED, Json(HospitalResponse::from(hospital))))
}

/// GET /api/v1/hospitals/:id
/// Public (ungated) hospital profile enriched with ratings, shift counts,
/// documents status and payment-method presence. Sensitive wallet/contact/spend
/// data is intentionally excluded here and served only from the admin detail.
#[utoipa::path(
    get,
    path = "/api/v1/hospitals/{id}",
    tag = "hospitals",
    params(("id" = Uuid, Path, description = "Hospital id")),
    responses(
        (status = 200, description = "Hospital public profile", body = HospitalPublicDetail),
        (status = 404, description = "Hospital not found")
    )
)]
pub async fn get_hospital(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<HospitalPublicDetail>> {
    let hospital: Option<HospitalPublicDetail> = sqlx::query_as(
        r#"
        SELECT
            h.id,
            h.name,
            h.registration_number,
            h.email,
            h.address,
            h.phone_number,
            h.verification_status::TEXT           AS verification_status,
            h.registration_step::TEXT             AS registration_step,
            h.setup_progress_percent,
            h.logo_url,
            h.created_at,
            h.admin_first_name,
            h.admin_last_name,
            h.email                               AS contact_email,
            h.phone_number                        AS contact_phone,
            (SELECT COUNT(*) FROM shifts s WHERE s.hospital_id = h.id)::BIGINT AS total_shifts,
            (SELECT COUNT(*) FROM shifts s WHERE s.hospital_id = h.id
                AND s.status IN ('open','assigned','in_progress'))::BIGINT     AS active_shifts,
            (SELECT COUNT(*) FROM shifts s WHERE s.hospital_id = h.id
                AND s.status = 'completed')::BIGINT                            AS completed_shifts,
            EXISTS (SELECT 1 FROM identity_verifications iv
                WHERE iv.owner_type = 'hospital' AND iv.owner_id = h.id
                  AND iv.status = 'verified')                                  AS identity_verified,
            CASE
                WHEN EXISTS (SELECT 1 FROM hospital_documents d WHERE d.hospital_id = h.id
                    AND d.submission_status = 'approved') THEN 'verified'
                WHEN EXISTS (SELECT 1 FROM hospital_documents d WHERE d.hospital_id = h.id
                    AND d.submission_status = 'under_review') THEN 'under_review'
                WHEN EXISTS (SELECT 1 FROM hospital_documents d WHERE d.hospital_id = h.id)
                    THEN 'submitted'
                ELSE 'unverified'
            END                                                               AS documents_status,
            EXISTS (SELECT 1 FROM hospital_wallets w WHERE w.hospital_id = h.id
                AND w.safehaven_account_number IS NOT NULL)                   AS payment_method_on_file,
            r.avg_rating                          AS average_rating,
            COALESCE(r.rating_count, 0)::BIGINT   AS rating_count,
            r.avg_staff_support                   AS rating_staff_support,
            r.avg_equipment_availability          AS rating_equipment_availability,
            r.avg_communication                   AS rating_communication,
            r.avg_payment_timeliness              AS rating_payment_timeliness
        FROM hospitals h
        LEFT JOIN LATERAL (
            SELECT
                AVG(sr.score)::FLOAT8                                     AS avg_rating,
                COUNT(*)                                                  AS rating_count,
                AVG((sr.dimensions->>'staff_support')::NUMERIC)::FLOAT8   AS avg_staff_support,
                AVG((sr.dimensions->>'equipment_availability')::NUMERIC)::FLOAT8 AS avg_equipment_availability,
                AVG((sr.dimensions->>'communication')::NUMERIC)::FLOAT8   AS avg_communication,
                AVG((sr.dimensions->>'payment_timeliness')::NUMERIC)::FLOAT8 AS avg_payment_timeliness
            FROM shift_ratings sr
            WHERE sr.ratee_kind = 'hospital' AND sr.ratee_id = h.id
        ) r ON TRUE
        WHERE h.id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;

    let hospital =
        hospital.ok_or_else(|| AppError::NotFound(format!("Hospital {} not found", id)))?;
    Ok(Json(hospital))
}

/// PATCH /api/v1/hospitals/:id
pub async fn update_hospital(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(payload): Json<UpdateHospitalRequest>,
) -> AppResult<Json<HospitalResponse>> {
    payload
        .validate()
        .map_err(|e| AppError::Validation(e.to_string()))?;

    let hospital: Option<Hospital> = sqlx::query_as(
        r#"
        UPDATE hospitals
        SET
            name         = COALESCE($2, name),
            email        = COALESCE($3, email),
            address      = COALESCE($4, address),
            phone_number = COALESCE($5, phone_number),
            logo_url     = COALESCE($6, logo_url),
            updated_at   = NOW()
        WHERE id = $1
        RETURNING id, name, registration_number, email, address, phone_number,
                  verification_status, registration_step, admin_registration_status,
                  approved_by, approved_at, rejection_reason, admin_user_id,
                  legal_submitted_at, setup_progress_percent, logo_url,
                  created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(&payload.name)
    .bind(&payload.email)
    .bind(&payload.address)
    .bind(&payload.phone_number)
    .bind(&payload.logo_url)
    .fetch_optional(&state.pool)
    .await?;

    let hospital =
        hospital.ok_or_else(|| AppError::NotFound(format!("Hospital {} not found", id)))?;

    // If exact coordinates were supplied, update the hospital's stored location.
    if let (Some(lat), Some(lng)) = (payload.latitude, payload.longitude) {
        sqlx::query(
            r#"
            UPDATE hospital_locations
               SET latitude = $2, longitude = $3, location_confirmed = TRUE, updated_at = NOW()
             WHERE hospital_id = $1
            "#,
        )
        .bind(id)
        .bind(lat)
        .bind(lng)
        .execute(&state.pool)
        .await?;
    }

    Ok(Json(HospitalResponse::from(hospital)))
}

/// Coordinates + geofence for a hospital (from `hospital_locations`).
#[derive(Debug, serde::Serialize, utoipa::ToSchema, sqlx::FromRow)]
pub struct HospitalLocationResponse {
    pub hospital_id: Uuid,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub place_label: Option<String>,
    pub clock_in_radius_meters: Option<i32>,
    pub location_confirmed: Option<bool>,
}

#[utoipa::path(
    get,
    path = "/api/v1/hospitals/{id}/location",
    params(("id" = Uuid, Path, description = "Hospital id")),
    responses(
        (status = 200, description = "Hospital location/coordinates", body = HospitalLocationResponse),
        (status = 404, description = "No location for this hospital")
    ),
    tag = "hospitals",
    summary = "Get a hospital's coordinates"
)]
pub async fn get_hospital_location(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<HospitalLocationResponse>> {
    let row: Option<HospitalLocationResponse> = sqlx::query_as(
        r#"
        SELECT hospital_id, latitude, longitude, place_label,
               clock_in_radius_meters, location_confirmed
        FROM hospital_locations
        WHERE hospital_id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await?;

    row.map(Json)
        .ok_or_else(|| AppError::NotFound(format!("Location for hospital {id} not found")))
}

/// PATCH /api/v1/hospitals/:id/advance-step

pub async fn advance_registration_step(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<HospitalResponse>> {
    // Fetch current step
    let row: Option<(RegistrationStep,)> =
        sqlx::query_as("SELECT registration_step FROM hospitals WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.pool)
            .await?;

    let (current_step,) =
        row.ok_or_else(|| AppError::NotFound(format!("Hospital {} not found", id)))?;

    let next_step = match current_step {
        RegistrationStep::ProfileSetup => RegistrationStep::Credentials,
        RegistrationStep::Credentials => RegistrationStep::Verification,
        RegistrationStep::Verification => RegistrationStep::AccessGranted,
        RegistrationStep::AccessGranted => {
            return Err(AppError::Conflict(
                "Registration is already complete".to_string(),
            ));
        }
    };

    let hospital: Hospital = sqlx::query_as(
        r#"
        UPDATE hospitals
        SET registration_step = $2, updated_at = NOW()
        WHERE id = $1
        RETURNING id, name, registration_number, email, address, phone_number,
                  verification_status, registration_step, admin_registration_status,
                  approved_by, approved_at, rejection_reason, admin_user_id,
                  legal_submitted_at, setup_progress_percent, logo_url,
                  created_at, updated_at
        "#,
    )
    .bind(id)
    .bind(next_step)
    .fetch_one(&state.pool)
    .await?;

    Ok(Json(HospitalResponse::from(hospital)))
}

/// GET /api/v1/hospitals

pub async fn list_hospitals(
    State(state): State<AppState>,
) -> AppResult<Json<Vec<HospitalResponse>>> {
    let hospitals: Vec<Hospital> = sqlx::query_as(
        r#"
        SELECT id, name, registration_number, email, address, phone_number,
               verification_status, registration_step, logo_url, created_at, updated_at
        FROM hospitals
        ORDER BY created_at DESC
        "#,
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(
        hospitals.into_iter().map(HospitalResponse::from).collect(),
    ))
}
