-- One rating per shift per direction, not one per rating *user*.
--
-- `uniq_rating_per_shift UNIQUE (shift_id, rater_user_id, ratee_kind)` from
-- 20240024 deduplicated by the individual who wrote the rating. That was
-- sufficient while only the admin who posted the shift could rate its worker,
-- because there was exactly one eligible rater. It is not sufficient now that
-- any admin at the owning hospital may rate (the previous check compared
-- `shift.created_by` to the caller and so refused a colleague, which was the
-- bug being fixed).
--
-- Without this, a hospital with five admins can post five worker ratings for the
-- same shift, and `recompute_clinician_rating_tx` averages `clinicians.rating`
-- over all of them — five one-star votes out of a single shift, with
-- `rating_count` reading 5. A super admin could add more from anywhere.
--
-- Keyed on (shift_id, ratee_kind): one clinician-rating and one hospital-rating
-- per shift, which is what BR-F1-47 ("both parties can rate independently")
-- actually means. `rater_user_id` stays on the row and still governs *editing* —
-- whoever wrote it is the only one who may change it.
--
-- Pre-existing duplicates are collapsed to the earliest row first, otherwise
-- the index cannot be created. There should be none in practice: the stacking
-- path only became reachable on this branch.

DELETE FROM shift_ratings r
 USING shift_ratings keep
 WHERE r.shift_id   = keep.shift_id
   AND r.ratee_kind = keep.ratee_kind
   AND r.created_at > keep.created_at;

ALTER TABLE shift_ratings
    DROP CONSTRAINT IF EXISTS uniq_rating_per_shift;

CREATE UNIQUE INDEX IF NOT EXISTS uq_rating_per_shift_direction
    ON shift_ratings (shift_id, ratee_kind);

-- Rollback:
--   DROP INDEX IF EXISTS uq_rating_per_shift_direction;
--   ALTER TABLE shift_ratings
--       ADD CONSTRAINT uniq_rating_per_shift UNIQUE (shift_id, rater_user_id, ratee_kind);
