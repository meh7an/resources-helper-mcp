//! Finding the process tree a reported command runs as.
//!
//! Claude Code starts every Bash and PowerShell command as a fresh direct child of the
//! session's `claude.exe`: Git's `bash.exe` launcher, or `cmd.exe` running `pwsh.exe`. Hooks
//! start short-lived children in the same second, and start times are whole seconds, so the
//! earliest new child may be a hook. It exits within a moment, and the next sample then binds
//! the next earliest child.

use rh_shared::types::{BoundProcess, Holder, LeaseBinding};

use crate::{ancestry::ProcessNode, kill::RH_BINARIES, process::ProcessTable};

/// How long after the report a new child of the holder may still be the command's root.
/// Later children are other commands the session ran while it held the lease.
pub const REBIND_WINDOW_MS: i64 = 10_000;

/// How much earlier than the report a child may appear to start: start times are whole
/// seconds, rounded down.
pub const START_TOLERANCE_MS: i64 = 1_000;

/// Console hosts Windows starts for console children; never the command itself.
const CONSOLE_HOST: &str = "conhost.exe";

/// Where a lease's command stands after looking at one process table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BindOutcome {
    /// The command's root process; store it when it differs from the binding's.
    Bound(BoundProcess),
    /// No root yet, but one may still start.
    Pending,
    /// No root is running and none can start any more: the command has ended, or it ended
    /// before a sample saw it.
    Ended,
}

/// Finds the root of the command `binding` reports, run by `holder`, at `now_ms`.
///
/// The current root is kept while it runs. Otherwise the root is the earliest running direct
/// child of the holder that is not an `rh` binary or a console host and started within
/// [`REBIND_WINDOW_MS`] of the report, allowing [`START_TOLERANCE_MS`] before it.
pub fn resolve(
    binding: &LeaseBinding,
    holder: &Holder,
    table: &ProcessTable,
    now_ms: i64,
) -> BindOutcome {
    if let Some(root) = binding.root
        && table.is_running(root.pid, root.start_time)
    {
        return BindOutcome::Bound(root);
    }
    let earliest = if table.is_running(holder.pid, holder.pid_start_time) {
        table
            .children(holder.pid)
            .filter(|node| is_candidate(node, binding.bound_at_ms))
            .min_by_key(|node| (node.start_time, node.pid))
    } else {
        None
    };
    match earliest {
        Some(node) => BindOutcome::Bound(BoundProcess {
            pid: node.pid,
            start_time: node.start_time,
        }),
        None if now_ms <= binding.bound_at_ms.saturating_add(REBIND_WINDOW_MS) => {
            BindOutcome::Pending
        }
        None => BindOutcome::Ended,
    }
}

/// The busiest descendant of `holder` by `load`, for showing what a lease with no bound root
/// is doing. Never a kill target: nothing ties it to the lease's command.
///
/// `rh` binaries and console hosts are skipped, and processes with no load are never picked.
pub fn busiest_descendant<'a, F>(
    holder: &Holder,
    table: &'a ProcessTable,
    load: F,
) -> Option<&'a ProcessNode>
where
    F: Fn(&ProcessNode) -> f32,
{
    if !table.is_running(holder.pid, holder.pid_start_time) {
        return None;
    }
    let mut busiest: Option<(&ProcessNode, f32)> = None;
    let mut pending: Vec<&ProcessNode> = table.children(holder.pid).collect();
    while let Some(node) = pending.pop() {
        pending.extend(table.children(node.pid));
        if is_excluded(&node.name) {
            continue;
        }
        let node_load = load(node);
        if node_load > 0.0 && busiest.is_none_or(|(_, best)| node_load > best) {
            busiest = Some((node, node_load));
        }
    }
    busiest.map(|(node, _)| node)
}

/// Whether `node` started early enough before `cutoff_ms` that it cannot belong to a command
/// reported at `cutoff_ms`.
pub fn started_before(node: &ProcessNode, cutoff_ms: i64) -> bool {
    start_ms(node) < cutoff_ms.saturating_sub(START_TOLERANCE_MS)
}

fn is_candidate(node: &ProcessNode, bound_at_ms: i64) -> bool {
    !is_excluded(&node.name)
        && !started_before(node, bound_at_ms)
        && start_ms(node) <= bound_at_ms.saturating_add(REBIND_WINDOW_MS)
}

fn is_excluded(name: &str) -> bool {
    name.eq_ignore_ascii_case(CONSOLE_HOST)
        || RH_BINARIES
            .iter()
            .any(|binary| name.eq_ignore_ascii_case(binary))
}

fn start_ms(node: &ProcessNode) -> i64 {
    i64::try_from(node.start_time)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000)
}

#[cfg(test)]
mod tests {
    use rh_shared::types::SessionKey;

    use super::*;
    use crate::{sampler::RawProcess, test_support::raw_process};

    /// The report arrives at 100.5 s; the holder is PID 10, started at 50 s.
    const REPORT_MS: i64 = 100_500;

    fn holder() -> Holder {
        Holder {
            session: SessionKey("s".into()),
            pid: 10,
            pid_start_time: 50,
        }
    }

    fn binding(root: Option<BoundProcess>) -> LeaseBinding {
        LeaseBinding {
            tool_use_id: "toolu_01".into(),
            bound_at_ms: REPORT_MS,
            root,
        }
    }

    fn root(pid: u32, start_time: u64) -> Option<BoundProcess> {
        Some(BoundProcess { pid, start_time })
    }

    fn bound(pid: u32, start_time: u64) -> BindOutcome {
        BindOutcome::Bound(BoundProcess { pid, start_time })
    }

    fn table(extra: &[RawProcess]) -> ProcessTable {
        let mut processes = vec![
            raw_process(1, None, "explorer.exe", 1),
            raw_process(10, Some(1), "claude.exe", 50),
            raw_process(11, Some(10), "rh-mcp.exe", 51),
        ];
        processes.extend_from_slice(extra);
        ProcessTable::new(&processes)
    }

    fn resolve_at(extra: &[RawProcess], root: Option<BoundProcess>, now_ms: i64) -> BindOutcome {
        resolve(&binding(root), &holder(), &table(extra), now_ms)
    }

    #[test]
    fn test_resolve_binds_the_earliest_new_child() {
        let extra = [
            raw_process(30, Some(10), "bash.exe", 102),
            raw_process(20, Some(10), "bash.exe", 101),
            raw_process(21, Some(20), "bash.exe", 101),
        ];
        assert_eq!(resolve_at(&extra, None, 103_000), bound(20, 101));
    }

    #[test]
    fn test_resolve_allows_one_second_before_the_report() {
        // 100 s rounds down from up to 100.999 s, so it may still follow a 100.5 s report.
        let extra = [raw_process(20, Some(10), "cmd.exe", 100)];
        assert_eq!(resolve_at(&extra, None, 101_000), bound(20, 100));
        let extra = [raw_process(20, Some(10), "cmd.exe", 99)];
        assert_eq!(resolve_at(&extra, None, 101_000), BindOutcome::Pending);
    }

    #[test]
    fn test_resolve_skips_rh_binaries_and_console_hosts() {
        let extra = [
            raw_process(20, Some(10), "RH.EXE", 101),
            raw_process(21, Some(10), "conhost.exe", 101),
            raw_process(22, Some(10), "rhd.exe", 101),
            raw_process(23, Some(10), "cmd.exe", 102),
        ];
        assert_eq!(resolve_at(&extra, None, 103_000), bound(23, 102));
    }

    #[test]
    fn test_resolve_skips_grandchildren_and_other_sessions() {
        let extra = [
            raw_process(40, Some(1), "claude.exe", 60),
            raw_process(41, Some(40), "bash.exe", 101),
            raw_process(20, Some(11), "cmd.exe", 101),
        ];
        assert_eq!(resolve_at(&extra, None, 103_000), BindOutcome::Pending);
    }

    #[test]
    fn test_resolve_keeps_a_running_root() {
        let extra = [
            raw_process(20, Some(10), "cmd.exe", 101),
            raw_process(30, Some(10), "bash.exe", 102),
        ];
        assert_eq!(resolve_at(&extra, root(30, 102), 103_000), bound(30, 102));
    }

    #[test]
    fn test_resolve_rebinds_when_a_hook_root_exits() {
        // The hook's cmd.exe (PID 20) was bound and is gone; the command's shell remains.
        let extra = [
            raw_process(30, Some(10), "cmd.exe", 101),
            raw_process(31, Some(30), "pwsh.exe", 101),
        ];
        assert_eq!(resolve_at(&extra, root(20, 101), 120_000), bound(30, 101));
    }

    #[test]
    fn test_resolve_rebinds_past_a_reused_pid() {
        // PID 20 now belongs to a process that started later than the bound root.
        let extra = [
            raw_process(20, Some(10), "git.exe", 105),
            raw_process(30, Some(10), "cmd.exe", 102),
        ];
        assert_eq!(resolve_at(&extra, root(20, 101), 106_000), bound(30, 102));
    }

    #[test]
    fn test_resolve_ignores_children_after_the_rebind_window() {
        let extra = [raw_process(20, Some(10), "git.exe", 111)];
        let ended = root(30, 101);
        assert_eq!(resolve_at(&extra, ended, 105_000), BindOutcome::Pending);
        assert_eq!(resolve_at(&extra, ended, 112_000), BindOutcome::Ended);
        assert_eq!(resolve_at(&extra, None, 110_500), BindOutcome::Pending);
        assert_eq!(resolve_at(&extra, None, 110_501), BindOutcome::Ended);
    }

    #[test]
    fn test_resolve_finds_nothing_under_a_dead_or_reused_holder() {
        let extra = [raw_process(20, Some(10), "cmd.exe", 101)];
        let mut reused = holder();
        reused.pid_start_time = 49;
        assert_eq!(
            resolve(&binding(None), &reused, &table(&extra), 103_000),
            BindOutcome::Pending
        );
    }

    #[test]
    fn test_busiest_descendant_picks_the_heaviest_non_rh_process() {
        let extra = [
            raw_process(20, Some(10), "bash.exe", 101),
            raw_process(21, Some(20), "cargo.exe", 101),
            raw_process(22, Some(21), "rustc.exe", 102),
            raw_process(23, Some(40), "rustc.exe", 102),
        ];
        let table = table(&extra);
        let load = |node: &ProcessNode| match node.pid {
            11 => 9.0,
            21 => 0.5,
            22 => 3.0,
            23 => 8.0,
            _ => 0.0,
        };
        let busiest = busiest_descendant(&holder(), &table, load).unwrap();
        assert_eq!(busiest.pid, 22);
    }

    #[test]
    fn test_busiest_descendant_is_none_when_idle() {
        let extra = [raw_process(20, Some(10), "bash.exe", 101)];
        assert!(busiest_descendant(&holder(), &table(&extra), |_| 0.0).is_none());
    }
}
