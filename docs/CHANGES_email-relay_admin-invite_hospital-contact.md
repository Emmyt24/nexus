# New Implementation — Email Relay, Sub-Admin Invites, Hospital Contact

Covers three changes:
1. Generic frontend-templated email relay endpoint
2. Sub-admin invite email on creation
3. Hospital "contact person" now returned by the get-hospital endpoints (fixes the frontend "unknown user")

Base URL (local): `http://localhost:8080` · All bodies are `application/json` · Auth = `Authorization: Bearer <JWT>`.

---

## 1. Generic email relay — `POST /api/v1/emails/send`

Lets the **frontend own the email template** (subject + HTML/text) and hand it to the backend to deliver. The backend queues it onto the same outbox → SMTP pipeline used by system emails. Backend-triggered emails (login OTP, shift/payout notices) are unchanged and stay backend-templated.

**Auth:** required — any valid bearer token. Without one → `401` (prevents an open mail relay).

**Request body (`SendEmailRequest`):**
| Field | Type | Required | Notes |
|---|---|---|---|
| `to` | string (email) | ✅ | Recipient; validated as an email address |
| `subject` | string | ✅ | 1–255 chars |
| `html` | string | ⬦ | Rendered HTML body (frontend template) |
| `text` | string | ⬦ | Plain-text body |

⬦ At least one of `html` / `text` is required. If only one is supplied, the other is derived automatically (text is wrapped in `<pre>` for HTML; HTML is copied into text).

**Responses:**
- `202 Accepted` — queued:
  ```json
  { "queued_id": "1c7d55f5-0f9c-427b-be76-f07819ea2366", "message": "Email queued for delivery" }
  ```
  Delivery happens shortly after via the outbox worker (SMTP). `queued_id` is the outbox row id.
- `401` — missing/invalid token.
- `422` — invalid `to`, empty `subject`, or neither body supplied.

**Example**
```bash
curl -X POST http://localhost:8080/api/v1/emails/send \
  -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{
        "to": "user@example.com",
        "subject": "Welcome to NexusCare",
        "html": "<h1>Hello 👋</h1><p>Your account is ready.</p>"
      }'
```

**Frontend integration**
- Render your template client-side (any templating lib), then POST the final `subject` + `html` (and/or `text`).
- Send the caller's `Authorization: Bearer` header — same token used elsewhere.
- Treat `202` as "queued, not yet delivered"; there is no synchronous send guarantee. If you need delivery status, that is tracked server-side in the `email_outbox` table (not yet exposed via an endpoint).
- Keep HTML self-contained (inline styles); many mail clients strip `<style>`/external assets.

---

## 2. Sub-admin invite email

When a super-admin creates a sub-admin, the new admin now automatically receives an **invite email** with their login link and initial password. No API shape change — this is added behavior on the existing endpoint.

**Endpoint:** `POST /api/v1/admin/admins` · **Auth:** SuperAdmin (`ManageAdmins`).

**Request body (`CreateAdminRequest`):**
```json
{
  "first_name": "New",
  "last_name": "Ops",
  "email": "new.ops@example.com",
  "phone": "+2348011112222",
  "role": "operations_admin",
  "password": "TempPass123"
}
```
- `role` ∈ `operations_admin` | `verification_admin` | `finance_admin` | `super_admin`
- `password` ≥ 8 chars (this is the initial password included in the invite).

**Response:** `200` with the created `AdminSummary` (`id`, `first_name`, `last_name`, `email`, `role`, `is_active`, `created_at`).

**Invite email**
- Subject: *"You've been invited to the NexusCare admin console"*.
- Body contains the admin's email, the temporary password, and a **Sign in** link.
- The link points at `{ADMIN_APP_URL}/admin/login` (falls back to `API_BASE_URL`, then `http://localhost:8080`). Set `ADMIN_APP_URL` in the environment to your admin-console URL so invites link to the right place.
- Sending is **best-effort**: if SMTP is down the admin is still created (the failure is logged, not surfaced), so the create call won't fail because of email.
- The new admin signs in at `POST /api/v1/auth/admin/login` with `{ email, password }`.

**New env var**
| Var | Default | Purpose |
|---|---|---|
| `ADMIN_APP_URL` | `API_BASE_URL` → `http://localhost:8080` | Base URL used to build the invite's sign-in link |

---

## 3. Hospital contact person on get-hospital endpoints

Fixes the frontend showing **"unknown user"** for a hospital's contact/admin. Previously the contact name was sourced from the admin `users` row, which does not exist until the hospital is **approved**, and the public get-hospital endpoint returned no contact at all. Both endpoints now return the contact sourced from the hospital record itself (captured at registration), so it is populated **before and after** approval.

Registration is unchanged and confirmed to persist the hospital (insert + commit), including `admin_first_name` / `admin_last_name`.

### `GET /api/v1/hospitals/{id}` (public detail) — new fields
Added to the response:
| Field | Type | Source |
|---|---|---|
| `admin_first_name` | string \| null | hospital record |
| `admin_last_name` | string \| null | hospital record |
| `contact_email` | string | hospital email |
| `contact_phone` | string | hospital phone |

(All previously-existing fields are unchanged; these are additive.)

### `GET /api/v1/admin/hospitals/{id}` (admin detail) — behavior fix
Already returned `admin_first_name` / `admin_last_name` / `admin_email` / `admin_phone`, but they were null before approval. Now they COALESCE the admin `users` row with the hospital's own contact columns, so they are always populated. No response-shape change.

**Frontend integration**
- Render the contact from `admin_first_name` + `admin_last_name` (fall back to `contact_email`). These are present regardless of approval state, so the "unknown user" fallback should no longer trigger.
- `contact_email` / `contact_phone` (public endpoint) and `admin_email` / `admin_phone` (admin endpoint) give the contact's email/phone.

---

## Notes
- No database migration is required for any of these changes.
- Full request/response schemas are also in Swagger: `GET /api/docs` (spec at `GET /api/openapi.json`), tags `emails`, `admin`, `hospitals`.
