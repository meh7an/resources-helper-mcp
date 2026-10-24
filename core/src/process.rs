//! The table of every running process in a sample, which liveness checks and command
//! binding read.

use std::collections::HashMap;

use rh_shared::types::Holder;

use crate::{ancestry::ProcessNode, ledger::ProcessLiveness, sampler::RawProcess};

/// Every process of one sample by PID, with its parent, name, exe path, and start time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessTable {
    nodes: HashMap<u32, ProcessNode>,
}

impl ProcessTable {
    /// The table of every process in a sample.
    pub fn new(processes: &[RawProcess]) -> Self {
        Self {
            nodes: processes
                .iter()
                .map(|process| {
                    let node = ProcessNode {
                        pid: process.pid,
                        parent_pid: process.parent_pid,
                        name: process.name.clone(),
                        exe: process.exe.clone(),
                        start_time: process.start_time,
                    };
                    (process.pid, node)
                })
                .collect(),
        }
    }

    /// The process with `pid`, if it was running.
    pub fn get(&self, pid: u32) -> Option<&ProcessNode> {
        self.nodes.get(&pid)
    }

    /// Whether `pid` was running and started at `start_time`, which rules out a reused PID.
    pub fn is_running(&self, pid: u32, start_time: u64) -> bool {
        self.get(pid)
            .is_some_and(|node| node.start_time == start_time)
    }

    /// Direct children of `pid`, in no particular order.
    ///
    /// A process that started before `pid` is not its child even if it names `pid` as its
    /// parent: Windows reuses PIDs and never updates a child's parent PID.
    pub fn children(&self, pid: u32) -> impl Iterator<Item = &ProcessNode> {
        let parent = self.get(pid);
        self.nodes.values().filter(move |node| {
            node.pid != pid
                && node.parent_pid == Some(pid)
                && parent.is_some_and(|parent| parent.start_time <= node.start_time)
        })
    }
}

impl ProcessLiveness for ProcessTable {
    fn is_alive(&self, holder: &Holder) -> bool {
        self.is_running(holder.pid, holder.pid_start_time)
    }
}

#[cfg(test)]
mod tests {
    use rh_shared::types::{ResourceUsage, SessionKey};

    use super::*;

    fn process(pid: u32, parent_pid: Option<u32>, start_time: u64) -> RawProcess {
        RawProcess {
            pid,
            parent_pid,
            name: format!("p{pid}.exe"),
            exe: Some(format!(r"C:\bin\p{pid}.exe")),
            start_time,
            usage: ResourceUsage::default(),
        }
    }

    fn child_pids(table: &ProcessTable, pid: u32) -> Vec<u32> {
        let mut pids: Vec<u32> = table.children(pid).map(|node| node.pid).collect();
        pids.sort_unstable();
        pids
    }

    #[test]
    fn test_process_table_keeps_parent_exe_and_start_time() {
        let table = ProcessTable::new(&[process(1, None, 10), process(2, Some(1), 20)]);
        let node = table.get(2).unwrap();
        assert_eq!(node.parent_pid, Some(1));
        assert_eq!(node.name, "p2.exe");
        assert_eq!(node.exe.as_deref(), Some(r"C:\bin\p2.exe"));
        assert_eq!(node.start_time, 20);
        assert!(table.get(3).is_none());
    }

    #[test]
    fn test_process_table_is_running_checks_start_time() {
        let table = ProcessTable::new(&[process(7, None, 70)]);
        assert!(table.is_running(7, 70));
        assert!(!table.is_running(7, 71));
        assert!(!table.is_running(8, 70));
    }

    #[test]
    fn test_process_table_liveness_matches_holder_start_time() {
        let table = ProcessTable::new(&[process(7, None, 70)]);
        let holder = |pid_start_time| Holder {
            session: SessionKey("a".into()),
            pid: 7,
            pid_start_time,
        };
        assert!(table.is_alive(&holder(70)));
        assert!(!table.is_alive(&holder(69)));
    }

    #[test]
    fn test_process_table_children_are_direct_only() {
        let table = ProcessTable::new(&[
            process(1, None, 10),
            process(2, Some(1), 20),
            process(3, Some(1), 30),
            process(4, Some(2), 40),
        ]);
        assert_eq!(child_pids(&table, 1), [2, 3]);
        assert_eq!(child_pids(&table, 2), [4]);
        assert!(child_pids(&table, 4).is_empty());
    }

    #[test]
    fn test_process_table_children_older_than_parent_are_skipped() {
        let table = ProcessTable::new(&[process(1, None, 50), process(2, Some(1), 30)]);
        assert!(child_pids(&table, 1).is_empty());
    }

    #[test]
    fn test_process_table_children_of_unknown_pid_are_empty() {
        let table = ProcessTable::new(&[process(2, Some(1), 30)]);
        assert!(child_pids(&table, 1).is_empty());
    }

    #[test]
    fn test_process_table_self_parent_is_not_a_child() {
        let table = ProcessTable::new(&[process(4, Some(4), 10)]);
        assert!(child_pids(&table, 4).is_empty());
    }
}
