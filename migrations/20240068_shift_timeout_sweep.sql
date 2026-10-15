-- Close out shifts whose scheduled end has passed, after a grace period.
--
-- Nothing currently sweeps `shifts.scheduled_end`. A shift whose end time has
-- passed with no clock-out stays `in_progress` (or `assigned`/`upcoming` if the
-- clinician never arrived) indefinitely, and three things stay stuck with it:
--
--   * the hospital's escrow, taken at creation into `hospital_wallets.held_kobo`
--     and released only by `cancel_shift`, which refuses `in_progress`;
--   * the clinician, because `clinician_has_active_assignment` counts
--     `upcoming`/`in_progress` and so blocks every future assignment;
--   * payout and ratings, which both require `completed`.
--
-- The sweep splits on whether the clinician actually turned up, because the two
-- cases owe opposite things. Someone who worked the shift and forgot to clock
-- out must still be paid; a no-show must not be, and the hospital's money must
-- go back.
--
-- `no_show` already exists in the `shift_status` enum (20240001) and has never
-- been written by anything, so this needs no `ALTER TYPE ... ADD VALUE` — which
-- could not run inside sqlx's per-file transaction anyway (see 20240057).
-- `shift_attendance.missed` (20240024) is likewise declared and never written.
--
-- The grace period is configuration, not a literal: `clock_in_grace_minutes`
-- and `auto_clock_out_hours` are already in this table and read by nothing, and
-- this must not become a third dead setting.

ALTER TABLE platform_settings
    ADD COLUMN IF NOT EXISTS shift_close_grace_minutes INTEGER NOT NULL DEFAULT 15
        CHECK (shift_close_grace_minutes >= 0);

-- Partial index matching the sweep's predicate exactly, following
-- 20240040_nearby_shifts_indexes.sql and the `idx_shift_assignments_expiry`
-- convention from 20240024: index only the rows a sweep can still claim, so the
-- scan stays proportional to open work rather than to all shifts ever created.
CREATE INDEX IF NOT EXISTS idx_shifts_timeout_sweep
    ON shifts (scheduled_end)
    WHERE status IN ('assigned', 'upcoming', 'in_progress');

-- Rollback:
--   DROP INDEX IF EXISTS idx_shifts_timeout_sweep;
--   ALTER TABLE platform_settings DROP COLUMN IF EXISTS shift_close_grace_minutes;
