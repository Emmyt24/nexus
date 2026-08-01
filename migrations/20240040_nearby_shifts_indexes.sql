-- E1 — View Nearby Shifts (SCRUM-24 / US-08)
-- Supports the radius-filtered worker discovery query in
-- ShiftRepository::list_nearby_shifts. Additive and reversible: indexes only,
-- no data or schema mutation.

-- The nearby query scans open shifts only. A partial index keeps that scan
-- small as completed/assigned shifts accumulate.
CREATE INDEX IF NOT EXISTS idx_shifts_open
    ON shifts (scheduled_start)
    WHERE status = 'open';

-- Bounding-box prefilter on hospital coordinates hits this index before the
-- exact haversine distance is evaluated. (A functional (latitude, longitude)
-- index already exists on hospital_locations from 20240009; this composite
-- covers the join + range scan used by the discovery query.)
CREATE INDEX IF NOT EXISTS idx_hospital_locations_hospital_coords
    ON hospital_locations (hospital_id, latitude, longitude);

-- Rollback:
--   DROP INDEX IF EXISTS idx_hospital_locations_hospital_coords;
--   DROP INDEX IF EXISTS idx_shifts_open;
