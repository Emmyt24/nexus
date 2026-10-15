-- Indexes for the hospital-scoped search (GET /api/v1/search).
--
-- Search matches with `ILIKE '%term%'`. A leading wildcard cannot use a B-tree
-- index, so these do NOT make the text matching itself indexed — what they do is
-- make the *tenant* predicate cheap, so each query scans one hospital's rows
-- rather than the whole table. That is the difference that matters: the text
-- filter then runs over a few hundred rows instead of every shift on the
-- platform.
--
-- Trigram indexing (`pg_trgm` + GIN) is the real fix for the text match and is
-- the upgrade path if these get slow. It is deliberately not done here: the
-- migration set contains no `CREATE EXTENSION` anywhere, and `src/main.rs` runs
-- migrations at boot with a fatal error, so if the deployed role may not install
-- an extension the service would fail to start rather than degrade.
--
-- `shift_handovers` needs no index of its own — it is reached by a join on
-- `shift_id`, which is already UNIQUE, and scoped through `shifts`.

-- Patients are searched by name within one hospital.
CREATE INDEX IF NOT EXISTS idx_patients_hospital_search
    ON patients (hospital_id, created_at DESC);

-- Shifts are searched within one hospital and ordered by start time.
CREATE INDEX IF NOT EXISTS idx_shifts_hospital_scheduled
    ON shifts (hospital_id, scheduled_start DESC);

-- The clinician search probes applications and interests by clinician_id, but
-- both already have exactly that index — idx_shift_applications_clinician_id
-- (20240021:27) and idx_shift_interests_clinician_id (20240012:107). Adding our
-- own would have been a second identical B-tree on each table: maintained on
-- every write, for no planner benefit. Nothing to add here.

-- Rollback:
--   DROP INDEX IF EXISTS idx_shifts_hospital_scheduled;
--   DROP INDEX IF EXISTS idx_patients_hospital_search;
