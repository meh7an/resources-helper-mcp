-- Why a lease ended, kept apart from `state`, whose CHECK constraint would need a table rebuild
-- to take new values. NULL while the lease is queued or active.
ALTER TABLE leases ADD COLUMN end_cause TEXT
    CHECK (end_cause IN ('released', 'expired', 'killed', 'removed'));
-- When the holder's session was told that the user killed or removed the lease; NULL until then.
ALTER TABLE leases ADD COLUMN notified_at_ms BIGINT;
-- Session that ran the job, and why its lease ended.
ALTER TABLE job_runs ADD COLUMN session TEXT;
ALTER TABLE job_runs ADD COLUMN end_cause TEXT
    CHECK (end_cause IN ('released', 'expired', 'killed', 'removed'));

-- Before causes existed, a lease ended only by its holder's release or by expiry, and only
-- released leases stored runs, so older rows can be filled in exactly.
UPDATE leases SET end_cause = state WHERE state IN ('released', 'expired');
UPDATE job_runs SET end_cause = 'released';
UPDATE job_runs
SET session = (SELECT session_id FROM leases WHERE leases.id = job_runs.lease_id)
WHERE lease_id IS NOT NULL;
