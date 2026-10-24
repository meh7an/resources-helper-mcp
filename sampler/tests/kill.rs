//! Kills a real process tree that this test spawns, and nothing else.

use std::{
    env,
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rh_core::{
    kill::{KillPlan, Protected},
    process::ProcessTable,
};
use rh_sampler::{kill_tree, process_table};
use rh_shared::types::{BoundProcess, Holder, LeaseBinding, SessionKey};

/// Levels left below a helper process; the helper at level 1 starts nothing.
const DEPTH_VAR: &str = "RH_KILL_TEST_DEPTH";

/// How long a helper lives when nobody kills it, so a failed test leaves nothing behind.
const HELPER_LIFETIME: Duration = Duration::from_secs(60);

#[test]
#[ignore = "helper process for the kill tests; does nothing without RH_KILL_TEST_DEPTH"]
fn tree_helper() {
    let Ok(depth) = env::var(DEPTH_VAR) else {
        return;
    };
    let depth: u32 = depth.parse().unwrap();
    let _child = (depth > 1).then(|| spawn_level(depth - 1));
    thread::sleep(HELPER_LIFETIME);
}

/// Starts this test binary as a helper with `depth` levels including itself.
fn spawn_level(depth: u32) -> Child {
    Command::new(env::current_exe().unwrap())
        .args(["tree_helper", "--exact", "--ignored"])
        .env(DEPTH_VAR, depth.to_string())
        .stdout(Stdio::null())
        .spawn()
        .unwrap()
}

/// Waits until `root` has a single chain of `depth` levels and returns its PIDs, root first,
/// with the table that shows it.
fn wait_for_chain(root: u32, depth: usize) -> (Vec<u32>, ProcessTable) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let table = process_table();
        let mut chain = vec![root];
        while let Some(child) = table.children(*chain.last().unwrap()).next() {
            chain.push(child.pid);
        }
        if chain.len() == depth {
            return (chain, table);
        }
        assert!(
            Instant::now() < deadline,
            "the tree has {} of {depth} levels",
            chain.len()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn now_ms() -> i64 {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    i64::try_from(elapsed.as_millis()).unwrap()
}

#[test]
fn test_kill_tree_terminates_exactly_a_spawned_three_level_tree() {
    let reported_at_ms = now_ms();
    let mut root = spawn_level(3);
    let mut control = spawn_level(1);
    let (chain, table) = wait_for_chain(root.id(), 3);

    let own = table.get(std::process::id()).unwrap();
    let holder = Holder {
        session: SessionKey("kill-test".into()),
        pid: own.pid,
        pid_start_time: own.start_time,
    };
    let root_start = table.get(root.id()).unwrap().start_time;
    let control_start = table.get(control.id()).unwrap().start_time;
    let binding = LeaseBinding {
        tool_use_id: "toolu_kill_test".into(),
        bound_at_ms: reported_at_ms,
        root: Some(BoundProcess {
            pid: root.id(),
            start_time: root_start,
        }),
    };
    let protected = Protected::new(&table, &holder, &[], own.pid);
    let mut plan = KillPlan::new(&binding, &table, protected).unwrap();

    // Before terminating anything, the plan must be exactly the spawned helpers.
    let exe_name = Path::new(&env::current_exe().unwrap())
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let planned: Vec<u32> = plan.planned().iter().map(|target| target.pid).collect();
    assert_eq!(planned, chain);
    assert!(
        plan.planned()
            .iter()
            .all(|target| target.name.eq_ignore_ascii_case(&exe_name)),
        "{:?}",
        plan.planned()
    );

    let report = kill_tree(&mut plan);
    let terminated: Vec<(u32, u32)> = report
        .terminated
        .iter()
        .map(|target| (target.pid, target.depth))
        .collect();
    assert_eq!(terminated, [(chain[0], 0), (chain[1], 1), (chain[2], 2)]);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert!(report.spared.is_empty(), "{:?}", report.spared);

    let after = process_table();
    for target in &report.terminated {
        assert!(
            !after.is_running(target.pid, target.start_time),
            "{target:?}"
        );
    }
    assert!(after.is_running(control.id(), control_start));
    assert!(after.is_running(holder.pid, holder.pid_start_time));
    assert_eq!(root.wait().unwrap().code(), Some(u32::MAX as i32));

    control.kill().unwrap();
    control.wait().unwrap();
}
