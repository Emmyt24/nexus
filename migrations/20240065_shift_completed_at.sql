-- A real completion timestamp for shifts.
--
-- The rating windows were anchored to `shifts.updated_at`, which is wrong in a
-- way that is easy to miss: `updated_at` moves on *any* write to the row, so an
-- unrelated edit silently slid both parties' 7-day rating window forward. The
-- per-row `shift_ratings.window_closes_at` is computed from it at insert time,
-- so two ratings on the same shift could end up with different deadlines.
--
-- `actual_end` already exists and would have been the natural home, but nothing
-- in the codebase has ever written it, so it cannot be trusted for a window
-- that decides whether a rating is accepted. A dedicated column that is written
-- exactly where the status becomes 'completed' is unambiguous.
--
-- Backfilled from `updated_at` for already-completed shifts: imprecise, but
-- strictly better than NULL, and the readers fall back to `updated_at` anyway.
-- The shift-timeout sweep sets this too, so it must land before that work.

ALTER TABLE shifts
    ADD COLUMN IF NOT EXISTS completed_at TIMESTAMPTZ;

UPDATE shifts
   SET completed_at = updated_at
 WHERE status = 'completed'
   AND completed_at IS NULL;

-- Rollback:
--   ALTER TABLE shifts DROP COLUMN IF EXISTS completed_at;
