# Endpoints added on `feat/platform-fee-ratings-search-shift-timeout`

The authoritative contract is the generated OpenAPI document, not this file.
Read it at `/api/docs` (Swagger UI) or `/api/openapi.json` on a running server,
or regenerate it without a server or a database:

```bash
cargo run --bin openapi > docs/openapi.json
```

`docs/openapi.json` in this repo is a snapshot of that output. Generate types
from it rather than hand-writing them, and if it ever contradicts this file,
trust the spec and tell us.

Spec totals at the time of writing: **137 paths, 149 operations, 225 schemas**,
one security scheme (`bearerAuth`), applied globally — an endpoint opts out with
an empty `security()`, which only the handoff-redeem and a few legacy
registration routes do.

## New operations (10)

| Method | Path | Auth | Notes |
|---|---|---|---|
| `GET` | `/api/v1/platform/fee` | hospital_admin, health_worker, super_admin | The fee a hospital needs before committing money. `platform_fee_flat_naira` is **naira**; `platform_fee_cap_kobo` is kobo. Deducted from the clinician's payout, not added to the hospital's bill. |
| `GET` | `/api/v1/search` | hospital_admin, super_admin | Hospital-scoped search over shifts, clinicians, patients and handovers. Tenant comes from the token, never a parameter. Clinicians are limited to those with a working relationship to this hospital and never carry an email or licence number; patients match on name only. |
| `GET` | `/api/v1/clinicians/{clinician_id}/ratings` | hospital_admin, health_worker (own only), super_admin, operations_admin | Ratings written about a clinician, with the overall average and total. `rater_name` is null for anonymous ratings, which is the default. A health worker may read only their own feed. |
| `GET` | `/api/v1/hospitals/{hospital_id}/ratings` | hospital_admin, health_worker, super_admin, operations_admin | Ratings about a hospital, including the four sub-scores in `dimensions`. Open to any authenticated caller — a clinician weighing a shift needs it. |
| `POST` | `/api/v1/hospitals/{hospital_id}/documents` | hospital_admin (own), super_admin | Stores metadata for a file already uploaded via `GET /uploads/signature?kind=hospital_document`. One document per `document_type`: a re-upload replaces the file and returns it to `draft`, clearing any earlier review. |
| `GET` | `/api/v1/hospitals/{hospital_id}/documents` | hospital_admin (own), super_admin | Documents on file with their review state. `reviewed_by` is not returned. |
| `POST` | `/api/v1/hospitals/{hospital_id}/documents/submit` | hospital_admin (own), super_admin | Moves every `draft` to `under_review`. Already-decided documents keep their decision, so fixing one file does not reopen the others. |
| `DELETE` | `/api/v1/hospitals/{hospital_id}/documents/{document_id}` | hospital_admin (own), super_admin | Only while `draft`. Returns 409 once submitted — it is then part of the compliance record. |
| `GET` | `/api/v1/admin/hospitals/{hospital_id}/documents` | `ViewHospitals` | The full rows, not the single derived `documents_status` string on `GET /admin/hospitals/{id}`. |
| `PATCH` | `/api/v1/admin/hospitals/{hospital_id}/documents/{document_id}` | `VerifyHospitals` | Approve or reject. A rejection must carry `review_notes`; a reviewer cannot write a document back to `draft`. |

## Changed operations (2)

| Method | Path | Change |
|---|---|---|
| `POST` | `/api/v1/shifts/preview` | Adds `platform_fee_kobo` and `worker_net_kobo`. Both in **kobo**. The fee is snapshotted onto the shift at creation, so the previewed figure is the one that settles. Also now applies the STAT auto-bonus that creation applies, so a STAT shift's `grand_total_kobo` no longer previews 20% low. |
| `GET` | `/api/v1/worker/shifts/nearby` | Adds `require_qualification_match` (optional, default false). Hides shifts whose requirements the caller's saved qualifications do not cover. Opt-in because requirements are free text with no shared vocabulary; a shift with no requirements always matches. |

## Changed schemas

- `PlatformSettings` / `UpdatePlatformSettings` — `platform_fee_mode` (`flat` | `percent`), `platform_fee_flat_naira` (**naira**), `shift_close_grace_minutes`.
- `ShiftPreviewResponse` — `platform_fee_kobo`, `worker_net_kobo`.
- `RateWorkerRequest` / `RateHospitalRequest` — `is_anonymous` (defaults true).
- `EditRatingRequest` — `clear_comment`. Sending it together with `comment` is a 422, not a silent discard.
- New: `PlatformFeeResponse`, `HospitalSearchResults`, `HospitalSearchHit`, `RatingFeed`, `RatingFeedItem`, `UploadDocumentRequest`, `HospitalDocumentResponse`, `ReviewDocumentRequest`, `DocumentType`, `IssuingAuthority`, `SubmissionStatus`, `SubmitForReviewResponse`.

## One unit trap

Every monetary field in this API is kobo **except** `platform_fee_flat_naira`,
which is naira because it is the figure an admin types. The name carries the
unit for that reason, and a test asserts an unqualified `platform_fee_flat`
never appears.

## Adding an endpoint here

Four steps, and missing the third compiles clean while leaving the route
invisible in the spec — which is what `b3eca25` existed to fix, for four routes
at once:

1. the handler with a `#[utoipa::path(...)]` attribute;
2. the `.route(...)` in `create_router` with its `require_role` / `require_permission`;
3. the handler in `ApiDoc`'s `paths(...)`, and any new DTO in `components(schemas(...))`;
4. the service-level tenancy check — the route guard cannot know which tenant a
   path parameter refers to.

`params(...)` lists are written out by hand and do **not** read the query
struct, so a new query field needs adding there too.

The `openapi_tests` module in `src/routes/app_routes.rs` checks all of this
without a database: that the routes are registered, that every security
requirement names a registered scheme, that no `$ref` dangles, that the new
operations carry a summary, description and responses, and that the nearby
params match their handler. CI runs neither the tests nor any spec check, so
run `cargo test --lib openapi_tests` before opening a PR.
