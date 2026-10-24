//! `rh kill` and `rh remove`: the flyout's Kill and Remove, from a terminal.

use rh_client::BlockingClient;
use rh_shared::{
    api::{KillResponse, KilledProcess, RemoveResponse, UnkilledProcess},
    types::LeaseId,
};

/// Ends an active lease of any session and terminates its bound process tree. The session
/// that held it is told on its next tool call.
///
/// # Errors
///
/// Fails when rhd cannot be reached, the lease is not active, or the kill is refused.
pub fn kill(id: i64) -> anyhow::Result<()> {
    let client = BlockingClient::discover(true)?;
    let response = client.kill(LeaseId(id))?;
    print!("{}", format_kill(&response));
    Ok(())
}

/// Ends a queued lease of any session before it starts. The session that asked for it does
/// not run its command.
///
/// # Errors
///
/// Fails when rhd cannot be reached or the lease is not queued.
pub fn remove(id: i64) -> anyhow::Result<()> {
    let client = BlockingClient::discover(true)?;
    let response = client.remove(LeaseId(id))?;
    print!("{}", format_remove(&response));
    Ok(())
}

fn format_kill(response: &KillResponse) -> String {
    let lease = &response.lease;
    let mut text = format!("lease {} killed: {}\n", lease.id.0, lease.label);
    if let Some(reason) = &response.untouched_reason {
        text.push_str(&format!("no process was terminated: {reason}\n"));
    }
    if !response.terminated.is_empty() {
        let terminated: Vec<String> = response.terminated.iter().map(killed).collect();
        text.push_str(&format!("terminated     {}\n", terminated.join(", ")));
    }
    for process in &response.failed {
        text.push_str(&format!("still running  {}\n", unkilled(process)));
    }
    for process in &response.spared {
        text.push_str(&format!("left running   {}\n", unkilled(process)));
    }
    text
}

fn format_remove(response: &RemoveResponse) -> String {
    let lease = &response.lease;
    format!(
        "lease {} removed from the queue: {}\n",
        lease.id.0, lease.label
    )
}

fn killed(process: &KilledProcess) -> String {
    format!("{} ({})", process.name, process.pid)
}

fn unkilled(process: &UnkilledProcess) -> String {
    format!("{} ({}): {}", process.name, process.pid, process.reason)
}

#[cfg(test)]
mod tests {
    use rh_shared::types::{
        EndCause, Holder, JobProfile, Lease, LeaseKind, LeaseState, SessionKey,
    };

    use super::*;

    fn ended(cause: EndCause) -> Lease {
        Lease {
            id: LeaseId(39),
            holder: Holder {
                session: SessionKey("s".into()),
                pid: 60_012,
                pid_start_time: 1,
            },
            kind: LeaseKind::Normal,
            state: LeaseState::Expired,
            class: None,
            profile: JobProfile {
                cores: 8.0,
                ram_mb: 4_096,
                gpu_pct: 0.0,
                vram_mb: 0,
                est_secs: 300,
            },
            label: "cargo build".into(),
            cwd: String::new(),
            requested_at_ms: 1_000,
            granted_at_ms: None,
            ended_at_ms: Some(3_000),
            expires_at_ms: 60_000,
            end_cause: Some(cause),
            binding: None,
        }
    }

    #[test]
    fn test_format_kill_lists_terminated_failed_and_spared_processes() {
        let response = KillResponse {
            lease: ended(EndCause::Killed),
            terminated: vec![
                KilledProcess {
                    pid: 31_020,
                    name: "bash.exe".into(),
                },
                KilledProcess {
                    pid: 31_032,
                    name: "cargo.exe".into(),
                },
            ],
            failed: vec![UnkilledProcess {
                pid: 31_090,
                name: "link.exe".into(),
                reason: "Access is denied.".into(),
            }],
            spared: vec![UnkilledProcess {
                pid: 31_101,
                name: "claude.exe".into(),
                reason: "protected".into(),
            }],
            untouched_reason: None,
        };
        assert_eq!(
            format_kill(&response),
            "lease 39 killed: cargo build\n\
             terminated     bash.exe (31020), cargo.exe (31032)\n\
             still running  link.exe (31090): Access is denied.\n\
             left running   claude.exe (31101): protected\n"
        );
    }

    #[test]
    fn test_format_kill_without_a_tree_says_why_nothing_was_terminated() {
        let response = KillResponse {
            lease: ended(EndCause::Killed),
            terminated: Vec::new(),
            failed: Vec::new(),
            spared: Vec::new(),
            untouched_reason: Some("the lease has no bound process tree".into()),
        };
        assert_eq!(
            format_kill(&response),
            "lease 39 killed: cargo build\n\
             no process was terminated: the lease has no bound process tree\n"
        );
    }

    #[test]
    fn test_format_remove_names_the_lease() {
        let response = RemoveResponse {
            lease: ended(EndCause::Removed),
        };
        assert_eq!(
            format_remove(&response),
            "lease 39 removed from the queue: cargo build\n"
        );
    }
}
