-- Working directory of the command, part of the job fingerprint; empty for older leases.
ALTER TABLE leases ADD COLUMN cwd TEXT NOT NULL DEFAULT '';
-- Whether the holder's session held another active lease during the run, so its measured
-- usage mixes several jobs and estimates leave it out.
ALTER TABLE job_runs ADD COLUMN overlapped BOOLEAN NOT NULL DEFAULT FALSE;
