-- Facts about the installation itself, in exactly one row.
CREATE TABLE meta (
    id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    -- When a Claude session first connected; NULL until one has.
    first_session_at_ms BIGINT
);

-- A database from before this table has already seen sessions; its oldest lease or run is the
-- earliest one left.
INSERT INTO meta (id, first_session_at_ms)
SELECT 1, MIN(at_ms) FROM (
    SELECT MIN(requested_at_ms) AS at_ms FROM leases
    UNION ALL
    SELECT MIN(started_at_ms) FROM job_runs
);
