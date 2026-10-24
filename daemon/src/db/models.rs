//! Diesel row structs and their conversions to the shared types.
//!
//! SQLite has no unsigned integers, so every `u64`/`u32` is stored as `i64` and checked on
//! the way back out. Enums are stored as their `as_str` names.

use anyhow::{Context, anyhow, bail};
use diesel::prelude::*;
use rh_shared::types::{
    BusyUsage, Holder, JobClass, JobProfile, JobRun, Lease, LeaseId, LeaseKind, LeaseState,
    NoiseReport, ResourceUsage, SessionKey,
};

use super::{
    repo::{LeaseDraft, LeaseEvent},
    schema::{job_runs, lease_events, leases},
};

#[derive(Debug, Queryable, Selectable)]
#[diesel(table_name = leases, check_for_backend(diesel::sqlite::Sqlite))]
pub(super) struct LeaseRow {
    id: i64,
    session_id: String,
    holder_pid: i64,
    holder_pid_start_time: i64,
    kind: String,
    state: String,
    class: Option<String>,
    cores: f32,
    ram_mb: i64,
    gpu_pct: f32,
    vram_mb: i64,
    est_secs: i64,
    label: String,
    requested_at_ms: i64,
    granted_at_ms: Option<i64>,
    ended_at_ms: Option<i64>,
    expires_at_ms: i64,
    cwd: String,
}

impl TryFrom<LeaseRow> for Lease {
    type Error = anyhow::Error;

    fn try_from(row: LeaseRow) -> anyhow::Result<Self> {
        Ok(Self {
            id: LeaseId(row.id),
            holder: Holder {
                session: SessionKey(row.session_id),
                pid: from_column(row.holder_pid, "leases.holder_pid")?,
                pid_start_time: from_column(
                    row.holder_pid_start_time,
                    "leases.holder_pid_start_time",
                )?,
            },
            kind: parse_name(&LeaseKind::ALL, LeaseKind::as_str, &row.kind, "leases.kind")?,
            state: parse_name(
                &LeaseState::ALL,
                LeaseState::as_str,
                &row.state,
                "leases.state",
            )?,
            class: row
                .class
                .as_deref()
                .map(|name| parse_name(&JobClass::ALL, JobClass::as_str, name, "leases.class"))
                .transpose()?,
            profile: JobProfile {
                cores: row.cores,
                ram_mb: from_column(row.ram_mb, "leases.ram_mb")?,
                gpu_pct: row.gpu_pct,
                vram_mb: from_column(row.vram_mb, "leases.vram_mb")?,
                est_secs: from_column(row.est_secs, "leases.est_secs")?,
            },
            label: row.label,
            cwd: row.cwd,
            requested_at_ms: row.requested_at_ms,
            granted_at_ms: row.granted_at_ms,
            ended_at_ms: row.ended_at_ms,
            expires_at_ms: row.expires_at_ms,
            end_cause: None,
        })
    }
}

/// A new lease row; it always starts `Queued`.
#[derive(Debug, Insertable)]
#[diesel(table_name = leases)]
pub(super) struct NewLeaseRow<'a> {
    session_id: &'a str,
    holder_pid: i64,
    holder_pid_start_time: i64,
    kind: &'static str,
    state: &'static str,
    class: Option<&'static str>,
    cores: f32,
    ram_mb: i64,
    gpu_pct: f32,
    vram_mb: i64,
    est_secs: i64,
    label: &'a str,
    cwd: &'a str,
    requested_at_ms: i64,
    expires_at_ms: i64,
}

impl<'a> NewLeaseRow<'a> {
    pub(super) fn from_draft(draft: &'a LeaseDraft) -> anyhow::Result<Self> {
        Ok(Self {
            session_id: &draft.holder.session.0,
            holder_pid: i64::from(draft.holder.pid),
            holder_pid_start_time: to_column(draft.holder.pid_start_time, "holder.pid_start_time")?,
            kind: draft.kind.as_str(),
            state: LeaseState::Queued.as_str(),
            class: draft.class.map(JobClass::as_str),
            cores: draft.profile.cores,
            ram_mb: to_column(draft.profile.ram_mb, "profile.ram_mb")?,
            gpu_pct: draft.profile.gpu_pct,
            vram_mb: to_column(draft.profile.vram_mb, "profile.vram_mb")?,
            est_secs: to_column(draft.profile.est_secs, "profile.est_secs")?,
            label: &draft.label,
            cwd: &draft.cwd,
            requested_at_ms: draft.requested_at_ms,
            expires_at_ms: draft.expires_at_ms,
        })
    }
}

#[derive(Debug, Queryable, Selectable)]
#[diesel(table_name = lease_events, check_for_backend(diesel::sqlite::Sqlite))]
pub(super) struct LeaseEventRow {
    lease_id: i64,
    at_ms: i64,
    from_state: Option<String>,
    to_state: String,
    reason: String,
}

impl TryFrom<LeaseEventRow> for LeaseEvent {
    type Error = anyhow::Error;

    fn try_from(row: LeaseEventRow) -> anyhow::Result<Self> {
        let state =
            |name: &str, column| parse_name(&LeaseState::ALL, LeaseState::as_str, name, column);
        Ok(Self {
            lease_id: LeaseId(row.lease_id),
            at_ms: row.at_ms,
            from_state: row
                .from_state
                .as_deref()
                .map(|name| state(name, "lease_events.from_state"))
                .transpose()?,
            to_state: state(&row.to_state, "lease_events.to_state")?,
            reason: row.reason,
        })
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = lease_events)]
pub(super) struct NewLeaseEventRow<'a> {
    lease_id: i64,
    at_ms: i64,
    from_state: Option<&'static str>,
    to_state: &'static str,
    reason: &'a str,
}

impl<'a> NewLeaseEventRow<'a> {
    pub(super) fn new(
        lease_id: LeaseId,
        at_ms: i64,
        from_state: Option<LeaseState>,
        to_state: LeaseState,
        reason: &'a str,
    ) -> Self {
        Self {
            lease_id: lease_id.0,
            at_ms,
            from_state: from_state.map(LeaseState::as_str),
            to_state: to_state.as_str(),
            reason,
        }
    }
}

#[derive(Debug, Queryable, Selectable)]
#[diesel(table_name = job_runs, check_for_backend(diesel::sqlite::Sqlite))]
pub(super) struct JobRunRow {
    lease_id: Option<i64>,
    fingerprint: String,
    command: String,
    cwd: String,
    declared_cores: f32,
    declared_ram_mb: i64,
    declared_gpu_pct: f32,
    declared_vram_mb: i64,
    declared_est_secs: i64,
    peak_cores: f32,
    avg_cores: f32,
    peak_ram_mb: i64,
    avg_ram_mb: i64,
    peak_gpu_pct: f32,
    avg_gpu_pct: f32,
    peak_vram_mb: i64,
    avg_vram_mb: i64,
    started_at_ms: i64,
    ended_at_ms: i64,
    noise_avg_cpu_pct: Option<f32>,
    noise_peak_cpu_pct: Option<f32>,
    noise_avg_gpu_pct: Option<f32>,
    noise_peak_gpu_pct: Option<f32>,
    noise_quiet_at_start: Option<bool>,
    overlapped: bool,
    busy_cores: Option<f32>,
    busy_gpu_pct: Option<f32>,
}

impl TryFrom<JobRunRow> for JobRun {
    type Error = anyhow::Error;

    fn try_from(row: JobRunRow) -> anyhow::Result<Self> {
        let noise = match (
            row.noise_avg_cpu_pct,
            row.noise_peak_cpu_pct,
            row.noise_avg_gpu_pct,
            row.noise_peak_gpu_pct,
            row.noise_quiet_at_start,
        ) {
            (
                Some(avg_cpu_pct),
                Some(peak_cpu_pct),
                Some(avg_gpu_pct),
                Some(peak_gpu_pct),
                Some(quiet_at_start),
            ) => Some(NoiseReport {
                avg_cpu_pct,
                peak_cpu_pct,
                avg_gpu_pct,
                peak_gpu_pct,
                quiet_at_start,
            }),
            (None, None, None, None, None) => None,
            _ => bail!("job_runs noise columns are only partly set"),
        };
        let busy = match (row.busy_cores, row.busy_gpu_pct) {
            (Some(cores), Some(gpu_pct)) => Some(BusyUsage { cores, gpu_pct }),
            (None, None) => None,
            _ => bail!("job_runs busy columns are only partly set"),
        };
        Ok(Self {
            lease_id: row.lease_id.map(LeaseId),
            fingerprint: row.fingerprint,
            command: row.command,
            cwd: row.cwd,
            declared: JobProfile {
                cores: row.declared_cores,
                ram_mb: from_column(row.declared_ram_mb, "job_runs.declared_ram_mb")?,
                gpu_pct: row.declared_gpu_pct,
                vram_mb: from_column(row.declared_vram_mb, "job_runs.declared_vram_mb")?,
                est_secs: from_column(row.declared_est_secs, "job_runs.declared_est_secs")?,
            },
            peak: ResourceUsage {
                cores: row.peak_cores,
                ram_mb: from_column(row.peak_ram_mb, "job_runs.peak_ram_mb")?,
                gpu_pct: row.peak_gpu_pct,
                vram_mb: from_column(row.peak_vram_mb, "job_runs.peak_vram_mb")?,
            },
            avg: ResourceUsage {
                cores: row.avg_cores,
                ram_mb: from_column(row.avg_ram_mb, "job_runs.avg_ram_mb")?,
                gpu_pct: row.avg_gpu_pct,
                vram_mb: from_column(row.avg_vram_mb, "job_runs.avg_vram_mb")?,
            },
            busy,
            started_at_ms: row.started_at_ms,
            ended_at_ms: row.ended_at_ms,
            noise,
            overlapped: row.overlapped,
        })
    }
}

#[derive(Debug, Insertable)]
#[diesel(table_name = job_runs)]
pub(super) struct NewJobRunRow<'a> {
    lease_id: Option<i64>,
    fingerprint: &'a str,
    command: &'a str,
    cwd: &'a str,
    declared_cores: f32,
    declared_ram_mb: i64,
    declared_gpu_pct: f32,
    declared_vram_mb: i64,
    declared_est_secs: i64,
    peak_cores: f32,
    avg_cores: f32,
    peak_ram_mb: i64,
    avg_ram_mb: i64,
    peak_gpu_pct: f32,
    avg_gpu_pct: f32,
    peak_vram_mb: i64,
    avg_vram_mb: i64,
    started_at_ms: i64,
    ended_at_ms: i64,
    noise_avg_cpu_pct: Option<f32>,
    noise_peak_cpu_pct: Option<f32>,
    noise_avg_gpu_pct: Option<f32>,
    noise_peak_gpu_pct: Option<f32>,
    noise_quiet_at_start: Option<bool>,
    overlapped: bool,
    busy_cores: Option<f32>,
    busy_gpu_pct: Option<f32>,
}

impl<'a> NewJobRunRow<'a> {
    pub(super) fn from_run(run: &'a JobRun) -> anyhow::Result<Self> {
        Ok(Self {
            lease_id: run.lease_id.map(|id| id.0),
            fingerprint: &run.fingerprint,
            command: &run.command,
            cwd: &run.cwd,
            declared_cores: run.declared.cores,
            declared_ram_mb: to_column(run.declared.ram_mb, "declared.ram_mb")?,
            declared_gpu_pct: run.declared.gpu_pct,
            declared_vram_mb: to_column(run.declared.vram_mb, "declared.vram_mb")?,
            declared_est_secs: to_column(run.declared.est_secs, "declared.est_secs")?,
            peak_cores: run.peak.cores,
            avg_cores: run.avg.cores,
            peak_ram_mb: to_column(run.peak.ram_mb, "peak.ram_mb")?,
            avg_ram_mb: to_column(run.avg.ram_mb, "avg.ram_mb")?,
            peak_gpu_pct: run.peak.gpu_pct,
            avg_gpu_pct: run.avg.gpu_pct,
            peak_vram_mb: to_column(run.peak.vram_mb, "peak.vram_mb")?,
            avg_vram_mb: to_column(run.avg.vram_mb, "avg.vram_mb")?,
            started_at_ms: run.started_at_ms,
            ended_at_ms: run.ended_at_ms,
            noise_avg_cpu_pct: run.noise.map(|noise| noise.avg_cpu_pct),
            noise_peak_cpu_pct: run.noise.map(|noise| noise.peak_cpu_pct),
            noise_avg_gpu_pct: run.noise.map(|noise| noise.avg_gpu_pct),
            noise_peak_gpu_pct: run.noise.map(|noise| noise.peak_gpu_pct),
            noise_quiet_at_start: run.noise.map(|noise| noise.quiet_at_start),
            overlapped: run.overlapped,
            busy_cores: run.busy.map(|busy| busy.cores),
            busy_gpu_pct: run.busy.map(|busy| busy.gpu_pct),
        })
    }
}

fn from_column<T: TryFrom<i64>>(value: i64, column: &str) -> anyhow::Result<T> {
    T::try_from(value).map_err(|_| anyhow!("{column} holds {value}, which is out of range"))
}

fn to_column(value: u64, field: &str) -> anyhow::Result<i64> {
    i64::try_from(value).with_context(|| format!("{field} is {value}, too large to store"))
}

fn parse_name<T: Copy>(
    all: &[T],
    name: fn(T) -> &'static str,
    text: &str,
    column: &str,
) -> anyhow::Result<T> {
    all.iter()
        .copied()
        .find(|&item| name(item) == text)
        .with_context(|| format!("{column} holds unknown value {text:?}"))
}
