// ! SQL for the LiveKit consultation tables. No business rules here — every
// ! method is one statement, and the idempotency guards live inside the
// ! statements themselves rather than in a read-then-write above them.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::video_session::{
    NewVideoSessionEvent, ParticipantRole, PendingClockout, VideoSession,
    VideoSessionParticipant,
};

/// Every column of `video_sessions`, in declaration order. Repeated in each
/// query so `SELECT *` never silently changes the `FromRow` mapping.
const SESSION_COLUMNS: &str = r#"
    id, shift_id, hospital_id, created_by, room_name, livekit_room_sid, status,
    max_participants, departure_timeout_s, empty_timeout_s,
    started_at, ended_at, ended_reason, recording_enabled, recording_consent,
    metadata, created_at, updated_at
"#;

const PARTICIPANT_COLUMNS: &str = r#"
    id, session_id, identity, user_id, clinician_id, display_name,
    participant_role, can_publish, token_issued_at, token_expires_at,
    token_issue_count, participant_sid, joined_at, left_at, disconnect_reason,
    clocked_in_at, created_at, updated_at
"#;

pub struct VideoSessionRepository {
    pool: PgPool,
}

impl VideoSessionRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    // Sessions

    pub async fn find_by_shift(
        &self,
        shift_id: Uuid,
    ) -> Result<Option<VideoSession>, sqlx::Error> {
        sqlx::query_as::<_, VideoSession>(&format!(
            "SELECT {SESSION_COLUMNS} FROM video_sessions WHERE shift_id = $1"
        ))
        .bind(shift_id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn find_by_room_name(
        &self,
        room_name: &str,
    ) -> Result<Option<VideoSession>, sqlx::Error> {
        sqlx::query_as::<_, VideoSession>(&format!(
            "SELECT {SESSION_COLUMNS} FROM video_sessions WHERE room_name = $1"
        ))
        .bind(room_name)
        .fetch_optional(&self.pool)
        .await
    }

    /// Seed-and-store the room for a shift. The `DO UPDATE` (rather than
    /// `DO NOTHING`) is what makes this safe under two concurrent join
    /// requests — `DO NOTHING` returns no row for the loser.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_session_for_shift(
        &self,
        shift_id: Uuid,
        hospital_id: Uuid,
        created_by: Option<Uuid>,
        room_name: &str,
        max_participants: i32,
        empty_timeout_s: i32,
        departure_timeout_s: i32,
    ) -> Result<VideoSession, sqlx::Error> {
        sqlx::query_as::<_, VideoSession>(&format!(
            r#"
            INSERT INTO video_sessions
                (shift_id, hospital_id, created_by, room_name,
                 max_participants, empty_timeout_s, departure_timeout_s)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (shift_id) DO UPDATE
               SET updated_at = NOW()
            RETURNING {SESSION_COLUMNS}
            "#
        ))
        .bind(shift_id)
        .bind(hospital_id)
        .bind(created_by)
        .bind(room_name)
        .bind(max_participants)
        .bind(empty_timeout_s)
        .bind(departure_timeout_s)
        .fetch_one(&self.pool)
        .await
    }

    /// Forward-only: a late `room_started` for an already-active room is a
    /// no-op, and an ended room is never resurrected.
    pub async fn mark_started(
        &self,
        room_name: &str,
        at: DateTime<Utc>,
        sid: Option<&str>,
    ) -> Result<Option<VideoSession>, sqlx::Error> {
        sqlx::query_as::<_, VideoSession>(&format!(
            r#"
            UPDATE video_sessions
               SET status           = 'active',
                   started_at       = COALESCE(started_at, $2),
                   livekit_room_sid = COALESCE($3, livekit_room_sid),
                   updated_at       = NOW()
             WHERE room_name = $1
               AND status    = 'pending'
            RETURNING {SESSION_COLUMNS}
            "#
        ))
        .bind(room_name)
        .bind(at)
        .bind(sid)
        .fetch_optional(&self.pool)
        .await
    }

    /// `WHERE status <> 'ended'` keeps `ended_at` / `ended_reason` at the
    /// values written by whoever ended the room first.
    pub async fn mark_ended(
        &self,
        room_name: &str,
        at: DateTime<Utc>,
        reason: &str,
    ) -> Result<Option<VideoSession>, sqlx::Error> {
        sqlx::query_as::<_, VideoSession>(&format!(
            r#"
            UPDATE video_sessions
               SET status       = 'ended',
                   ended_at     = $2,
                   ended_reason = $3,
                   updated_at   = NOW()
             WHERE room_name = $1
               AND status   <> 'ended'
            RETURNING {SESSION_COLUMNS}
            "#
        ))
        .bind(room_name)
        .bind(at)
        .bind(reason)
        .fetch_optional(&self.pool)
        .await
    }

    /// Close every still-open participant row when a room finishes.
    pub async fn close_open_participants(
        &self,
        session_id: Uuid,
        at: DateTime<Utc>,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            UPDATE video_session_participants
               SET left_at    = GREATEST(COALESCE(left_at, $2), $2),
                   updated_at = NOW()
             WHERE session_id = $1
               AND left_at IS NULL
            "#,
        )
        .bind(session_id)
        .bind(at)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    // Participants

    /// Called on every token issue. The conflict branch bumps
    /// `token_issue_count` and clears the previous departure, so a rejoin after
    /// a dropped call reuses the row instead of creating a second one.
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_participant_on_token(
        &self,
        session_id: Uuid,
        identity: &str,
        user_id: Option<Uuid>,
        clinician_id: Option<Uuid>,
        display_name: &str,
        participant_role: ParticipantRole,
        can_publish: bool,
        token_expires_at: DateTime<Utc>,
    ) -> Result<VideoSessionParticipant, sqlx::Error> {
        sqlx::query_as::<_, VideoSessionParticipant>(&format!(
            r#"
            INSERT INTO video_session_participants
                (session_id, identity, user_id, clinician_id, display_name,
                 participant_role, can_publish, token_expires_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (session_id, identity) DO UPDATE
               SET user_id           = EXCLUDED.user_id,
                   clinician_id      = EXCLUDED.clinician_id,
                   display_name      = EXCLUDED.display_name,
                   participant_role  = EXCLUDED.participant_role,
                   can_publish       = EXCLUDED.can_publish,
                   token_issued_at   = NOW(),
                   token_expires_at  = EXCLUDED.token_expires_at,
                   token_issue_count = video_session_participants.token_issue_count + 1,
                   left_at           = NULL,
                   disconnect_reason = NULL,
                   updated_at        = NOW()
            RETURNING {PARTICIPANT_COLUMNS}
            "#
        ))
        .bind(session_id)
        .bind(identity)
        .bind(user_id)
        .bind(clinician_id)
        .bind(display_name)
        .bind(participant_role.as_str())
        .bind(can_publish)
        .bind(token_expires_at)
        .fetch_one(&self.pool)
        .await
    }

    pub async fn find_participant(
        &self,
        session_id: Uuid,
        identity: &str,
    ) -> Result<Option<VideoSessionParticipant>, sqlx::Error> {
        sqlx::query_as::<_, VideoSessionParticipant>(&format!(
            r#"
            SELECT {PARTICIPANT_COLUMNS}
              FROM video_session_participants
             WHERE session_id = $1 AND identity = $2
            "#
        ))
        .bind(session_id)
        .bind(identity)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn list_participants(
        &self,
        session_id: Uuid,
    ) -> Result<Vec<VideoSessionParticipant>, sqlx::Error> {
        sqlx::query_as::<_, VideoSessionParticipant>(&format!(
            r#"
            SELECT {PARTICIPANT_COLUMNS}
              FROM video_session_participants
             WHERE session_id = $1
             ORDER BY token_issued_at ASC
            "#
        ))
        .bind(session_id)
        .fetch_all(&self.pool)
        .await
    }

    /// `joined_at = COALESCE(joined_at, $ts)` — monotonic, so a re-ordered
    /// delivery cannot push the first join forward. Returns `None` when we
    /// never issued a token for this identity.
    pub async fn mark_participant_joined(
        &self,
        session_id: Uuid,
        identity: &str,
        participant_sid: Option<&str>,
        at: DateTime<Utc>,
    ) -> Result<Option<VideoSessionParticipant>, sqlx::Error> {
        sqlx::query_as::<_, VideoSessionParticipant>(&format!(
            r#"
            UPDATE video_session_participants
               SET joined_at         = COALESCE(joined_at, $4),
                   participant_sid   = COALESCE($3, participant_sid),
                   left_at           = NULL,
                   disconnect_reason = NULL,
                   updated_at        = NOW()
             WHERE session_id = $1 AND identity = $2
            RETURNING {PARTICIPANT_COLUMNS}
            "#
        ))
        .bind(session_id)
        .bind(identity)
        .bind(participant_sid)
        .bind(at)
        .fetch_optional(&self.pool)
        .await
    }

    /// `left_at = GREATEST(...)` so an out-of-order delivery cannot move a
    /// departure backwards. A `participant_left` for an unknown identity is a
    /// no-op, not an error.
    pub async fn mark_participant_left(
        &self,
        session_id: Uuid,
        identity: &str,
        at: DateTime<Utc>,
        disconnect_reason: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE video_session_participants
               SET left_at           = GREATEST(COALESCE(left_at, $3), $3),
                   disconnect_reason = COALESCE($4, disconnect_reason),
                   updated_at        = NOW()
             WHERE session_id = $1 AND identity = $2
            "#,
        )
        .bind(session_id)
        .bind(identity)
        .bind(at)
        .bind(disconnect_reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn count_connected_participants(
        &self,
        session_id: Uuid,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
              FROM video_session_participants
             WHERE session_id = $1
               AND joined_at IS NOT NULL
               AND left_at   IS NULL
            "#,
        )
        .bind(session_id)
        .fetch_one(&self.pool)
        .await
    }

    /// Second idempotency layer. The single `UPDATE … WHERE clocked_in_at IS
    /// NULL RETURNING id` means the row lock resolves concurrent deliveries:
    /// exactly one caller gets `Some`.
    pub async fn claim_clockin_slot(
        &self,
        session_id: Uuid,
        identity: &str,
        at: DateTime<Utc>,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE video_session_participants
               SET clocked_in_at = $3,
                   updated_at    = NOW()
             WHERE session_id = $1
               AND identity   = $2
               AND clocked_in_at IS NULL
            RETURNING id
            "#,
        )
        .bind(session_id)
        .bind(identity)
        .bind(at)
        .fetch_optional(&self.pool)
        .await
    }

    /// Hand the slot back so the reconciler can retry. Only ever called after a
    /// genuine `Err` — a "we decided not to clock in" outcome keeps the claim.
    pub async fn release_clockin_slot(
        &self,
        session_id: Uuid,
        identity: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE video_session_participants
               SET clocked_in_at = NULL,
                   updated_at    = NOW()
             WHERE session_id = $1 AND identity = $2
            "#,
        )
        .bind(session_id)
        .bind(identity)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // Audit trail

    pub async fn insert_event(&self, ev: NewVideoSessionEvent) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO video_session_events
                (session_id, room_name, event_type, identity, actor_user_id,
                 livekit_event_id, payload, occurred_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            RETURNING id
            "#,
        )
        .bind(ev.session_id)
        .bind(&ev.room_name)
        .bind(&ev.event_type)
        .bind(&ev.identity)
        .bind(ev.actor_user_id)
        .bind(&ev.livekit_event_id)
        .bind(&ev.payload)
        .bind(ev.occurred_at)
        .fetch_one(&self.pool)
        .await
    }

    /// Guards one-shot side effects (the handover reminder push).
    pub async fn has_event(
        &self,
        session_id: Uuid,
        event_type: &str,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar::<_, bool>(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM video_session_events
                 WHERE session_id = $1 AND event_type = $2
            )
            "#,
        )
        .bind(session_id)
        .bind(event_type)
        .fetch_one(&self.pool)
        .await
    }

    // Webhook idempotency — the shared `webhook_events` table

    /// Sibling of `WalletRepository::insert_webhook_event_if_new`, which
    /// hardcodes `provider = 'safehaven'` and returns a wallet error. Returns
    /// `Some(id)` for a new event and `None` for a re-delivery.
    pub async fn insert_livekit_webhook_if_new(
        &self,
        provider_event_id: &str,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        let res = sqlx::query_scalar::<_, Uuid>(
            r#"
            INSERT INTO webhook_events
                (provider, provider_event_id, event_type, raw_payload)
            VALUES ('livekit', $1, $2, $3)
            RETURNING id
            "#,
        )
        .bind(provider_event_id)
        .bind(event_type)
        .bind(payload)
        .fetch_one(&self.pool)
        .await;

        match res {
            Ok(id) => Ok(Some(id)),
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn mark_webhook_processed(
        &self,
        event_id: Uuid,
        error_message: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE webhook_events
               SET processed     = ($2 IS NULL),
                   processed_at  = NOW(),
                   error_message = $2
             WHERE id = $1
            "#,
        )
        .bind(event_id)
        .bind(error_message)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // Reconciler sweeps

    /// Branch 1: a token was minted long enough ago that the
    /// `participant_joined` webhook should have arrived, and it did not.
    ///
    /// Bounded to sessions whose consultation window is still open. Past that a
    /// join could not clock anyone in even if we recovered it, so an abandoned
    /// pre-join screen stops being swept instead of costing a LiveKit call
    /// every tick for the life of the row.
    pub async fn sessions_awaiting_join(
        &self,
        token_issued_before: DateTime<Utc>,
    ) -> Result<Vec<VideoSession>, sqlx::Error> {
        sqlx::query_as::<_, VideoSession>(&format!(
            r#"
            SELECT {SESSION_COLUMNS}
              FROM video_sessions s
             WHERE s.status = 'pending'
               AND EXISTS (
                   SELECT 1
                     FROM video_session_participants p
                    WHERE p.session_id      = s.id
                      AND p.joined_at       IS NULL
                      AND p.token_issued_at < $1
               )
               AND CASE
                     WHEN s.shift_id IS NULL
                       -- Ad-hoc seam: no shift to bound against, so bound on age.
                       THEN s.created_at > NOW() - INTERVAL '6 hours'
                       ELSE EXISTS (
                           SELECT 1 FROM shifts sh
                            WHERE sh.id = s.shift_id
                              AND NOW() <= sh.scheduled_end + INTERVAL '1 hour'
                       )
                   END
            "#
        ))
        .bind(token_issued_before)
        .fetch_all(&self.pool)
        .await
    }

    /// Branch 2: active rooms nothing has touched for a while — a lost
    /// `room_finished` looks exactly like this.
    pub async fn stale_active_sessions(
        &self,
        updated_before: DateTime<Utc>,
    ) -> Result<Vec<VideoSession>, sqlx::Error> {
        sqlx::query_as::<_, VideoSession>(&format!(
            r#"
            SELECT {SESSION_COLUMNS}
              FROM video_sessions
             WHERE status     = 'active'
               AND updated_at < $1
            "#
        ))
        .bind(updated_before)
        .fetch_all(&self.pool)
        .await
    }

    /// Branch 3: the consult is over, the worker is clocked in, and the shift
    /// is still running. Columns are qualified because `video_sessions` and
    /// `shifts` share most of their names.
    pub async fn ended_sessions_pending_clockout(
        &self,
        ended_before: DateTime<Utc>,
    ) -> Result<Vec<PendingClockout>, sqlx::Error> {
        sqlx::query_as::<_, PendingClockout>(
            r#"
            SELECT s.id   AS session_id,
                   s.room_name,
                   sh.id  AS shift_id,
                   a.clinician_id
              FROM video_sessions      s
              JOIN shifts              sh ON sh.id      = s.shift_id
              JOIN shift_attendance    a  ON a.shift_id = sh.id
             WHERE s.status      = 'ended'
               AND s.ended_at    < $1
               AND sh.status     = 'in_progress'
               AND a.clockin_at  IS NOT NULL
               AND a.clockout_at IS NULL
            "#,
        )
        .bind(ended_before)
        .fetch_all(&self.pool)
        .await
    }
}
