//! Queries over leases, their audit trail and notices, job history, and installation facts.
//!
//! Every function takes a plain connection so callers decide the transaction scope. Functions
//! that write a lease and its event do so in one transaction, so the audit trail cannot drift
//! from the lease row.

use anyhow::{Context, bail};
use diesel::prelude::*;
use rh_shared::{
    api::LeaseQuery,
    config::RetentionConfig,
    types::{
        EndCause, Holder, JobClass, JobProfile, JobRun, Lease, LeaseBinding, LeaseId, LeaseKind,
        LeaseState, SessionKey,
    },
    ui::{Notice, RecentRun},
};

use super::{
    models::{
        BindingChanges, JobRunRow, LeaseEventRow, LeaseRow, NewJobRunRow, NewLeaseEventRow,
        NewLeaseRow,
    },
    schema::{job_runs, lease_events, leases, meta},
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

/// The working directory of the newest lease of `session` that named one, which names the
/// session's project.
///
/// # Errors
///
/// Returns an error when the query fails.
pub fn latest_cwd(
    conn: &mut SqliteConnection,
    session: &SessionKey,
) -> anyhow::Result<Option<String>> {
    Ok(leases::table
        .filter(leases::session_id.eq(session.0.as_str()))
        .filter(leases::cwd.ne(""))
        .order(leases::id.desc())
        .select(leases::cwd)
        .first(conn)
        .optional()?)
}

/// Moves a lease to `to_state` and records the transition. Entering `Active` sets
/// `granted_at_ms`; entering `Released` or `Expired` sets `ended_at_ms` and the cause of the
/// same name. [`end_lease`] records the other causes.
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
    let cause = match to_state {
        LeaseState::Released => Some(EndCause::Released),
        LeaseState::Expired => Some(EndCause::Expired),
        _ => None,
    };
    change_state(conn, id, to_state, cause, at_ms, reason)
}

/// Ends a lease with `cause`, in the state [`EndCause::ended_state`] names, and records the
/// transition. A kill or removal becomes a notice for the holder's session.
///
/// Returns `None` when no lease has `id`.
///
/// # Errors
///
/// Returns an error when the database write fails; nothing is changed in that case.
pub fn end_lease(
    conn: &mut SqliteConnection,
    id: LeaseId,
    cause: EndCause,
    at_ms: i64,
    reason: &str,
) -> anyhow::Result<Option<Lease>> {
    change_state(conn, id, cause.ended_state(), Some(cause), at_ms, reason)
}

fn change_state(
    conn: &mut SqliteConnection,
    id: LeaseId,
    to_state: LeaseState,
    cause: Option<EndCause>,
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
        let end_cause = cause.or(current.end_cause);
        let row = diesel::update(leases::table.find(id.0))
            .set((
                leases::state.eq(to_state.as_str()),
                leases::granted_at_ms.eq(granted_at_ms),
                leases::ended_at_ms.eq(ended_at_ms),
                leases::end_cause.eq(end_cause.map(EndCause::as_str)),
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

/// Stores `binding` as the command lease `id` covers, replacing an earlier one. Returns
/// `false` when no lease has `id`.
///
/// Bindings are not recorded in the audit trail; a lease held around several commands is
/// rebound for each.
///
/// # Errors
///
/// Returns an error when a value does not fit its column or the database write fails.
pub fn bind_lease(
    conn: &mut SqliteConnection,
    id: LeaseId,
    binding: &LeaseBinding,
) -> anyhow::Result<bool> {
    let updated = diesel::update(leases::table.find(id.0))
        .set(BindingChanges::from_binding(binding)?)
        .execute(conn)?;
    Ok(updated == 1)
}

/// Kills and removals `session` has not been told about yet, oldest first.
///
/// # Errors
///
/// Returns an error when the query fails or a row holds values the shared types reject.
pub fn undelivered_notices(
    conn: &mut SqliteConnection,
    session: &SessionKey,
) -> anyhow::Result<Vec<Notice>> {
    leases::table
        .filter(leases::session_id.eq(session.0.as_str()))
        .filter(leases::end_cause.eq_any(notice_causes()))
        .filter(leases::notified_at_ms.is_null())
        .order(leases::id.asc())
        .select((LeaseRow::as_select(), leases::notified_at_ms))
        .load(conn)?
        .into_iter()
        .map(notice)
        .collect()
}

/// Up to `limit` kills and removals of any session, delivered or not, newest end first.
///
/// # Errors
///
/// Returns an error when the query fails or a row holds values the shared types reject.
pub fn recent_notices(conn: &mut SqliteConnection, limit: u32) -> anyhow::Result<Vec<Notice>> {
    leases::table
        .filter(leases::end_cause.eq_any(notice_causes()))
        .order((leases::ended_at_ms.desc(), leases::id.desc()))
        .limit(i64::from(limit))
        .select((LeaseRow::as_select(), leases::notified_at_ms))
        .load(conn)?
        .into_iter()
        .map(notice)
        .collect()
}

/// Records that the holder's session was told at `at_ms` about the kill or removal of lease
/// `id`. Returns `false` when the lease has no notice or it was already delivered, which keeps
/// the first delivery time.
///
/// # Errors
///
/// Returns an error when the database write fails.
pub fn mark_notice_delivered(
    conn: &mut SqliteConnection,
    id: LeaseId,
    at_ms: i64,
) -> anyhow::Result<bool> {
    let updated = diesel::update(
        leases::table
            .filter(leases::id.eq(id.0))
            .filter(leases::end_cause.eq_any(notice_causes()))
            .filter(leases::notified_at_ms.is_null()),
    )
    .set(leases::notified_at_ms.eq(at_ms))
    .execute(conn)?;
    Ok(updated == 1)
}

fn notice_causes() -> Vec<&'static str> {
    EndCause::ALL
        .into_iter()
        .filter(|cause| cause.is_notice())
        .map(EndCause::as_str)
        .collect()
}

fn notice((row, delivered_at_ms): (LeaseRow, Option<i64>)) -> anyhow::Result<Notice> {
    let lease = Lease::try_from(row)?;
    let (Some(cause), Some(ended_at_ms)) = (lease.end_cause, lease.ended_at_ms) else {
        bail!("lease {} has a notice cause but no end time", lease.id.0);
    };
    Ok(Notice {
        lease_id: lease.id,
        session: lease.holder.session,
        cause,
        command: lease.label,
        ended_at_ms,
        delivered_at_ms,
    })
}

/// Records that a Claude session connected at `at_ms`, unless one already had, and returns
/// when the first one did.
///
/// # Errors
///
/// Returns an error when the database write fails or the `meta` row is missing.
pub fn note_first_session(conn: &mut SqliteConnection, at_ms: i64) -> anyhow::Result<i64> {
    conn.transaction(|conn| {
        diesel::update(meta::table.filter(meta::first_session_at_ms.is_null()))
            .set(meta::first_session_at_ms.eq(at_ms))
            .execute(conn)?;
        first_session_at_ms(conn)?.context("the meta row is missing")
    })
}

/// When a Claude session first connected, or `None` before any has.
///
/// # Errors
///
/// Returns an error when the query fails.
pub fn first_session_at_ms(conn: &mut SqliteConnection) -> anyhow::Result<Option<i64>> {
    let at_ms = meta::table
        .select(meta::first_session_at_ms)
        .first::<Option<i64>>(conn)
        .optional()?;
    Ok(at_ms.flatten())
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

/// Up to `limit` finished runs of any job, newest end first, as the flyout lists them.
///
/// # Errors
///
/// Returns an error when the query fails or a row holds values the shared types reject.
pub fn recent_runs(conn: &mut SqliteConnection, limit: u32) -> anyhow::Result<Vec<RecentRun>> {
    Ok(latest_job_runs(conn, limit)?
        .into_iter()
        .map(RecentRun::from)
        .collect())
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
    use rh_shared::types::{BoundProcess, BusyUsage, NoiseReport, ResourceUsage};

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
            session: None,
            end_cause: None,
            fingerprint: fingerprint.into(),
            command: "cargo build --release".into(),
            cwd: r"E:\work\app".into(),
            declared: sample_draft("s1", 0).profile,
            peak: usage(7.5, 3900),
            avg: usage(5.25, 2800),
            busy: Some(BusyUsage {
                cores: 6.5,
                gpu_pct: 0.0,
            }),
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
    fn test_latest_cwd_skips_leases_without_one_and_other_sessions() {
        let mut conn = memory_conn();
        let session = sample_draft("s1", 0).holder.session;
        assert_eq!(latest_cwd(&mut conn, &session).unwrap(), None);

        let older = LeaseDraft {
            cwd: r"E:\work\old".into(),
            ..sample_draft("s1", 1_000)
        };
        let newer = LeaseDraft {
            cwd: r"E:\work\new".into(),
            ..sample_draft("s1", 2_000)
        };
        let without = LeaseDraft {
            cwd: String::new(),
            ..sample_draft("s1", 3_000)
        };
        let other = LeaseDraft {
            cwd: r"E:\work\other".into(),
            ..sample_draft("s2", 4_000)
        };
        for draft in [older, newer, without, other] {
            insert_lease(&mut conn, &draft).unwrap();
        }
        assert_eq!(
            latest_cwd(&mut conn, &session).unwrap().as_deref(),
            Some(r"E:\work\new")
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
    fn test_job_run_without_busy_load_reads_back_as_none() {
        let mut conn = memory_conn();
        let older = JobRun {
            busy: None,
            ..sample_run("f", 1_000, None)
        };
        insert_job_run(&mut conn, &older).unwrap();
        assert_eq!(recent_job_runs(&mut conn, "f", 1).unwrap(), [older]);

        sql_query("UPDATE job_runs SET busy_cores = 1.5")
            .execute(&mut conn)
            .unwrap();
        let error = recent_job_runs(&mut conn, "f", 1).unwrap_err();
        assert!(format!("{error:#}").contains("busy columns"), "{error:#}");
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

    fn active_lease(conn: &mut SqliteConnection, session: &str) -> LeaseId {
        let id = insert_lease(conn, &sample_draft(session, 1_000))
            .unwrap()
            .id;
        transition_lease(conn, id, LeaseState::Active, 2_000, "granted").unwrap();
        id
    }

    fn notice_ids(notices: Vec<Notice>) -> Vec<LeaseId> {
        notices.into_iter().map(|notice| notice.lease_id).collect()
    }

    #[test]
    fn test_transition_lease_into_an_ended_state_records_the_matching_cause() {
        let mut conn = memory_conn();
        let active = active_lease(&mut conn, "s1");
        assert_eq!(
            get_lease(&mut conn, active).unwrap().unwrap().end_cause,
            None
        );
        let released = transition_lease(&mut conn, active, LeaseState::Released, 3_000, "done");
        assert_eq!(
            released.unwrap().unwrap().end_cause,
            Some(EndCause::Released)
        );

        let queued = insert_lease(&mut conn, &sample_draft("s1", 1_000))
            .unwrap()
            .id;
        let expired = transition_lease(&mut conn, queued, LeaseState::Expired, 3_000, "reaped");
        assert_eq!(expired.unwrap().unwrap().end_cause, Some(EndCause::Expired));
    }

    #[test]
    fn test_end_lease_kill_and_removal_end_expired_with_their_cause() {
        let mut conn = memory_conn();
        let active = active_lease(&mut conn, "s1");
        let killed = end_lease(
            &mut conn,
            active,
            EndCause::Killed,
            5_000,
            "killed by the user",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            (killed.state, killed.end_cause, killed.ended_at_ms),
            (LeaseState::Expired, Some(EndCause::Killed), Some(5_000))
        );
        let last = lease_events(&mut conn, active).unwrap().pop().unwrap();
        assert_eq!(
            (last.from_state, last.to_state, last.reason.as_str()),
            (
                Some(LeaseState::Active),
                LeaseState::Expired,
                "killed by the user"
            )
        );

        let queued = insert_lease(&mut conn, &sample_draft("s1", 1_000))
            .unwrap()
            .id;
        let removed = end_lease(&mut conn, queued, EndCause::Removed, 6_000, "removed")
            .unwrap()
            .unwrap();
        assert_eq!(
            (removed.state, removed.end_cause, removed.granted_at_ms),
            (LeaseState::Expired, Some(EndCause::Removed), None)
        );
        assert_eq!(
            end_lease(&mut conn, LeaseId(404), EndCause::Killed, 1, "x").unwrap(),
            None
        );
    }

    #[test]
    fn test_notice_is_returned_until_delivered_then_never_again() {
        let mut conn = memory_conn();
        let killed = active_lease(&mut conn, "s1");
        end_lease(&mut conn, killed, EndCause::Killed, 5_000, "killed").unwrap();
        let removed = insert_lease(&mut conn, &sample_draft("s1", 1_000))
            .unwrap()
            .id;
        end_lease(&mut conn, removed, EndCause::Removed, 6_000, "removed").unwrap();
        let released = active_lease(&mut conn, "s1");
        transition_lease(&mut conn, released, LeaseState::Released, 7_000, "done").unwrap();
        let other = active_lease(&mut conn, "s2");
        end_lease(&mut conn, other, EndCause::Killed, 8_000, "killed").unwrap();

        let session = SessionKey("s1".into());
        let pending = undelivered_notices(&mut conn, &session).unwrap();
        assert_eq!(
            pending[0],
            Notice {
                lease_id: killed,
                session: session.clone(),
                cause: EndCause::Killed,
                command: "cargo build --release".into(),
                ended_at_ms: 5_000,
                delivered_at_ms: None,
            }
        );
        assert_eq!(notice_ids(pending), [killed, removed]);

        assert!(mark_notice_delivered(&mut conn, killed, 9_000).unwrap());
        assert_eq!(
            notice_ids(undelivered_notices(&mut conn, &session).unwrap()),
            [removed]
        );
        assert!(!mark_notice_delivered(&mut conn, killed, 9_500).unwrap());
        assert!(mark_notice_delivered(&mut conn, removed, 9_600).unwrap());
        assert!(undelivered_notices(&mut conn, &session).unwrap().is_empty());

        let recent = recent_notices(&mut conn, 10).unwrap();
        assert_eq!(notice_ids(recent.clone()), [other, removed, killed]);
        assert_eq!(recent[2].delivered_at_ms, Some(9_000));
        assert_eq!(notice_ids(recent_notices(&mut conn, 1).unwrap()), [other]);
    }

    #[test]
    fn test_mark_notice_delivered_without_a_notice_changes_nothing() {
        let mut conn = memory_conn();
        let released = active_lease(&mut conn, "s1");
        transition_lease(&mut conn, released, LeaseState::Released, 7_000, "done").unwrap();
        let active = active_lease(&mut conn, "s1");
        for id in [released, active, LeaseId(404)] {
            assert!(!mark_notice_delivered(&mut conn, id, 9_000).unwrap());
        }
        assert!(recent_notices(&mut conn, 10).unwrap().is_empty());
    }

    fn binding(root: Option<BoundProcess>) -> LeaseBinding {
        LeaseBinding {
            tool_use_id: "toolu_01AbC".into(),
            bound_at_ms: 2_500,
            root,
        }
    }

    const ROOT: BoundProcess = BoundProcess {
        pid: 30_112,
        start_time: 1_793_000_013,
    };

    #[test]
    fn test_bind_lease_stores_the_binding_and_a_new_report_replaces_it() {
        let mut conn = memory_conn();
        let id = active_lease(&mut conn, "s1");
        assert_eq!(get_lease(&mut conn, id).unwrap().unwrap().binding, None);

        assert!(bind_lease(&mut conn, id, &binding(None)).unwrap());
        assert_eq!(
            get_lease(&mut conn, id).unwrap().unwrap().binding,
            Some(binding(None))
        );
        assert!(bind_lease(&mut conn, id, &binding(Some(ROOT))).unwrap());
        assert_eq!(
            get_lease(&mut conn, id).unwrap().unwrap().binding,
            Some(binding(Some(ROOT)))
        );

        let next = LeaseBinding {
            tool_use_id: "toolu_02XyZ".into(),
            bound_at_ms: 4_000,
            root: None,
        };
        assert!(bind_lease(&mut conn, id, &next).unwrap());
        assert_eq!(
            get_lease(&mut conn, id).unwrap().unwrap().binding,
            Some(next)
        );
        assert!(!bind_lease(&mut conn, LeaseId(404), &binding(None)).unwrap());
        assert_eq!(lease_events(&mut conn, id).unwrap().len(), 2);
    }

    #[test]
    fn test_restored_lease_keeps_its_binding() {
        let mut conn = memory_conn();
        let id = active_lease(&mut conn, "s1");
        bind_lease(&mut conn, id, &binding(Some(ROOT))).unwrap();
        assert!(restore_lease(&mut conn, id, 99_000, 50_000).unwrap());
        let lease = get_lease(&mut conn, id).unwrap().unwrap();
        assert_eq!(lease.binding, Some(binding(Some(ROOT))));
        assert_eq!(lease.expires_at_ms, 99_000);
    }

    #[test]
    fn test_lease_with_partly_set_binding_columns_is_rejected() {
        let mut conn = memory_conn();
        let id = active_lease(&mut conn, "s1");
        sql_query("UPDATE leases SET bound_pid = 30112")
            .execute(&mut conn)
            .unwrap();
        let error = get_lease(&mut conn, id).unwrap_err();
        assert!(format!("{error:#}").contains("bound process"), "{error:#}");

        sql_query("UPDATE leases SET bound_pid = NULL, bound_at_ms = 2500")
            .execute(&mut conn)
            .unwrap();
        let error = get_lease(&mut conn, id).unwrap_err();
        assert!(
            format!("{error:#}").contains("binding columns"),
            "{error:#}"
        );
    }

    #[test]
    fn test_lease_end_cause_check_constraint_rejects_unknown_name() {
        let mut conn = memory_conn();
        active_lease(&mut conn, "s1");
        let result = sql_query("UPDATE leases SET end_cause = 'paused'").execute(&mut conn);
        assert!(result.is_err());
    }

    #[test]
    fn test_recent_runs_carry_session_and_cause_newest_first() {
        let mut conn = memory_conn();
        let lease_id = active_lease(&mut conn, "s1");
        let killed = JobRun {
            lease_id: Some(lease_id),
            session: Some(SessionKey("s1".into())),
            end_cause: Some(EndCause::Killed),
            ..sample_run("f", 3_000, None)
        };
        insert_job_run(&mut conn, &sample_run("f", 1_000, None)).unwrap();
        insert_job_run(&mut conn, &killed).unwrap();
        assert_eq!(
            recent_job_runs(&mut conn, "f", 1).unwrap(),
            std::slice::from_ref(&killed)
        );

        let recent = recent_runs(&mut conn, 10).unwrap();
        let older = sample_run("f", 1_000, None);
        assert_eq!(recent, [RecentRun::from(killed), RecentRun::from(older)]);
        assert_eq!(recent[0].end_cause, Some(EndCause::Killed));
        assert_eq!(recent[1].session, None);
    }

    #[test]
    fn test_first_session_is_recorded_once() {
        let mut conn = memory_conn();
        assert_eq!(first_session_at_ms(&mut conn).unwrap(), None);
        assert_eq!(note_first_session(&mut conn, 5_000).unwrap(), 5_000);
        assert_eq!(note_first_session(&mut conn, 9_000).unwrap(), 5_000);
        assert_eq!(first_session_at_ms(&mut conn).unwrap(), Some(5_000));
    }

    #[test]
    fn test_meta_check_constraint_allows_only_one_row() {
        let mut conn = memory_conn();
        let result = sql_query("INSERT INTO meta (id, first_session_at_ms) VALUES (2, 1)")
            .execute(&mut conn);
        assert!(result.is_err());
    }
}
