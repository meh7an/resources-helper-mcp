//! Planning a kill: exactly the process tree bound to a lease, never anything that runs the
//! session or the coordinator.
//!
//! The plan is terminated top-down, root first: a shell killed after its child would run its
//! next statement in between. A killed parent's children keep its PID as their parent PID, so
//! later sweeps still find processes a dying parent started at the last moment.

use std::collections::HashSet;

use rh_shared::types::{Holder, LeaseBinding};

use crate::{
    ancestry::{ProcessNode, ancestry},
    binding::started_before,
    process::ProcessTable,
};

/// The coordinator's own executables, which a kill never touches.
pub const RH_BINARIES: [&str; 3] = ["rh.exe", "rhd.exe", "rh-mcp.exe"];

/// A process identified across PID reuse.
type ProcessKey = (u32, u64);

/// One process a kill terminates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillTarget {
    /// Process ID.
    pub pid: u32,
    /// Start time in seconds since the Unix epoch; a process with the same PID and another
    /// start time is not this one.
    pub start_time: u64,
    /// Executable name.
    pub name: String,
    /// Levels below the bound root, which is 0.
    pub depth: u32,
}

/// A process inside the bound tree that a kill leaves running, together with everything
/// below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spared {
    /// Process ID.
    pub pid: u32,
    /// Executable name.
    pub name: String,
    /// Why it was left running.
    pub reason: SpareReason,
}

/// Why a kill left a process running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpareReason {
    /// A lease holder, one of its ancestors, the daemon or its ancestors, or an `rh` binary.
    Protected,
    /// It started before the command was reported, so it is not part of the command.
    OlderThanReport,
}

/// Why a kill was refused before anything was terminated.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KillRefusal {
    /// The lease has no bound root, so nothing ties it to a process tree.
    #[error("the lease has no bound process tree")]
    NotBound,
    /// The bound root is not running any more (or its PID now belongs to another process).
    #[error("the bound process {pid} is no longer running")]
    RootGone {
        /// The bound root's PID.
        pid: u32,
    },
    /// The bound root is a protected process.
    #[error("the bound process {pid} ({name}) is protected")]
    RootProtected {
        /// The bound root's PID.
        pid: u32,
        /// Its executable name.
        name: String,
    },
    /// The bound root started before the command was reported.
    #[error("the bound process {pid} ({name}) started before the command was reported")]
    RootOlderThanReport {
        /// The bound root's PID.
        pid: u32,
        /// Its executable name.
        name: String,
    },
}

/// What a kill did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KillReport {
    /// Processes terminated, in the order they were terminated.
    pub terminated: Vec<KillTarget>,
    /// Processes that could not be terminated, with the last error for each.
    pub failed: Vec<KillFailure>,
    /// Processes in the tree left running on purpose.
    pub spared: Vec<Spared>,
    /// Sweeps that found something to terminate.
    pub sweeps: u32,
}

/// A process a kill could not terminate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillFailure {
    /// The process.
    pub target: KillTarget,
    /// What the OS said.
    pub error: String,
}

/// Processes no kill may terminate: the lease's holder and its ancestors (the terminal or
/// editor it runs in), every other running lease holder, the daemon and its ancestors, and
/// anything named like an `rh` binary.
///
/// Other holders' ancestors are not protected: a command that starts a nested Claude session
/// is that session's ancestor, and only the nested session itself must survive the kill.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Protected {
    processes: HashSet<ProcessKey>,
}

impl Protected {
    /// Protects `holder` and the daemon `own_pid` with their ancestors as `table` shows them,
    /// and every running process among `others`.
    pub fn new<'a, I>(table: &ProcessTable, holder: &Holder, others: I, own_pid: u32) -> Self
    where
        I: IntoIterator<Item = &'a Holder>,
    {
        let lineage = |pid| ancestry(pid, |pid| table.get(pid).cloned());
        let mut processes: HashSet<ProcessKey> = others
            .into_iter()
            .filter(|other| table.is_running(other.pid, other.pid_start_time))
            .map(|other| (other.pid, other.pid_start_time))
            .collect();
        if table.is_running(holder.pid, holder.pid_start_time) {
            processes.extend(lineage(holder.pid).iter().map(key));
        }
        processes.extend(lineage(own_pid).iter().map(key));
        Self { processes }
    }

    /// Whether `node` must never be terminated.
    pub fn covers(&self, node: &ProcessNode) -> bool {
        self.processes.contains(&key(node))
            || RH_BINARIES
                .iter()
                .any(|binary| node.name.eq_ignore_ascii_case(binary))
    }
}

/// The processes a kill of one lease terminates, grown on every sweep by the processes that
/// appeared below them since.
#[derive(Debug, Clone)]
pub struct KillPlan {
    bound_at_ms: i64,
    protected: Protected,
    /// Every process planned so far: parents before children within one growth step.
    planned: Vec<KillTarget>,
    spared: Vec<Spared>,
}

impl KillPlan {
    /// Plans the kill of the tree under `binding`'s root, as `table` shows it.
    ///
    /// Protected processes and processes older than the report are left running with
    /// everything below them.
    ///
    /// # Errors
    ///
    /// A [`KillRefusal`] when the binding has no root, the root is not running, is protected,
    /// or started before the report.
    pub fn new(
        binding: &LeaseBinding,
        table: &ProcessTable,
        protected: Protected,
    ) -> Result<Self, KillRefusal> {
        let root = binding.root.ok_or(KillRefusal::NotBound)?;
        let node = table
            .get(root.pid)
            .filter(|node| node.start_time == root.start_time)
            .ok_or(KillRefusal::RootGone { pid: root.pid })?;
        if protected.covers(node) {
            return Err(KillRefusal::RootProtected {
                pid: node.pid,
                name: node.name.clone(),
            });
        }
        if started_before(node, binding.bound_at_ms) {
            return Err(KillRefusal::RootOlderThanReport {
                pid: node.pid,
                name: node.name.clone(),
            });
        }
        let mut plan = Self {
            bound_at_ms: binding.bound_at_ms,
            protected,
            planned: vec![target(node, 0)],
            spared: Vec::new(),
        };
        plan.grow(table);
        Ok(plan)
    }

    /// Every process planned so far.
    pub fn planned(&self) -> &[KillTarget] {
        &self.planned
    }

    /// Processes in the tree left running on purpose.
    pub fn spared(&self) -> &[Spared] {
        &self.spared
    }

    /// The processes one sweep terminates, root first: every planned process still running
    /// in `table`, after adding the processes that appeared below planned ones.
    pub fn sweep(&mut self, table: &ProcessTable) -> Vec<KillTarget> {
        self.grow(table);
        let mut targets: Vec<KillTarget> = self
            .planned
            .iter()
            .filter(|target| table.is_running(target.pid, target.start_time))
            .cloned()
            .collect();
        targets.sort_by_key(|target| target.depth);
        targets
    }

    /// Adds every unplanned descendant of a planned process, skipping protected and old ones
    /// with everything below them.
    fn grow(&mut self, table: &ProcessTable) {
        let mut seen: HashSet<ProcessKey> = self
            .planned
            .iter()
            .map(|target| (target.pid, target.start_time))
            .collect();
        let mut next = 0;
        while next < self.planned.len() {
            let parent = &self.planned[next];
            let depth = parent.depth + 1;
            let mut children: Vec<&ProcessNode> = table
                .children_of(parent.pid, parent.start_time)
                .filter(|child| seen.insert((child.pid, child.start_time)))
                .collect();
            children.sort_by_key(|child| (child.start_time, child.pid));
            for child in children {
                let reason = if self.protected.covers(child) {
                    Some(SpareReason::Protected)
                } else if started_before(child, self.bound_at_ms) {
                    Some(SpareReason::OlderThanReport)
                } else {
                    None
                };
                match reason {
                    Some(reason) => self.spare(child, reason),
                    None => self.planned.push(target(child, depth)),
                }
            }
            next += 1;
        }
    }

    fn spare(&mut self, node: &ProcessNode, reason: SpareReason) {
        if !self.spared.iter().any(|spared| spared.pid == node.pid) {
            self.spared.push(Spared {
                pid: node.pid,
                name: node.name.clone(),
                reason,
            });
        }
    }
}

fn key(node: &ProcessNode) -> ProcessKey {
    (node.pid, node.start_time)
}

fn target(node: &ProcessNode, depth: u32) -> KillTarget {
    KillTarget {
        pid: node.pid,
        start_time: node.start_time,
        name: node.name.clone(),
        depth,
    }
}

#[cfg(test)]
mod tests {
    use rh_shared::types::{BoundProcess, SessionKey};

    use super::*;
    use crate::{sampler::RawProcess, test_support::raw_process};

    /// The command was reported at 100.5 s.
    const REPORT_MS: i64 = 100_500;
    /// The daemon's PID.
    const RHD: u32 = 5;

    fn holder(pid: u32, pid_start_time: u64) -> Holder {
        Holder {
            session: SessionKey(format!("s{pid}")),
            pid,
            pid_start_time,
        }
    }

    fn binding(pid: u32, start_time: u64) -> LeaseBinding {
        LeaseBinding {
            tool_use_id: "toolu_01".into(),
            bound_at_ms: REPORT_MS,
            root: Some(BoundProcess { pid, start_time }),
        }
    }

    /// Explorer runs a terminal running the session's claude.exe (PID 10) and rhd.
    fn base() -> Vec<RawProcess> {
        vec![
            raw_process(1, None, "explorer.exe", 1),
            raw_process(2, Some(1), "WindowsTerminal.exe", 20),
            raw_process(10, Some(2), "claude.exe", 50),
            raw_process(11, Some(10), "rh-mcp.exe", 51),
            raw_process(RHD, Some(11), "rhd.exe", 52),
        ]
    }

    fn with(extra: &[RawProcess]) -> ProcessTable {
        let mut processes = base();
        processes.extend_from_slice(extra);
        ProcessTable::new(&processes)
    }

    /// Session A (PID 10) holds the lease; `others` hold other leases.
    fn protected(table: &ProcessTable, others: &[Holder]) -> Protected {
        Protected::new(table, &holder(10, 50), others, RHD)
    }

    fn plan(table: &ProcessTable, binding: &LeaseBinding) -> Result<KillPlan, KillRefusal> {
        KillPlan::new(binding, table, protected(table, &[]))
    }

    fn pids(targets: &[KillTarget]) -> Vec<u32> {
        targets.iter().map(|target| target.pid).collect()
    }

    /// A Bash command: Git's bash launcher, the MSYS bash, its console host, and a build.
    fn bash_command() -> Vec<RawProcess> {
        vec![
            raw_process(20, Some(10), "bash.exe", 101),
            raw_process(21, Some(20), "conhost.exe", 101),
            raw_process(22, Some(20), "bash.exe", 101),
            raw_process(23, Some(22), "cargo.exe", 102),
            raw_process(24, Some(23), "rustc.exe", 103),
            raw_process(25, Some(23), "rustc.exe", 103),
        ]
    }

    #[test]
    fn test_plan_covers_nested_shells_root_first() {
        let table = with(&bash_command());
        let mut plan = plan(&table, &binding(20, 101)).unwrap();
        let targets = plan.sweep(&table);
        assert_eq!(pids(&targets), [20, 21, 22, 23, 24, 25]);
        let depths: Vec<u32> = targets.iter().map(|target| target.depth).collect();
        assert_eq!(depths, [0, 1, 1, 2, 3, 3]);
        assert!(plan.spared().is_empty());
    }

    #[test]
    fn test_plan_leaves_the_session_and_siblings_alone() {
        let mut extra = bash_command();
        extra.push(raw_process(30, Some(10), "cmd.exe", 104));
        extra.push(raw_process(31, Some(2), "pwsh.exe", 104));
        let table = with(&extra);
        let plan = plan(&table, &binding(22, 101)).unwrap();
        assert_eq!(pids(plan.planned()), [22, 23, 24, 25]);
    }

    #[test]
    fn test_plan_refuses_without_a_bound_root() {
        let table = with(&bash_command());
        let mut unbound = binding(20, 101);
        unbound.root = None;
        assert_eq!(plan(&table, &unbound).unwrap_err(), KillRefusal::NotBound);
    }

    #[test]
    fn test_plan_refuses_a_reused_root_pid() {
        let table = with(&bash_command());
        assert_eq!(
            plan(&table, &binding(20, 99)).unwrap_err(),
            KillRefusal::RootGone { pid: 20 }
        );
        assert_eq!(
            plan(&table, &binding(77, 101)).unwrap_err(),
            KillRefusal::RootGone { pid: 77 }
        );
    }

    #[test]
    fn test_plan_refuses_the_holder_its_ancestors_and_rh_binaries() {
        let table = with(&bash_command());
        for (pid, start_time, name) in [
            (10, 50, "claude.exe"),
            (2, 20, "WindowsTerminal.exe"),
            (1, 1, "explorer.exe"),
            (11, 51, "rh-mcp.exe"),
            (RHD, 52, "rhd.exe"),
        ] {
            let mut root = binding(pid, start_time);
            root.bound_at_ms = 0;
            assert_eq!(
                plan(&table, &root).unwrap_err(),
                KillRefusal::RootProtected {
                    pid,
                    name: name.into()
                },
                "{name}"
            );
        }
    }

    #[test]
    fn test_plan_refuses_another_sessions_holder_as_root() {
        let table = with(&[raw_process(60, Some(10), "claude.exe", 101)]);
        let protected = protected(&table, &[holder(60, 101)]);
        assert_eq!(
            KillPlan::new(&binding(60, 101), &table, protected).unwrap_err(),
            KillRefusal::RootProtected {
                pid: 60,
                name: "claude.exe".into()
            }
        );
    }

    #[test]
    fn test_plan_refuses_a_root_older_than_the_report() {
        let table = with(&[raw_process(20, Some(10), "node.exe", 90)]);
        assert_eq!(
            plan(&table, &binding(20, 90)).unwrap_err(),
            KillRefusal::RootOlderThanReport {
                pid: 20,
                name: "node.exe".into()
            }
        );
    }

    #[test]
    fn test_plan_skips_a_holder_inside_the_subtree() {
        // The command runs a nested `claude -p` session that holds a lease of its own.
        let mut extra = bash_command();
        extra.push(raw_process(40, Some(22), "claude.exe", 104));
        extra.push(raw_process(41, Some(40), "bash.exe", 105));
        extra.push(raw_process(42, Some(40), "rh.exe", 105));
        extra.push(raw_process(43, Some(22), "RH.EXE", 104));
        let table = with(&extra);
        let others = [holder(40, 104)];
        let plan = KillPlan::new(&binding(20, 101), &table, protected(&table, &others)).unwrap();
        assert_eq!(pids(plan.planned()), [20, 21, 22, 23, 24, 25]);
        let spared: Vec<(u32, SpareReason)> = plan
            .spared()
            .iter()
            .map(|spared| (spared.pid, spared.reason))
            .collect();
        assert_eq!(
            spared,
            [(40, SpareReason::Protected), (43, SpareReason::Protected)]
        );
    }

    #[test]
    fn test_plan_keeps_overlapping_leases_apart() {
        // Session A (PID 10) and session B (PID 60) each run a build.
        let mut extra = bash_command();
        extra.extend([
            raw_process(60, Some(2), "claude.exe", 70),
            raw_process(61, Some(60), "cmd.exe", 101),
            raw_process(62, Some(61), "pwsh.exe", 101),
            raw_process(63, Some(62), "cargo.exe", 102),
        ]);
        let table = with(&extra);
        let plan_a = KillPlan::new(
            &binding(20, 101),
            &table,
            protected(&table, &[holder(60, 70)]),
        )
        .unwrap();
        let protected_b = Protected::new(&table, &holder(60, 70), &[holder(10, 50)], RHD);
        let plan_b = KillPlan::new(&binding(61, 101), &table, protected_b).unwrap();
        assert_eq!(pids(plan_a.planned()), [20, 21, 22, 23, 24, 25]);
        assert_eq!(pids(plan_b.planned()), [61, 62, 63]);
    }

    #[test]
    fn test_plan_skips_children_whose_parent_pid_was_reused() {
        // PID 23 belonged to an older process; this child names it but predates cargo.
        let mut extra = bash_command();
        extra.push(raw_process(26, Some(23), "svchost.exe", 30));
        let table = with(&extra);
        let plan = plan(&table, &binding(20, 101)).unwrap();
        assert!(!pids(plan.planned()).contains(&26));
    }

    #[test]
    fn test_sweep_finds_children_of_killed_parents_and_drops_reused_pids() {
        let table = with(&bash_command());
        let mut plan = plan(&table, &binding(20, 101)).unwrap();
        plan.sweep(&table);

        // After the first sweep: the shells are gone, cargo started one more rustc before
        // dying, rustc 24's PID was reused by an unrelated process, and rustc 25 survived.
        let after = with(&[
            raw_process(24, Some(1), "notepad.exe", 110),
            raw_process(25, Some(23), "rustc.exe", 103),
            raw_process(26, Some(23), "rustc.exe", 109),
            raw_process(27, Some(26), "link.exe", 109),
        ]);
        let targets = plan.sweep(&after);
        assert_eq!(pids(&targets), [25, 26, 27]);
        assert_eq!(targets[1].depth, 3);
        assert!(plan.sweep(&with(&[])).is_empty());
    }
}
