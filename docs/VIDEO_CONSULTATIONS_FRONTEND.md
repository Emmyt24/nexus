# Video consultations — frontend integration

Everything the app needs to run a virtual shift's consultation. The backend
contract is five endpoints; four of them you call, one is LiveKit calling us.

Live spec: `GET /api/openapi.json` · Swagger UI: `GET /api/docs` (tag **video**).

---

## The whole flow, in one paragraph

Call `POST /api/v1/shifts/{shift_id}/consult/token`, hand the `url` and `token`
it returns straight to the LiveKit client SDK, and you are in the call. Joining
is what clocks the worker in — you do not call the clock-in endpoint on the
happy path. Everything else in the response is UI state.

```ts
import { Room } from 'livekit-client';   // npm i livekit-client

const res  = await fetch(`/api/v1/shifts/${shiftId}/consult/token`, {
  method: 'POST',
  headers: { Authorization: `Bearer ${jwt}`, 'Content-Type': 'application/json' },
  body: JSON.stringify({}),              // the body is required; {} is fine
});
const consult = await res.json();

const room = new Room({ adaptiveStream: true, dynacast: true });
await room.connect(consult.url, consult.token);
if (consult.can_publish) await room.localParticipant.enableCameraAndMicrophone();
```

You never see the LiveKit API key. Tokens are minted server-side, scoped to one
room and one identity, and expire.

---

## `POST /api/v1/shifts/{shift_id}/consult/token`

Roles: `HealthWorker`, `HospitalAdmin`. The service then checks that the caller
is *this* shift's assigned clinician or an admin of *this* shift's hospital.

Request — every field optional:

| Field | Type | Default | Notes |
|---|---|---|---|
| `mode` | `"participant"` \| `"observer"` | `"participant"` | `"observer"` is hospital-admin only; a worker sending it gets 403, not a downgrade |
| `device_label` | string | `null` | Free text, audit trail only |

`200 OK`:

```json
{
  "session_id": "8f1c…",
  "room_name": "shift-3b2a…",
  "url": "wss://nexuscare-xyz.livekit.cloud",
  "token": "eyJhbGciOiJIUzI1NiJ9…",
  "identity": "u:9d4e…",
  "display_name": "Dr. Amina Bello",
  "participant_role": "clinician",
  "mode": "participant",
  "can_publish": true,
  "can_subscribe": true,
  "expires_at": "2026-08-21T10:15:00Z",
  "session_status": "pending",
  "shift": {
    "id": "3b2a…", "role_title": "Emergency Doctor",
    "hospital_name": "Lagos General",
    "scheduled_start": "2026-08-21T10:00:00Z",
    "scheduled_end":   "2026-08-21T14:00:00Z",
    "status": "assigned", "shift_type": "virtual"
  },
  "clock_in": {
    "mode": "auto_on_join",
    "already_clocked_in": false,
    "clocked_in_at": null,
    "fallback_endpoint": "/api/v1/shifts/3b2a…/clockin"
  },
  "recording": { "enabled": false, "status": null },
  "mock": false
}
```

Three fields decide how your UI behaves:

- **`expires_at` is a join deadline, not a call deadline.** LiveKit validates the
  token only at `connect()`. Once connected, the call outlives it. If the user
  sits on the pre-join screen past it, just call this endpoint again — it is
  idempotent and safe to call any number of times.
- **`clock_in.mode`** is `"auto_on_join"` when the backend will clock the worker
  in from the LiveKit webhook, or `"manual"` when that mapping is switched off
  (`LIVEKIT_VIRTUAL_CLOCKIN_ENABLED=false`, which is the shipping default).
  On `"manual"`, show the normal clock-in button instead of "clocking you in…".
- **`mock: true`** means the backend has no LiveKit credentials — local dev or
  CI. The token is fake. Do not attempt to connect unless you are pointing at a
  local `livekit-server --dev`.

Errors: `401` no/expired JWT · `403` not the assigned clinician, wrong hospital,
or `observer` requested by a worker · `404` shift not found · `409` not a virtual
shift, status not joinable, outside the consultation window, or already ended ·
`500` LiveKit unavailable.

All errors share the platform shape: `{"error":{"message":"…","status":409}}`.

### The consultation window

A token is only minted from **60 minutes before `scheduled_start` to 60 minutes
after `scheduled_end`**, and only while the shift is `assigned`, `upcoming`, or
`in_progress`. Outside that you get `409`. This mirrors the clock-in rules
exactly — a token minted earlier would produce a join that cannot clock anyone
in. Gate the "Join consultation" button on the same window so the user never
sees the 409.

---

## `GET /api/v1/shifts/{shift_id}/consult`

Roles: `HealthWorker`, `HospitalAdmin`, `SuperAdmin`, `OperationsAdmin`. Platform
admins get metadata only and can never obtain a token.

```json
{
  "session_id": "8f1c…", "shift_id": "3b2a…", "room_name": "shift-3b2a…",
  "status": "active",
  "started_at": "2026-08-21T10:02:11Z", "ended_at": null, "ended_reason": null,
  "live": true,
  "clock_in_recorded": true,
  "participants": [
    { "identity": "u:9d4e…", "display_name": "Dr. Amina Bello",
      "participant_role": "clinician", "connected": true,
      "joined_at": "2026-08-21T10:02:11Z", "left_at": null,
      "is_publisher": true, "clocked_in_at": "2026-08-21T10:02:12Z" }
  ],
  "recording": { "enabled": false, "status": null }
}
```

`status` is `pending` → `active` → `ended` and only ever moves forward.

`"live": true` means the participant list was reconciled against LiveKit on this
request. `"live": false` means LiveKit was unreachable and you are looking at the
last webhook-fed state, which may lag a few seconds — worth a subtle
"reconnecting" hint, not an error.

`404` only if nobody has ever requested a token for this shift. Once a token has
been issued you get `200` with `status: "pending"` and an empty-ish participant
list, so the pre-join screen never has to handle a 404.

### The clock-in handshake

Joining is the clock-in, but it arrives via a webhook from LiveKit, so it lands a
beat after `connect()` resolves:

1. `connect()` resolves → show "clocking you in…".
2. Poll `GET …/consult` once about **5 s** later; `clock_in_recorded: true` means
   done.
3. If it is still `false` **30 s** after connecting, a webhook was lost. Fall back
   to `POST /api/v1/shifts/{shift_id}/clockin` with `{"method":"virtual"}` — the
   existing endpoint, kept permanently as the documented fallback.

A rejoin after a dropped connection will **not** move `clocked_in_at`, so it is
safe to reconnect as often as you like.

---

## `POST /api/v1/shifts/{shift_id}/consult/leave`

Roles: `HealthWorker`, `HospitalAdmin`. Fire it from the Leave button **and**
`beforeunload`, in addition to `room.disconnect()`. Idempotent, always `200`,
never blocks the UI.

```json
{ "session_id": "8f1c…", "identity": "u:9d4e…", "left_at": "2026-08-21T12:31:04Z",
  "session_status": "active", "remaining_participants": 1 }
```

It does **not** end the call for anyone else and does **not** clock the worker
out. Clock-out still needs a handover, then `POST …/clockout`, as it always has.

For `beforeunload`, `navigator.sendBeacon` is the reliable option.

---

## `POST /api/v1/shifts/{shift_id}/consult/end`

Roles: `HospitalAdmin`, `SuperAdmin`, `OperationsAdmin` — and, for a hospital
admin, only their own hospital's shift. A clinician leaving uses `/leave`, never
`/end`. This disconnects everyone.

Request `{ "reason": "Consultation complete" }` (optional) → `200 OK`:

```json
{ "session_id": "8f1c…", "status": "ended", "ended_at": "2026-08-21T12:45:00Z",
  "ended_reason": "ended_by_hospital", "clock_out_required": true,
  "clock_out_hint": "The clinician must submit a handover, then POST /api/v1/shifts/{shift_id}/clockout" }
```

Idempotent: ending an already-ended session returns `200` with the **original**
`ended_at`. Ending the room does not clock the worker out.

Once a session is `ended`, `POST …/consult/token` returns `409`. There is no
rejoining an ended consultation — create a new shift if the call needs to resume.

---

## `POST /api/v1/webhooks/livekit`

LiveKit Cloud calls this, not you. Listed only so nobody wires the app at it.
Configure the URL in the LiveKit console under Settings → Webhooks.

---

## Not in this release

`recording` is always `{ "enabled": false, "status": null }`. It ships now so the
recording indicator can be built against a stable shape, but nothing records yet
and no token ever carries a record grant. Ad-hoc (non-shift) consults are not
built either — every session today belongs to a shift.

---

## Checklist before you call it done

- [ ] "Join" is gated on the ±60-minute window and on `shift_type === "virtual"`
- [ ] `mock: true` shows a dev banner instead of attempting `connect()`
- [ ] `clock_in.mode === "manual"` shows the clock-in button instead of the
      "clocking you in…" spinner
- [ ] The 5 s poll and the 30 s manual fallback are both wired
- [ ] `/leave` fires from the Leave button *and* `beforeunload`
- [ ] `/end` is only rendered for hospital admins
- [ ] `expires_at` on the pre-join screen re-requests a token rather than erroring
- [ ] `live: false` degrades to a hint, not an error state

## Local testing

`dev/consult-tester.html` in this repo is a single-file harness that does exactly
the flow above — paste a JWT and a shift ID and it mints a token, connects, shows
the tiles, and polls the session. Serve it with any static server and point it at
your local API. See the "Trying it locally" section of the PR description for the
setup, including how to get a JWT without a mailbox.
