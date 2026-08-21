# Video consultations — frontend implementation guide

How to build the consultation screen against the backend that shipped in
`20240057_video_consultations`. Part 1 is how to implement it; part 2 is the
endpoint contract it is implemented against.

Live spec: `GET /api/openapi.json` · Swagger UI: `GET /api/docs`, tag **video**.

---

# Part 1 — Implementing it

## What you are building

A health worker assigned to a **virtual** shift, and an admin of that shift's
hospital, join the same video room. The worker joining is what records their
attendance — there is no clock-in button on the happy path.

```
  App                        Nexus API                 LiveKit
   │                             │                        │
   │ POST /consult/token         │                        │
   ├────────────────────────────►│  create room if needed │
   │                             ├───────────────────────►│
   │  { url, token, … }          │                        │
   │◄────────────────────────────┤                        │
   │                             │                        │
   │ room.connect(url, token)    │                        │
   ├──────────────────────────────────────────────────────►│
   │                             │  participant_joined     │
   │                             │◄───────────────────────┤
   │                             │  writes shift_attendance│
   │ GET /consult (poll ~5s)     │                        │
   ├────────────────────────────►│                        │
   │  { clock_in_recorded: true }│                        │
   │◄────────────────────────────┤                        │
```

Two things follow from that shape and drive most of the design below:

1. **The clock-in is asynchronous.** It arrives at the backend from LiveKit,
   not from you, so it lands *after* `connect()` resolves. You poll for it.
2. **You never hold a LiveKit credential.** Tokens are minted server-side,
   scoped to one room and one identity, and expire. If you find yourself
   wanting an API key, something has gone wrong.

## Install

The contract is identical on every platform; only the SDK differs.

| Target | Package | Verified |
|---|---|---|
| Web | `livekit-client` | `^2` (2.22.0 latest; the in-repo harness pins 2.7.2) |
| React Native | `@livekit/react-native` + `@livekit/react-native-webrtc` | 2.12.0 / 144.1.2 |
| Flutter | `livekit_client` | 2.11.0 |

Optional for web: `@livekit/components-react` (2.9.24) gives you prebuilt video
tiles and controls. It is a shortcut for the *rendering*, not for the flow
below — you still own the token call, the clock-in poll and the teardown.

## Types: generate them, don't hand-write them

Every DTO below is in the OpenAPI spec, so the response types the code in this
guide refers to — `JoinConsultResponse`, `ConsultSessionView`,
`ConsultParticipantView`, `EndConsultResponse` — should be generated rather than
transcribed:

```bash
npx openapi-typescript@7 http://localhost:8080/api/openapi.json -o src/api/nexus.d.ts
```

```ts
import type { components } from './api/nexus';
type JoinConsultResponse = components['schemas']['JoinConsultResponse'];
type ConsultSessionView  = components['schemas']['ConsultSessionView'];
```

The generated types carry the backend's own field documentation through as
JSDoc, so the semantics that matter — *"a join deadline, not a call deadline"*,
*"`true` when the backend has no LiveKit credentials"* — show up on hover in
your editor rather than only in this file.

`orval` (8.x) will additionally generate the fetch client and React Query hooks
from the same spec if you want to go further. Either way, regenerate rather than
edit by hand — the spec is the contract, and it is served live by the backend
you are pointing at.

## The state machine

Model the screen explicitly. Most of the bugs in a call UI come from treating
"connected" as a boolean.

```
        idle
          │  user taps Join
          ▼
      requesting          POST /consult/token
          │
          ├─── 403/404/409 ──► blocked   (terminal; show the reason)
          │
          ▼
     ready-to-join         token in hand, not yet connected
          │  (a pre-join screen lives here: device pickers, mirror preview)
          ▼
      connecting           room.connect()
          │
          ├─── failure ────► failed      (retryable: get a fresh token)
          │
          ▼
       connected  ◄──────┐
          │              │ Reconnecting / Reconnected
          │              │ (the SDK handles this; do not tear down)
          ▼              │
     clocking-in ────────┘  poll GET /consult until clock_in_recorded
          │
          ▼
        in-call
          │  user leaves, admin ends, or the room finishes
          ▼
        ended            terminal; no rejoining an ended session
```

`ready-to-join` earns its own state because `expires_at` applies there and
nowhere else — see the clock-in and expiry notes below.

## A reference implementation

Framework-neutral TypeScript. Wrap it in whatever your app uses; the React hook
that follows is a thin shell around it.

```ts
import {
  Room, RoomEvent, Track, ConnectionState,
  type RemoteTrack, type RemoteParticipant, type LocalTrackPublication,
} from 'livekit-client';

export type ConsultPhase =
  | 'idle' | 'requesting' | 'ready' | 'connecting'
  | 'connected' | 'ended' | 'blocked' | 'failed';

export interface ConsultCallbacks {
  onPhase(phase: ConsultPhase, detail?: string): void;
  onTrack(identity: string, track: RemoteTrack | LocalTrackPublication['track'], isLocal: boolean): void;
  onTrackGone(identity: string): void;
  onClockIn(at: string): void;
}

export class ConsultSession {
  private room: Room | null = null;
  private token: JoinConsultResponse | null = null;
  private pollTimer: ReturnType<typeof setInterval> | null = null;

  constructor(
    private readonly shiftId: string,
    private readonly api: { baseUrl: string; jwt: string },
    private readonly cb: ConsultCallbacks,
  ) {}

  private async request<T>(method: string, path: string, body?: unknown): Promise<T> {
    const res = await fetch(`${this.api.baseUrl}/api/v1/shifts/${this.shiftId}/consult${path}`, {
      method,
      headers: {
        Authorization: `Bearer ${this.api.jwt}`,
        ...(body !== undefined ? { 'Content-Type': 'application/json' } : {}),
      },
      ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
    });
    const payload = res.status === 204 ? null : await res.json().catch(() => null);
    if (!res.ok) throw new ConsultError(res.status, payload?.error?.message ?? 'Request failed');
    return payload as T;
  }

  /** Step 1. Idempotent — call it again whenever you need a fresh token. */
  async requestToken(mode: 'participant' | 'observer' = 'participant') {
    this.cb.onPhase('requesting');
    try {
      this.token = await this.request<JoinConsultResponse>('POST', '/token', { mode });
    } catch (e) {
      // 4xx here is a decision, not an outage: the shift is not virtual, the
      // window is closed, the call already ended, or this user is not a party
      // to it. None of those are retryable without something changing.
      this.cb.onPhase(e instanceof ConsultError && e.status < 500 ? 'blocked' : 'failed',
                      (e as Error).message);
      throw e;
    }
    this.cb.onPhase('ready');
    return this.token;
  }

  /** Step 2. Requires a token from requestToken(). */
  async connect() {
    if (!this.token) throw new Error('requestToken() first');
    if (this.token.mock) {
      // Local dev with no LiveKit credentials. The token is fake; connecting
      // would fail confusingly, so say so instead.
      this.cb.onPhase('failed', 'Backend is in LiveKit mock mode');
      return;
    }

    this.cb.onPhase('connecting');
    const room = new Room({ adaptiveStream: true, dynacast: true });
    this.room = room;

    room.on(RoomEvent.TrackSubscribed, (track: RemoteTrack, _pub, p: RemoteParticipant) =>
      this.cb.onTrack(p.identity, track, false));
    room.on(RoomEvent.TrackUnsubscribed, (track) =>
      track.detach().forEach((el) => el.remove()));
    room.on(RoomEvent.ParticipantDisconnected, (p) => this.cb.onTrackGone(p.identity));

    // Attach the self-view from the event, never by reaching for the
    // publication after enableCameraAndMicrophone() — that races the publish.
    room.on(RoomEvent.LocalTrackPublished, (pub) => {
      if (pub.track) this.cb.onTrack(room.localParticipant.identity, pub.track, true);
    });

    // The SDK reconnects on its own. Surface it, do not tear anything down.
    room.on(RoomEvent.Reconnecting, () => this.cb.onPhase('connected', 'reconnecting'));
    room.on(RoomEvent.Reconnected, () => this.cb.onPhase('connected'));
    room.on(RoomEvent.Disconnected, (reason) => {
      this.stopPolling();
      this.cb.onPhase('ended', reason ? String(reason) : undefined);
    });

    await room.connect(this.token.url, this.token.token);
    if (this.token.can_publish) {
      await room.localParticipant.enableCameraAndMicrophone();
    }
    this.cb.onPhase('connected');

    if (this.token.clock_in.mode === 'auto_on_join' && !this.token.clock_in.already_clocked_in) {
      this.startClockInPoll();
    }
  }

  // See "The clock-in handshake" below for why these numbers.
  private startClockInPoll() {
    const startedAt = Date.now();
    const tick = async () => {
      const view = await this.request<ConsultSessionView>('GET', '');
      if (view.clock_in_recorded) {
        this.stopPolling();
        const me = view.participants.find((p) => p.identity === this.token!.identity);
        this.cb.onClockIn(me?.clocked_in_at ?? new Date().toISOString());
        return;
      }
      if (Date.now() - startedAt > 30_000) {
        this.stopPolling();
        await this.manualClockInFallback();
      }
    };
    setTimeout(tick, 5_000);
    this.pollTimer = setInterval(tick, 5_000);
  }

  private stopPolling() {
    if (this.pollTimer) clearInterval(this.pollTimer);
    this.pollTimer = null;
  }

  /** A webhook was lost. The manual endpoint is the permanent fallback. */
  private async manualClockInFallback() {
    await fetch(`${this.api.baseUrl}/api/v1/shifts/${this.shiftId}/clockin`, {
      method: 'POST',
      headers: { Authorization: `Bearer ${this.api.jwt}`, 'Content-Type': 'application/json' },
      body: JSON.stringify({ method: 'virtual' }),
    });
  }

  /** Leave. Safe to call repeatedly; does not end the call for anyone else. */
  async leave() {
    this.stopPolling();
    await this.room?.disconnect();
    this.room = null;
    await this.request('POST', '/leave').catch(() => { /* best effort */ });
  }

  /** Hospital admin only. Disconnects everyone. */
  async endForEveryone(reason?: string) {
    await this.request('POST', '/end', { reason });
    await this.leave();
  }
}

export class ConsultError extends Error {
  constructor(readonly status: number, message: string) { super(message); }
}
```

### Wiring it to React

```tsx
function useConsult(shiftId: string, api: { baseUrl: string; jwt: string }) {
  const [phase, setPhase] = useState<ConsultPhase>('idle');
  const [clockedInAt, setClockedInAt] = useState<string | null>(null);
  const sessionRef = useRef<ConsultSession | null>(null);

  useEffect(() => {
    const session = new ConsultSession(shiftId, api, {
      onPhase: setPhase,
      onClockIn: setClockedInAt,
      // Your own renderers: attach the media element into a tile, and remove
      // the tile when a participant goes. On web that is track.attach() /
      // track.detach(); on React Native it is <VideoTrack>.
      onTrack: attachToDom,
      onTrackGone: removeFromDom,
    });
    sessionRef.current = session;

    // The single most important line in this file. Without it, navigating away
    // mid-call leaves the media connection open and the camera light on.
    return () => { void session.leave(); };
  }, [shiftId]);

  return { phase, clockedInAt, session: sessionRef.current };
}
```

## The clock-in handshake

Worth its own section because it is the part that is easy to get subtly wrong,
and it is the part that touches the worker's pay.

- Poll `GET /consult` starting **~5 s** after `connect()` resolves, then every
  5 s. Do not poll before connecting — there is nothing to see.
- Show *"Clocking you in…"* while polling. It normally resolves on the first or
  second tick.
- If `clock_in_recorded` is still false **30 s** after connecting, a webhook was
  lost. `POST /api/v1/shifts/{shift_id}/clockin` with `{"method":"virtual"}`.
  That endpoint is a permanent, documented fallback, not a stopgap.
- **Only poll when `clock_in.mode === "auto_on_join"`.** When it is `"manual"`
  the backend's join → clock-in mapping is switched off
  (`LIVEKIT_VIRTUAL_CLOCKIN_ENABLED=false`, which is the shipping default), and
  polling would spin for 30 s before falling back. Render the normal clock-in
  button instead.
- **Rejoining is safe.** A reconnect will not move `clocked_in_at`; the backend
  refuses to overwrite an existing clock-in specifically so a dropped call
  cannot shorten someone's paid hours. Reconnect as often as you need to.
- Only the **clinician** is clocked in. A hospital admin joining records nothing,
  so never show clock-in UI to them.

## Teardown

The most common source of "my camera light is still on" bugs. You need **all
three** of these, because each covers a case the others miss:

```ts
// 1. Explicit — the Leave button.
await session.leave();

// 2. Route change / unmount.
useEffect(() => () => { void session.leave(); }, []);

// 3. Tab close or navigation away.
window.addEventListener('beforeunload', () => {
  fetch(`${baseUrl}/api/v1/shifts/${shiftId}/consult/leave`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${jwt}` },
    keepalive: true,          // survives the page going away
  }).catch(() => {});
});
```

**Do not use `navigator.sendBeacon` here.** It cannot carry an `Authorization`
header, so the request arrives unauthenticated and is rejected — the one call
that most needs to be reliable would silently 401 every time.
`fetch({ keepalive: true })` carries headers and survives unload.

`/leave` is advisory: it records the departure and does not end the call for
anyone else or clock the worker out. LiveKit will also notice the disconnect on
its own, so a missed `/leave` is untidy, not broken.

## Mapping errors to the UI

Every error uses the platform shape `{"error":{"message":"…","status":409}}`.

| Status | When | What to show |
|---|---|---|
| `401` | Missing or expired app JWT | Send them through login again |
| `403` | Not the assigned clinician, wrong hospital, or a worker asking for `observer` | "You're not a participant in this consultation." Terminal — do not retry |
| `404` | Shift does not exist, or (on `GET`) nobody has requested a token yet | On `GET`, treat as "not started", not an error |
| `409` | Not a virtual shift · status not joinable · outside the window · already ended | Show `error.message` directly; it is written to be user-facing. Terminal |
| `500` | LiveKit unreachable | "Video is temporarily unavailable." Retryable — offer a Retry button |

A `409` is a decision, not a failure. Retrying it unchanged will always fail
again, so do not put it behind an automatic retry.

## Platform notes

**Web.** `getUserMedia` requires a secure context: HTTPS, or `localhost` /
`127.0.0.1`. A LAN address like `192.168.1.x` over plain HTTP will **not**
prompt for the camera — this catches people out constantly when testing from a
phone. Tunnel it (`ngrok http …`) and use the HTTPS URL.

**React Native.** Same flow, same contract. Register the globals once at app
start (`registerGlobals()` from `@livekit/react-native`), declare camera and
microphone permissions in `Info.plist` / `AndroidManifest.xml`, and request them
before `connect()`. Use `<VideoTrack>` from the RN package instead of
`track.attach()`. Keep the call alive in the background with
`AppState`-aware handling — backgrounding will drop video on iOS unless you
configure it.

**Flutter.** `livekit_client` mirrors the JS API closely; `Room`, `RoomEvent`
and `connect(url, token)` all carry over. Render with `VideoTrackRenderer`.

## Traps

Each of these was hit for real while building the reference harness.

1. **`sendBeacon` cannot set headers.** Covered above. It fails silently.
2. **Do not reach for the local publication after
   `enableCameraAndMicrophone()`** — it races the publish, so the self-view
   appears intermittently. Attach from `RoomEvent.LocalTrackPublished`.
3. **`expires_at` is a join deadline, not a call deadline.** LiveKit validates
   the token only at `connect()`. Once connected, the call outlives it. If the
   user idles on the pre-join screen past it, call `/token` again — never show
   an error for this.
4. **`mock: true` means the backend has no LiveKit credentials.** The token is
   fake. Show a dev banner; do not attempt to connect.
5. **Gate the Join button** on `shift_type === "virtual"`, status in
   `assigned | upcoming | in_progress`, and the ±60-minute window. Otherwise the
   user hits a `409` the UI could have prevented.
6. **`live: false` is a hint, not an error.** It means LiveKit was unreachable
   on that request and you are seeing the last webhook-fed state, which may lag
   a few seconds. Show a subtle "reconnecting" marker, not a failure state.
7. **Two tabs on one origin share `localStorage`.** If you cache the JWT there,
   opening the worker and the admin in two tabs will have them overwrite each
   other. Use separate browsers or a private window when testing.
8. **`/end` is hospital-admin only.** A clinician leaving uses `/leave`. Do not
   render an End button for workers.

---

# Part 2 — The endpoint contract

Four endpoints you call, plus one LiveKit calls. All under the `bearerAuth` JWT
scheme and the literal `/api/v1` prefix.

## `POST /api/v1/shifts/{shift_id}/consult/token`

Roles: `HealthWorker`, `HospitalAdmin`. The service then checks the caller is
*this* shift's assigned clinician or an admin of *this* shift's hospital.

Request — every field optional; send `{}` if you have nothing to say:

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

`participant_role` is `clinician` or `hospital_observer`. `can_publish` is false
only for `mode: "observer"`.

### The consultation window

A token is minted only from **60 minutes before `scheduled_start` to 60 minutes
after `scheduled_end`**, and only while the shift is `assigned`, `upcoming` or
`in_progress`. Outside that, `409`.

This mirrors the clock-in rules deliberately: a token minted earlier would
produce a join that could not clock anyone in. Gate your Join button on the same
window.

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

`404` only if nobody has ever requested a token. Once one has been issued you get
`200` with `status: "pending"`, so the pre-join screen never handles a 404.

## `POST /api/v1/shifts/{shift_id}/consult/leave`

Roles: `HealthWorker`, `HospitalAdmin`. Idempotent, always `200`.

```json
{ "session_id": "8f1c…", "identity": "u:9d4e…", "left_at": "2026-08-21T12:31:04Z",
  "session_status": "active", "remaining_participants": 1 }
```

Does not end the call for anyone else and does not clock the worker out.
Clock-out still requires a handover, then `POST …/clockout`.

## `POST /api/v1/shifts/{shift_id}/consult/end`

Roles: `HospitalAdmin` (own hospital only), `SuperAdmin`, `OperationsAdmin`.
Disconnects everyone.

Request `{ "reason": "Consultation complete" }` (optional) → `200 OK`:

```json
{ "session_id": "8f1c…", "status": "ended", "ended_at": "2026-08-21T12:45:00Z",
  "ended_reason": "ended_by_hospital", "clock_out_required": true,
  "clock_out_hint": "The clinician must submit a handover, then POST /api/v1/shifts/{shift_id}/clockout" }
```

Idempotent: ending an already-ended session returns the **original** `ended_at`.
After this, `/token` returns `409` — there is no rejoining an ended session.

## `POST /api/v1/webhooks/livekit`

LiveKit Cloud calls this, not you. Listed so nobody wires the app at it.

## Not in this release

`recording` is always `{ "enabled": false, "status": null }`. It ships now so the
recording indicator can be built against a stable shape, but nothing records and
no token ever carries a record grant. Ad-hoc (non-shift) consults are not built
either — every session belongs to a shift.

---

# Testing

`dev/consult-tester.html` is a single-file harness that runs this exact flow —
token, connect, tiles, session polling, leave, end. It is the reference
implementation of everything above, and it is worth reading before you write
your own.

```bash
# One command gives you a joinable shift and both JWTs.
JWT_SECRET=... DATABASE_URL=... ./dev/seed-consult-fixture.sh

# Same-origin proxy for the page and the API, so one tunnel covers both.
./dev/tunnel-proxy.py          # then: ngrok http 5100
```

Open the printed link on two devices — worker on one, hospital admin on the
other. For webhook-driven clock-in, set the tunnel's
`…/api/v1/webhooks/livekit` in the LiveKit console under Settings → Webhooks;
without it the reconciler still clocks the worker in, but after ~5 minutes
rather than instantly.

To check a token in isolation, paste `url` and `token` into
<https://meet.livekit.io/?tab=custom>. If that connects, the backend contract is
correct and anything remaining is in your SDK wiring — which removes a whole
class of "is it me or the backend" back-and-forth.

## Before you call it done

- [ ] Join is gated on `shift_type === "virtual"`, joinable status, and the window
- [ ] `mock: true` shows a dev banner instead of attempting `connect()`
- [ ] `clock_in.mode === "manual"` shows the clock-in button, not a spinner
- [ ] The 5 s poll and the 30 s manual fallback are both wired
- [ ] Teardown covers the Leave button, unmount, **and** `beforeunload`
- [ ] `beforeunload` uses `fetch({keepalive:true})`, never `sendBeacon`
- [ ] `/end` renders only for hospital admins
- [ ] An expired `expires_at` on the pre-join screen re-requests a token
- [ ] `live: false` degrades to a hint, not an error
- [ ] Reconnecting mid-call does not move `clocked_in_at` (reload and check)
