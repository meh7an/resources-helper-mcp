//! Queries over leases, their audit trail, and job history.
//!
//! Every function takes a plain connection so callers decide the transaction scope. Functions
//! that write a lease and its event do so in one transaction, so the audit trail cannot drift
//! from the lease row.

use diesel::prelude::*;
use rh_shared::{
    api::LeaseQuery,
    config::RetentionConfig,
    types::{Holder, JobClass, JobProfile, JobRun, Lease, LeaseId, LeaseKind, LeaseState},
};

use super::{
    models::{JobRunRow, LeaseEventRow, LeaseRow, NewJobRunRow, NewLeaseEventRow, NewLeaseRow},
    schema::{job_runs, lease_events, leases},
};

/// A lease about to be stored. It is stored as `Queued`; [`transition_lease`] moves it on.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaseDraft {
    /// Session asking for the lease.
    pub holder: Holder,
    /// How the lease shares the machine.
    pub kind: LeaseKind,
    /// Class the profile was taken from, or `None` for a custom profile.
    pub class: Option<JobClass>,
    /// Resources to reserve.
    pub profile: JobProfile,
    /// What the lease is for, usually the command that will run.
    pub label: String,
    /// Working directory of the command, or empty when unknown.
    pub cwd: String,
    /// When the lease was requested, in milliseconds since the Unix epoch.
    pub requested_at_ms: i64,
    /// First heartbeat deadline.
    pub expires_at_ms: i64,
}

/// One entry of a lease's audit trail.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaseEvent {
    /// Lease the event belongs to.
    pub lease_id: LeaseId,
    /// When the transition happened, in milliseconds since the Unix epoch.
    pub at_ms: i64,
    /// State before the transition, or `None` for the lease's creation.
    pub from_state: Option<LeaseState>,
    /// State after the transition.
    pub to_state: LeaseState,
    /// Why the transition happened, for example `granted` or `holder process exited`.
    pub reason: String,
}

/// Stores a new `Queued` lease and its creation event.
///
/// # Errors
///
/// Returns an error when a value does not fit its column or the database write fails.
pub fn insert_lease(conn: &mut SqliteConnection, draft: &LeaseDraft) -> anyhow::Result<Lease> {
    conn.transaction(|conn| {
        let row = diesel::insert_into(leases::table)
            .values(NewLeaseRow::from_draft(draft)?)
            .returning(LeaseRow::as_returning())
            .get_result(conn)?;
        let lease = Lease::try_from(row)?;
        append_event(
            conn,
            lease.id,
            lease.requested_at_ms,
            None,
            lease.state,
            "requested",
        )?;
        Ok(lease)
    })
}

/// Returns the lease with `id`, if it exists.
///
/// # Errors
///
/// Returns an error when the query fails or the row holds values the shared types reject.
pub fn get_lease(conn: &mut SqliteConnection, id: LeaseId) -> anyhow::Result<Option<Lease>> {
    leases::table
        .find(id.0)
        .select(LeaseRow::as_select())
        .first(conn)
        .optional()?
        .map(Lease::try_from)
        .transpose()
}

/// Returns the leases matching every filter in `query`, oldest first.
///
/// # Errors
///
/// Returns an error when the query fails or a row holds values the shared types reject.
pub fn list_leases(conn: &mut SqliteConnection, query: &LeaseQuery) -> anyhow::Result<Vec<Lease>> {
    let mut select = leases::table
        .select(LeaseRow::as_select())
        .order(leases::id.asc())
        .into_boxed();
    if let Some(session) = &query.session {
        select = select.filter(leases::session_id.eq(session.0.as_str()));
    }
    if let Some(state) = query.state {
        select = select.filter(leases::state.eq(state.as_str()));
    }
    select
        .load(conn)?
        .into_iter()
        .map(Lease::try_from)
        .collect()
}

/// Moves a lease to `to_state` and records the transition. Entering `Active` sets
/// `granted_at_ms`; entering `Released` or `Expired` sets `ended_at_ms`.
///
/// Whether the transition is allowed is the ledger's decision; this only records it.
/// Returns `None` when no lease has `id`.
///
/// # Errors
///
/// Returns an error when the database write fails; nothing is changed in that case.
pub fn transition_lease(
    conn: &mut SqliteConnection,
    id: LeaseId,
    to_state: LeaseState,
    at_ms: i64,
    reason: &str,
) -> anyhow::Result<Option<Lease>> {
    conn.transaction(|conn| {
        let Some(current) = get_lease(conn, id)? else {
            return Ok(None);
        };
        let granted_at_ms = match to_state {
            LeaseState::Active => Some(at_ms),
            _ => current.granted_at_ms,
        };
        let ended_at_ms = match to_state {
            LeaseState::Released | LeaseState::Expired => Some(at_ms),
            _ => current.ended_at_ms,
        };
        let row = diesel::update(leases::table.find(id.0))
            .set((
                leases::state.eq(to_state.as_str()),
                leases::granted_at_ms.eq(granted_at_ms),
                leases::ended_at_ms.eq(ended_at_ms),
            ))
            .returning(LeaseRow::as_returning())
            .get_result(conn)?;
        append_event(conn, id, at_ms, Some(current.state), to_state, reason)?;
        Ok(Some(Lease::try_from(row)?))
    })
}

/// Moves a lease's heartbeat deadline. Returns `false` when no lease has `id`.
///
/// Heartbeats are not recorded in the audit trail; they would drown out the transitions.
///
/// # Errors
///
/// Returns an error when the database write fails.
pub fn renew_lease(
    conn: &mut SqliteConnection,
    id: LeaseId,
    expires_at_ms: i64,
) -> anyhow::Result<bool> {
    let updated = diesel::update(leases::table.find(id.0))
        .set(leases::expires_at_ms.eq(expires_at_ms))
        .execute(conn)?;
    Ok(updated == 1)
}

/// Moves the heartbeat deadline of a lease that a restarted daemon tracks again, and records
/// the restart in its audit trail. Returns `false` when no lease has `id`.
///
/// # Errors
///
/// Returns an error when the database write fails; nothing is changed in that case.
pub fn restore_lease(
    conn: &mut SqliteConnection,
    id: LeaseId,
    expires_at_ms: i64,
    at_ms: i64,
) -> anyhow::Result<bool> {
    conn.transaction(|conn| {
        let Some(current) = get_lease(conn, id)? else {
            return Ok(false);
        };
        renew_lease(conn, id, expires_at_ms)?;
        append_event(
            conn,
            id,
            at_ms,
            Some(current.state),
            current.state,
            "restored after a daemon restart",
        )?;
        Ok(true)
    })
}

/// What one [`prune`] deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Pruned {
    /// Ended leases, each with its audit trail.
    pub leases: usize,
    /// Job runs.
    pub job_runs: usize,
}

// SQLite limits the parameters of one statement; deletes by ID go in batches below it.
const DELETE_BATCH: usize = 500;

/// Deletes what `retention` no longer keeps at `now_ms`: leases that ended more than
/// `lease_days` ago (job runs stay, unlinked), job runs older than `history_days` except the
/// newest `keep_runs_per_job` of each fingerprint, and then the oldest runs beyond
/// `max_job_runs`.
///
/// # Errors
///
/// Returns an error when a query fails; nothing is deleted in that case.
pub fn prune(
    conn: &mut SqliteConnection,
    retention: &RetentionConfig,
    now_ms: i64,
) -> anyhow::Result<Pruned> {
    let lease_cutoff = now_ms.saturating_sub(days_to_ms(retention.lease_days));
    let history_cutoff = now_ms.saturating_sub(days_to_ms(retention.history_days));
    let keep = i64::try_from(retention.keep_runs_per_job).unwrap_or(i64::MAX);
    let max_runs = i64::try_from(retention.max_job_runs).unwrap_or(i64::MAX);
    conn.transaction(|conn| {
        let ended = [LeaseState::Released.as_str(), LeaseState::Expired.as_str()];
        let leases = diesel::delete(
            leases::table
                .filter(leases::state.eq_any(ended))
                .filter(leases::ended_at_ms.lt(lease_cutoff)),
        )
        .execute(conn)?;

        let mut job_runs = 0;
        let old_fingerprints: Vec<String> = job_runs::table
            .filter(job_runs::ended_at_ms.lt(history_cutoff))
            .select(job_runs::fingerprint)
            .distinct()
            .load(conn)?;
        for fingerprint in old_fingerprints {
            let kept: Vec<i64> = job_runs::table
                .filter(job_runs::fingerprint.eq(&fingerprint))
                .order((job_runs::ended_at_ms.desc(), job_runs::id.desc()))
                .limit(keep)
                .select(job_runs::id)
                .load(conn)?;
            job_runs += diesel::delete(
                job_runs::table
                    .filter(job_runs::fingerprint.eq(&fingerprint))
                    .filter(job_runs::ended_at_ms.lt(history_cutoff))
                    .filter(job_runs::id.ne_all(kept)),
            )
            .execute(conn)?;
        }

        let beyond_cap: Vec<i64> = job_runs::table
            .order((job_runs::ended_at_ms.desc(), job_runs::id.desc()))
            .offset(max_runs)
            .limit(i64::MAX)
            .select(job_runs::id)
            .load(conn)?;
        for batch in beyond_cap.chunks(DELETE_BATCH) {
            job_runs +=
                diesel::delete(job_runs::table.filter(job_runs::id.eq_any(batch))).execute(conn)?;
        }
        Ok(Pruned { leases, job_runs })
    })
}

fn days_to_ms(days: u64) -> i64 {
    i64::try_from(days.saturating_mul(86_400_000)).unwrap_or(i64::MAX)
}

/// Returns the audit trail of a lease, oldest first.
///
/// # Errors
///
/// Returns an error when the query fails or a row holds an unknown state.
pub fn lease_events(conn: &mut SqliteConnection, id: LeaseId) -> anyhow::Result<Vec<LeaseEvent>> {
    lease_events::table
        .filter(lease_events::lease_id.eq(id.0))
        .order(lease_events::id.asc())
        .select(LeaseEventRow::as_select())
        .load(conn)?
        .into_iter()
        .map(LeaseEvent::try_from)
        .collect()
}

/// Stores a finished job.
///
/// # Errors
///
/// Returns an error when a value does not fit its column or the database write fails.
pub fn insert_job_run(conn: &mut SqliteConnection, run: &JobRun) -> anyhow::Result<()> {
    diesel::insert_into(job_runs::table)
        .values(NewJobRunRow::from_run(run)?)
        .execute(conn)?;
    Ok(())
}

/// Returns up to `limit` runs with `fingerprint`, newest first.
///
/// # Errors
///
/// Returns an error when the query fails or a row holds values the shared types reject.
pub fn recent_job_runs(
    conn: &mut SqliteConnection,
    fingerprint: &str,
    limit: u32,
) -> anyhow::Result<Vec<JobRun>> {
    job_runs::table
        .filter(job_runs::fingerprint.eq(fingerprint))
        .order((job_runs::started_at_ms.desc(), job_runs::id.desc()))
        .limit(i64::from(limit))
        .select(JobRunRow::as_select())
        .load(conn)?
        .into_iter()
        .map(JobRun::try_from)
        .collect()
}

/// Returns up to `limit` runs of any fingerprint, newest first.
///
/// # Errors
///
/// Returns an error when the query fails or a row holds values the shared types reject.
pub fn latest_job_runs(conn: &mut SqliteConnection, limit: u32) -> anyhow::Result<Vec<JobRun>> {
    job_runs::table
        .order((job_runs::ended_at_ms.desc(), job_runs::id.desc()))
        .limit(i64::from(limit))
        .select(JobRunRow::as_select())
        .load(conn)?
        .into_iter()
        .map(JobRun::try_from)
        .collect()
}

fn append_event(
    conn: &mut SqliteConnection,
    lease_id: LeaseId,
    at_ms: i64,
    from_state: Option<LeaseState>,
    to_state: LeaseState,
    reason: &str,
) -> QueryResult<()> {
    diesel::insert_into(lease_events::table)
        .values(NewLeaseEventRow::new(
            lease_id, at_ms, from_state, to_state, reason,
        ))
        .execute(conn)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use diesel::sql_query;
    use rh_shared::types::{NoiseReport, ResourceUsage, SessionKey};

    use super::*;
    use crate::db::test_support::{memory_conn, sample_draft};

    fn sample_run(fingerprint: &str, started_at_ms: i64, noise: Option<NoiseReport>) -> JobRun {
        let usage = |cores, ram_mb| ResourceUsage {
            cores,
            ram_mb,
            gpu_pct: 0.0,
            vram_mb: 0,
        };
        JobRun {
            lease_id: None,
            fingerprint: fingerprint.into(),
            command: "cargo build --release".into(),
            cwd: r"E:\work\app".into(),
            declared: sample_draft("s1", 0).profile,
            peak: usage(7.5, 3900),
            avg: usage(5.25, 2800),
            started_at_ms,
            ended_at_ms: started_at_ms + 95_000,
            noise,
            overlapped: false,
        }
    }

    #[test]
    fn test_insert_lease_stores_queued_lease_with_creation_event() {
        let mut conn = memory_conn();
        let draft = sample_draft("s1", 1_000);
        let lease = insert_lease(&mut conn, &draft).unwrap();

        assert_eq!(lease.state, LeaseState::Queued);
        assert_eq!(lease.holder, draft.holder);
        assert_eq!(lease.class, draft.class);
        assert_eq!(lease.profile, draft.profile);
        assert_eq!(lease.cwd, draft.cwd);
        assert_eq!(lease.granted_at_ms, None);
        assert_eq!(get_lease(&mut conn, lease.id).unwrap(), Some(lease.clone()));
        assert_eq!(
            lease_events(&mut conn, lease.id).unwrap(),
            vec![LeaseEvent {
                lease_id: lease.id,
                at_ms: 1_000,
                from_state: None,
                to_state: LeaseState::Queued,
                reason: "requested".into(),
            }]
        );
    }

    #[test]
    fn test_get_lease_unknown_id_is_none() {
        let mut conn = memory_conn();
        assert_eq!(get_lease(&mut conn, LeaseId(404)).unwrap(), None);
    }

    #[test]
    fn test_transition_lease_sets_timestamps_and_records_each_step() {
        let mut conn = memory_conn();
        let id = insert_lease(&mut conn, &sample_draft("s1", 1_000))
            .unwrap()
            .id;

        let active = transition_lease(&mut conn, id, LeaseState::Active, 2_000, "granted")
            .unwrap()
            .unwrap();
        assert_eq!(active.state, LeaseState::Active);
        assert_eq!(active.granted_at_ms, Some(2_000));
        assert_eq!(active.ended_at_ms, None);

        let released = transition_lease(&mut conn, id, LeaseState::Released, 9_000, "released")
            .unwrap()
            .unwrap();
        assert_eq!(released.granted_at_ms, Some(2_000));
        assert_eq!(released.ended_at_ms, Some(9_000));
        assert_eq!(get_lease(&mut conn, id).unwrap(), Some(released));

        let steps: Vec<_> = lease_events(&mut conn, id)
            .unwrap()
            .into_iter()
            .map(|event| (event.from_state, event.to_state, event.at_ms))
            .collect();
        assert_eq!(
            steps,
            [
                (None, LeaseState::Queued, 1_000),
                (Some(LeaseState::Queued), LeaseState::Active, 2_000),
                (Some(LeaseState::Active), LeaseState::Released, 9_000),
            ]
        );
    }

    #[test]
    fn test_transition_lease_unknown_id_is_none_and_records_nothing() {
        let mut conn = memory_conn();
        let result = transition_lease(&mut conn, LeaseId(404), LeaseState::Expired, 1, "reaped");
        assert_eq!(result.unwrap(), None);
        assert!(lease_events(&mut conn, LeaseId(404)).unwrap().is_empty());
    }

    #[test]
    fn test_list_leases_filters_by_session_and_state_oldest_first() {
        let mut conn = memory_conn();
        let first = insert_lease(&mut conn, &sample_draft("s1", 1_000)).unwrap();
        let other = insert_lease(&mut conn, &sample_draft("s2", 2_000)).unwrap();
        let second = insert_lease(&mut conn, &sample_draft("s1", 3_000)).unwrap();
        transition_lease(&mut conn, second.id, LeaseState::Active, 3_500, "granted").unwrap();

        let ids = |query: LeaseQuery, conn: &mut SqliteConnection| -> Vec<LeaseId> {
            list_leases(conn, &query)
                .unwrap()
                .into_iter()
                .map(|lease| lease.id)
                .collect()
        };
        let session = Some(SessionKey("s1".into()));
        assert_eq!(
            ids(LeaseQuery::default(), &mut conn),
            [first.id, other.id, second.id]
        );
        assert_eq!(
            ids(
                LeaseQuery {
                    session: session.clone(),
                    state: None
                },
                &mut conn
            ),
            [first.id, second.id]
        );
        assert_eq!(
            ids(
                LeaseQuery {
                    session,
                    state: Some(LeaseState::Queued)
                },
                &mut conn
            ),
            [first.id]
        );
        assert_eq!(
            ids(
                LeaseQuery {
                    session: None,
                    state: Some(LeaseState::Active)
                },
                &mut conn
            ),
            [second.id]
        );
    }

    #[test]
    fn test_renew_lease_moves_expiry_and_reports_unknown_id() {
        let mut conn = memory_conn();
        let id = insert_lease(&mut conn, &sample_draft("s1", 1_000))
            .unwrap()
            .id;
        assert!(renew_lease(&mut conn, id, 99_000).unwrap());
        assert_eq!(
            get_lease(&mut conn, id).unwrap().unwrap().expires_at_ms,
            99_000
        );
        assert!(!renew_lease(&mut conn, LeaseId(404), 99_000).unwrap());
        assert_eq!(lease_events(&mut conn, id).unwrap().len(), 1);
    }

    #[test]
    fn test_restore_lease_moves_expiry_and_records_the_restart() {
        let mut conn = memory_conn();
        let id = insert_lease(&mut conn, &sample_draft("s1", 1_000))
            .unwrap()
            .id;
        transition_lease(&mut conn, id, LeaseState::Active, 2_000, "granted").unwrap();
        assert!(restore_lease(&mut conn, id, 99_000, 50_000).unwrap());
        assert!(!restore_lease(&mut conn, LeaseId(404), 99_000, 50_000).unwrap());

        let lease = get_lease(&mut conn, id).unwrap().unwrap();
        assert_eq!(
            (lease.state, lease.expires_at_ms),
            (LeaseState::Active, 99_000)
        );
        let last = lease_events(&mut conn, id).unwrap().pop().unwrap();
        assert_eq!(
            (last.from_state, last.to_state, last.at_ms),
            (Some(LeaseState::Active), LeaseState::Active, 50_000)
        );
        assert_eq!(last.reason, "restored after a daemon restart");
    }

    const DAY_MS: i64 = 86_400_000;

    fn retention(keep_runs_per_job: u64, max_job_runs: u64) -> RetentionConfig {
        RetentionConfig {
            lease_days: 30,
            history_days: 90,
            keep_runs_per_job,
            max_job_runs,
        }
    }

    fn ended_lease(conn: &mut SqliteConnection, to_state: LeaseState, ended_at_ms: i64) -> LeaseId {
        let id = insert_lease(conn, &sample_draft("s1", ended_at_ms - 1))
            .unwrap()
            .id;
        transition_lease(conn, id, to_state, ended_at_ms, "ended").unwrap();
        id
    }

    fn run_starts(conn: &mut SqliteConnection, fingerprint: &str) -> Vec<i64> {
        recent_job_runs(conn, fingerprint, 100)
            .unwrap()
            .into_iter()
            .map(|run| run.started_at_ms)
            .collect()
    }

    #[test]
    fn test_prune_deletes_only_leases_that_ended_before_the_cutoff() {
        let mut conn = memory_conn();
        let now = 100 * DAY_MS;
        let old = ended_lease(&mut conn, LeaseState::Released, now - 31 * DAY_MS);
        let old_expired = ended_lease(&mut conn, LeaseState::Expired, now - 40 * DAY_MS);
        let recent = ended_lease(&mut conn, LeaseState::Released, now - 29 * DAY_MS);
        let queued = insert_lease(&mut conn, &sample_draft("s1", 1_000))
            .unwrap()
            .id;
        let run = JobRun {
            lease_id: Some(old),
            ..sample_run("f", now - DAY_MS, None)
        };
        insert_job_run(&mut conn, &run).unwrap();

        let pruned = prune(&mut conn, &retention(20, 5_000), now).unwrap();
        assert_eq!(
            pruned,
            Pruned {
                leases: 2,
                job_runs: 0
            }
        );
        for gone in [old, old_expired] {
            assert_eq!(get_lease(&mut conn, gone).unwrap(), None);
            assert!(lease_events(&mut conn, gone).unwrap().is_empty());
        }
        for kept in [recent, queued] {
            assert!(get_lease(&mut conn, kept).unwrap().is_some());
        }
        // The run outlives its lease, unlinked.
        let stored = recent_job_runs(&mut conn, "f", 1).unwrap();
        assert_eq!(stored[0].lease_id, None);
    }

    #[test]
    fn test_prune_keeps_the_newest_runs_of_each_job_past_the_history_cutoff() {
        let mut conn = memory_conn();
        let now = 1_000 * DAY_MS;
        let old = |days: i64| now - days * DAY_MS - 95_000;
        for start in [old(200), old(150), old(120), old(10)] {
            insert_job_run(&mut conn, &sample_run("busy", start, None)).unwrap();
        }
        for start in [old(300), old(250)] {
            insert_job_run(&mut conn, &sample_run("rare", start, None)).unwrap();
        }

        let pruned = prune(&mut conn, &retention(2, 5_000), now).unwrap();
        assert_eq!(pruned.job_runs, 2);
        assert_eq!(run_starts(&mut conn, "busy"), [old(10), old(120)]);
        assert_eq!(run_starts(&mut conn, "rare"), [old(250), old(300)]);
    }

    #[test]
    fn test_prune_caps_the_job_runs_whatever_their_job() {
        let mut conn = memory_conn();
        let now = 10 * DAY_MS;
        for (fingerprint, start) in [("a", 1_000), ("b", 2_000), ("a", 3_000), ("c", 4_000)] {
            insert_job_run(&mut conn, &sample_run(fingerprint, start, None)).unwrap();
        }
        let pruned = prune(&mut conn, &retention(1, 2), now).unwrap();
        assert_eq!(pruned.job_runs, 2);
        let left: Vec<String> = latest_job_runs(&mut conn, 10)
            .unwrap()
            .into_iter()
            .map(|run| run.fingerprint)
            .collect();
        assert_eq!(left, ["c", "a"]);
    }

    #[test]
    fn test_job_runs_round_trip_newest_first_within_limit() {
        let mut conn = memory_conn();
        let noise = NoiseReport {
            avg_cpu_pct: 1.5,
            peak_cpu_pct: 6.0,
            avg_gpu_pct: 0.0,
            peak_gpu_pct: 2.25,
            quiet_at_start: true,
        };
        let old = sample_run("cargo build|app", 1_000, None);
        let mid = sample_run("cargo build|app", 2_000, Some(noise));
        let new = sample_run("cargo build|app", 3_000, None);
        for run in [&old, &mid, &new, &sample_run("pnpm test|web", 4_000, None)] {
            insert_job_run(&mut conn, run).unwrap();
        }

        assert_eq!(
            recent_job_runs(&mut conn, "cargo build|app", 2).unwrap(),
            [new, mid]
        );
        assert_eq!(
            recent_job_runs(&mut conn, "cargo build|app", 10)
                .unwrap()
                .len(),
            3
        );
        assert!(
            recent_job_runs(&mut conn, "unknown", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_latest_job_runs_spans_fingerprints_newest_first() {
        let mut conn = memory_conn();
        let overlapped = JobRun {
            overlapped: true,
            ..sample_run("pnpm test|web", 5_000, None)
        };
        for run in [
            &sample_run("a", 1_000, None),
            &overlapped,
            &sample_run("b", 3_000, None),
        ] {
            insert_job_run(&mut conn, run).unwrap();
        }
        let fingerprints: Vec<(String, bool)> = latest_job_runs(&mut conn, 2)
            .unwrap()
            .into_iter()
            .map(|run| (run.fingerprint, run.overlapped))
            .collect();
        assert_eq!(
            fingerprints,
            [("pnpm test|web".to_owned(), true), ("b".to_owned(), false)]
        );
    }

    #[test]
    fn test_job_run_links_to_its_lease() {
        let mut conn = memory_conn();
        let lease_id = insert_lease(&mut conn, &sample_draft("s1", 1_000))
            .unwrap()
            .id;
        let run = JobRun {
            lease_id: Some(lease_id),
            ..sample_run("f", 1_000, None)
        };
        insert_job_run(&mut conn, &run).unwrap();
        assert_eq!(recent_job_runs(&mut conn, "f", 1).unwrap(), [run]);
    }

    #[test]
    fn test_job_run_unknown_lease_is_rejected_by_foreign_key() {
        let mut conn = memory_conn();
        let run = JobRun {
            lease_id: Some(LeaseId(404)),
            ..sample_run("f", 1_000, None)
        };
        assert!(insert_job_run(&mut conn, &run).is_err());
    }

    #[test]
    fn test_lease_state_check_constraint_rejects_unknown_name() {
        let mut conn = memory_conn();
        insert_lease(&mut conn, &sample_draft("s1", 1_000)).unwrap();
        let result = sql_query("UPDATE leases SET state = 'paused'").execute(&mut conn);
        assert!(result.is_err());
    }
}
