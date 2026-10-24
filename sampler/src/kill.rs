//! Terminating a planned process tree with `TerminateProcess`, checking every process's
//! start time on the open handle so a reused PID is never hit.

use std::{collections::HashMap, thread, time::Duration};

use rh_core::{
    kill::{KillFailure, KillPlan, KillReport, KillTarget},
    process::ProcessTable,
    sampler::RawProcess,
};
use rh_shared::types::ResourceUsage;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use windows::Win32::{
    Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, HANDLE, WAIT_OBJECT_0},
    System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
    },
};

/// Sweeps at most; each takes a fresh process list and catches what the previous one missed.
const SWEEPS: u32 = 3;

/// Pause between sweeps, so processes started by dying parents show up.
const SWEEP_PAUSE: Duration = Duration::from_millis(100);

/// How long to wait for one terminated process to exit before moving on.
const EXIT_WAIT_MS: u32 = 500;

/// The exit code Task Manager and `Stop-Process` use. Claude Code reports a command killed
/// with it as "Exit code 255".
const KILLED_EXIT_CODE: u32 = u32::MAX;

/// FILETIME ticks per second.
const TICKS_PER_SEC: u64 = 10_000_000;

/// Seconds from the FILETIME epoch (1601) to the Unix epoch.
const FILETIME_TO_UNIX_SECS: u64 = 11_644_473_600;

/// What happened to one target.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Terminated,
    /// It exited, or its PID now belongs to another process.
    Gone,
    Failed(String),
}

/// Terminates the processes `plan` covers, root first, in up to three sweeps.
///
/// Every sweep reads the running processes again, so children a dying parent started at
/// the last moment are terminated too.
pub fn kill_tree(plan: &mut KillPlan) -> KillReport {
    let mut report = KillReport::default();
    let mut failures: HashMap<(u32, u64), KillFailure> = HashMap::new();
    for sweep in 0..SWEEPS {
        if sweep > 0 {
            thread::sleep(SWEEP_PAUSE);
        }
        let targets = plan.sweep(&process_table());
        if targets.is_empty() {
            break;
        }
        report.sweeps += 1;
        for target in targets {
            let key = (target.pid, target.start_time);
            match terminate(&target) {
                Outcome::Terminated => {
                    failures.remove(&key);
                    if !report.terminated.contains(&target) {
                        report.terminated.push(target);
                    }
                }
                Outcome::Gone => {
                    failures.remove(&key);
                }
                Outcome::Failed(error) => {
                    failures.insert(key, KillFailure { target, error });
                }
            }
        }
    }
    report.failed = failures.into_values().collect();
    report.failed.sort_by_key(|failure| failure.target.depth);
    report.spared = plan.spared().to_vec();
    report
}

/// Every running process with its parent, name, and start time; no usage or exe paths.
pub fn process_table() -> ProcessTable {
    let mut system = System::new();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let processes: Vec<RawProcess> = system
        .processes()
        .values()
        .map(|process| RawProcess {
            pid: process.pid().as_u32(),
            parent_pid: process.parent().map(Pid::as_u32),
            name: process.name().to_string_lossy().into_owned(),
            exe: None,
            start_time: process.start_time(),
            usage: ResourceUsage::default(),
        })
        .collect();
    ProcessTable::new(&processes)
}

fn terminate(target: &KillTarget) -> Outcome {
    let access = PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
    // SAFETY: plain FFI call; the returned handle is closed below.
    let handle = match unsafe { OpenProcess(access, false, target.pid) } {
        Ok(handle) => handle,
        Err(error) if error.code() == ERROR_INVALID_PARAMETER.to_hresult() => {
            return Outcome::Gone;
        }
        Err(error) => return Outcome::Failed(error.message()),
    };
    let outcome = terminate_open(handle, target);
    // SAFETY: `handle` came from `OpenProcess` and is closed exactly once.
    let _ = unsafe { CloseHandle(handle) };
    outcome
}

fn terminate_open(handle: HANDLE, target: &KillTarget) -> Outcome {
    match start_time(handle) {
        Ok(start_time) if start_time == target.start_time => {}
        Ok(_) => return Outcome::Gone,
        Err(error) => return Outcome::Failed(error.message()),
    }
    // SAFETY: `handle` is an open process handle with terminate access.
    if let Err(error) = unsafe { TerminateProcess(handle, KILLED_EXIT_CODE) } {
        // Terminating a process that is already exiting fails with access denied.
        return if has_exited(handle, 0) {
            Outcome::Gone
        } else {
            Outcome::Failed(error.message())
        };
    }
    has_exited(handle, EXIT_WAIT_MS);
    Outcome::Terminated
}

/// The process's start time in whole seconds since the Unix epoch, rounded down as sysinfo
/// does, so it compares equal to the process table's.
fn start_time(handle: HANDLE) -> windows::core::Result<u64> {
    let mut creation = FILETIME::default();
    let mut unused = [FILETIME::default(); 3];
    let [exit, kernel, user] = &mut unused;
    // SAFETY: `handle` has limited query access and every pointer is a valid FILETIME.
    unsafe { GetProcessTimes(handle, &mut creation, exit, kernel, user) }?;
    let ticks = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    Ok((ticks / TICKS_PER_SEC).saturating_sub(FILETIME_TO_UNIX_SECS))
}

fn has_exited(handle: HANDLE, wait_ms: u32) -> bool {
    // SAFETY: `handle` is an open process handle with synchronize access.
    let event = unsafe { WaitForSingleObject(handle, wait_ms) };
    event == WAIT_OBJECT_0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::busy_child;

    #[test]
    fn test_process_table_lists_own_process_with_its_start_time() {
        let table = process_table();
        let own = table.get(std::process::id()).unwrap();
        assert!(own.start_time > 0);
        assert!(own.parent_pid.is_some());
    }

    #[test]
    fn test_start_time_of_own_process_matches_the_process_table() {
        use windows::Win32::System::Threading::GetCurrentProcess;

        let own = process_table().get(std::process::id()).unwrap().start_time;
        // SAFETY: the pseudo handle needs no closing and has full access.
        let read = start_time(unsafe { GetCurrentProcess() }).unwrap();
        assert_eq!(read, own);
    }

    #[test]
    fn test_terminate_reports_a_missing_process_as_gone() {
        let target = KillTarget {
            pid: u32::MAX - 3,
            start_time: 1,
            name: "missing.exe".into(),
            depth: 0,
        };
        assert_eq!(terminate(&target), Outcome::Gone);
    }

    #[test]
    fn test_terminate_checks_the_start_time_before_terminating() {
        let mut child = busy_child(10_000);
        let node = process_table().get(child.id()).unwrap().clone();
        let target = KillTarget {
            pid: node.pid,
            start_time: node.start_time - 1,
            name: node.name,
            depth: 0,
        };
        assert_eq!(terminate(&target), Outcome::Gone);
        assert!(
            child.try_wait().unwrap().is_none(),
            "the child must still run"
        );

        let target = KillTarget {
            start_time: node.start_time,
            ..target
        };
        assert_eq!(terminate(&target), Outcome::Terminated);
        assert_eq!(child.wait().unwrap().code(), Some(KILLED_EXIT_CODE as i32));
    }
}
