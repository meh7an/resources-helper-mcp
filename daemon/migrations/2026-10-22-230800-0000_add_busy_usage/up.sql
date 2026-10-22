-- Average cores and GPU load of the holder's session over only the samples in which it was
-- busy, so idle time around the command does not dilute them. Both NULL when the session was
-- never busy and for runs recorded before this was measured; estimates then use the averages.
ALTER TABLE job_runs ADD COLUMN busy_cores FLOAT;
ALTER TABLE job_runs ADD COLUMN busy_gpu_pct FLOAT;
