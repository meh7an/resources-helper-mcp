//! End-to-end tests: the real router and ledger over a temp database, with a fake sampler
//! whose machine state each test controls.

use std::{
    env, fs,
    future::Future,
    path::PathBuf,
    process::{self, Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use diesel::{Connection, sqlite::SqliteConnection};
use reqwest::{RequestBuilder, Response, StatusCode};
use rh_core::sampler::{RawProcess, RawSample, Sampler};
use rh_shared::{
    api::{
        AcquireRequest, AcquireResponse, BenchmarkProgress, BindRequest, CanFitRequest,
        DeliveredResponse, HealthResponse, HeartbeatResponse, HistoryResponse, HistoryRunsResponse,
        KillResponse, LeasesResponse, NoticesResponse, ProfileSpec, RegisterSessionRequest,
        ReleaseResponse, RemoveResponse, SessionsResponse, Verdict, WaitRequest,
    },
    config::Config,
    types::{
        EndCause, Holder, JobClass, JobProfile, Lease, LeaseId, LeaseKind, LeaseState, MemStats,
        ResourceUsage, SessionKey, Snapshot,
    },
    ui::{Notice, UiState},
};
use rhd::{
    DaemonOptions, Maintenance,
    db::{
        open_pool,
        repo::{self, LeaseDraft},
    },
};
use serde_json::Value;
use tempfile::TempDir;
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

const TOKEN: &str = "test-token";
const ALICE_PID: u32 = 4_242;
const BOB_PID: u32 = 5_353;

struct FakeSampler(Arc<Mutex<RawSample>>);

impl Sampler for FakeSampler {
    /// The current machine state, stamped now: the quiet window and the benchmark meter
    /// count each sample once, by its timestamp.
    fn sample(&mut self) -> rh_shared::Result<RawSample> {
        let mut sample = self.0.lock().unwrap().clone();
        sample.taken_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        Ok(sample)
    }
}

fn process(pid: u32, start_time: u64) -> RawProcess {
    RawProcess {
        pid,
        parent_pid: None,
        name: "claude.exe".into(),
        exe: None,
        start_time,
        usage: ResourceUsage::default(),
    }
}

/// A 16-core, 32 GB machine at rest with two Claude sessions running.
fn idle_machine() -> RawSample {
    RawSample {
        taken_at_ms: 0,
        cpu_total_pct: 0.0,
        per_core_pct: vec![0.0; 16],
        memory: MemStats {
            total_mb: 32_768,
            used_mb: 4_000,
            available_mb: 28_768,
            commit_used_mb: 6_000,
            commit_limit_mb: 65_536,
        },
        processes: vec![process(ALICE_PID, 100), process(BOB_PID, 200)],
        gpus: vec![],
        drives: vec![],
    }
}

fn holder(session: &str, pid: u32, pid_start_time: u64) -> Holder {
    Holder {
        session: SessionKey(session.into()),
        pid,
        pid_start_time,
    }
}

fn alice() -> Holder {
    holder("alice", ALICE_PID, 100)
}

fn bob() -> Holder {
    holder("bob", BOB_PID, 200)
}

async fn send(request: RequestBuilder) -> Response {
    request.send().await.unwrap()
}

/// Polls `check` every 10 ms for up to 3 s.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

struct Daemon {
    base: String,
    client: reqwest::Client,
    machine: Arc<Mutex<RawSample>>,
    stop: Option<oneshot::Sender<()>>,
    server: Option<JoinHandle<anyhow::Result<()>>>,
    _dir: TempDir,
}

impl Daemon {
    async fn start() -> Self {
        Self::start_with(Config::default(), |_| {}).await
    }

    /// Starts a daemon on `config` after `prepare` has run against its fresh database.
    async fn start_with(config: Config, prepare: impl FnOnce(&mut SqliteConnection)) -> Self {
        let dir = TempDir::new().unwrap();
        prepare(&mut open_pool(&database(&dir)).unwrap().get().unwrap());
        Self::serve_in(dir, config, Arc::new(Mutex::new(idle_machine()))).await
    }

    /// Stops the daemon gracefully and starts a new one on the same database, config file,
    /// and machine.
    async fn restart(mut self) -> Self {
        self.stop_server().await;
        let Self {
            machine, _dir: dir, ..
        } = self;
        Self::serve_in(dir, Config::default(), machine).await
    }

    async fn serve_in(dir: TempDir, config: Config, machine: Arc<Mutex<RawSample>>) -> Self {
        let pool = open_pool(&database(&dir)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let sampled = machine.clone();
        let options = DaemonOptions {
            config,
            token: TOKEN.into(),
            pool,
            sampler: Box::new(move || Ok(Box::new(FakeSampler(sampled)) as Box<dyn Sampler>)),
            sample_interval: Duration::from_millis(20),
            maintenance: Maintenance {
                config_path: Some(dir.path().join("config.toml")),
                config_poll: Duration::from_millis(20),
                prune_every: Duration::from_secs(3_600),
            },
        };
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(rhd::serve(listener, options, async {
            let _ = stopped.await;
        }));
        let daemon = Self {
            base,
            client: reqwest::Client::new(),
            machine,
            stop: Some(stop),
            server: Some(server),
            _dir: dir,
        };
        eventually("the first snapshot", || async {
            daemon.get("/snapshot").await.status() == StatusCode::OK
        })
        .await;
        daemon
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn get(&self, path: &str) -> Response {
        let request = self.client.get(self.url(path)).bearer_auth(TOKEN);
        request.send().await.unwrap()
    }

    fn post(&self, path: &str) -> RequestBuilder {
        self.client.post(self.url(path)).bearer_auth(TOKEN)
    }

    async fn delete(&self, path: &str) -> Response {
        let request = self.client.delete(self.url(path)).bearer_auth(TOKEN);
        request.send().await.unwrap()
    }

    async fn acquire(&self, holder: Holder, profile: ProfileSpec, wait_secs: u64) -> Response {
        send(self.acquire_request(holder, profile, wait_secs)).await
    }

    fn acquire_request(
        &self,
        holder: Holder,
        profile: ProfileSpec,
        wait_secs: u64,
    ) -> RequestBuilder {
        let request = AcquireRequest {
            holder,
            profile,
            kind: LeaseKind::Normal,
            label: "cargo build".into(),
            cwd: None,
            wait_timeout_secs: wait_secs,
        };
        self.post("/leases").json(&request)
    }

    async fn acquire_build(&self, holder: Holder, wait_secs: u64) -> AcquireResponse {
        let profile = ProfileSpec::Class(JobClass::CpuHeavy);
        let response = self.acquire(holder, profile, wait_secs).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    /// A one-core benchmark, small enough to fit beside any background load.
    async fn acquire_benchmark(&self, holder: Holder, wait_secs: u64) -> AcquireResponse {
        let request = AcquireRequest {
            holder,
            profile: ProfileSpec::Custom(JobProfile {
                cores: 1.0,
                ram_mb: 100,
                gpu_pct: 0.0,
                vram_mb: 0,
                est_secs: 30,
            }),
            kind: LeaseKind::BenchmarkExclusive,
            label: "cargo bench".into(),
            cwd: None,
            wait_timeout_secs: wait_secs,
        };
        let response = send(self.post("/leases").json(&request)).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn wait(&self, id: LeaseId, session: &str, wait_secs: u64) -> Response {
        let path = format!("/leases/{}/wait?session={session}", id.0);
        let request = WaitRequest {
            wait_timeout_secs: wait_secs,
        };
        send(self.post(&path).json(&request)).await
    }

    async fn release_response(&self, id: LeaseId, session: &str) -> ReleaseResponse {
        let response = self.release(id, session).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn heartbeat(&self, id: LeaseId, session: &str) -> Response {
        let path = format!("/leases/{}/heartbeat?session={session}", id.0);
        send(self.post(&path)).await
    }

    async fn heartbeat_state(&self, id: LeaseId, session: &str) -> LeaseState {
        let response = self.heartbeat(id, session).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<HeartbeatResponse>().await.unwrap().state
    }

    async fn release(&self, id: LeaseId, session: &str) -> Response {
        let path = format!("/leases/{}?session={session}", id.0);
        self.delete(&path).await
    }

    async fn leases(&self, query: &str) -> Vec<Lease> {
        let response = self.get(&format!("/leases{query}")).await;
        response.json::<LeasesResponse>().await.unwrap().leases
    }

    /// A connection to the daemon's database, for checks the API does not expose.
    fn db(&self) -> SqliteConnection {
        SqliteConnection::establish(&database(&self._dir).to_string_lossy()).unwrap()
    }

    fn write_config(&self, text: &str) {
        fs::write(self._dir.path().join("config.toml"), text).unwrap();
    }

    async fn health(&self) -> HealthResponse {
        send(self.client.get(self.url("/health")))
            .await
            .json()
            .await
            .unwrap()
    }

    async fn stop(mut self) {
        self.stop_server().await;
    }

    async fn stop_server(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.send(()).unwrap();
        }
        if let Some(server) = self.server.take() {
            server.await.unwrap().unwrap();
        }
    }
}

fn database(dir: &TempDir) -> PathBuf {
    dir.path().join("rh.db")
}

#[tokio::test]
async fn test_health_is_open_and_every_other_route_needs_the_token() {
    let daemon = Daemon::start().await;
    let health = daemon
        .client
        .get(daemon.url("/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    let health: HealthResponse = health.json().await.unwrap();
    assert_eq!(health.pid, process::id());

    let anonymous = daemon
        .client
        .get(daemon.url("/snapshot"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let wrong = daemon.client.get(daemon.url("/leases")).bearer_auth("nope");
    assert_eq!(
        wrong.send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    daemon.stop().await;
}

#[tokio::test]
async fn test_snapshot_lists_registered_live_sessions_only() {
    let daemon = Daemon::start().await;
    let snapshot: Snapshot = daemon.get("/snapshot").await.json().await.unwrap();
    assert_eq!(snapshot.cpu.logical_cores, 16);

    for holder in [alice(), holder("ghost", 9_999, 1)] {
        let request = RegisterSessionRequest { holder };
        let response = send(daemon.post("/sessions").json(&request)).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    eventually("alice in /sessions", || async {
        let sessions: SessionsResponse = daemon.get("/sessions").await.json().await.unwrap();
        let keys: Vec<_> = sessions
            .sessions
            .iter()
            .map(|s| s.holder.session.0.clone())
            .collect();
        keys == ["alice"]
    })
    .await;
    daemon.stop().await;
}

#[tokio::test]
async fn test_acquire_on_idle_machine_is_granted_at_once() {
    let daemon = Daemon::start().await;
    let acquired = daemon.acquire_build(alice(), 0).await;
    assert_eq!(acquired.lease.state, LeaseState::Active);
    assert!(acquired.verdict.fits, "{:?}", acquired.verdict);
    assert_eq!(acquired.lease.class, Some(JobClass::CpuHeavy));

    let active = daemon.leases("?state=active").await;
    assert_eq!(
        active.iter().map(|lease| lease.id).collect::<Vec<_>>(),
        [acquired.lease.id]
    );
    daemon.stop().await;
}

#[tokio::test]
async fn test_second_build_queues_and_starts_after_release() {
    let daemon = Daemon::start().await;
    let first = daemon.acquire_build(alice(), 0).await.lease;
    let second = daemon.acquire_build(bob(), 0).await;
    assert_eq!(second.lease.state, LeaseState::Queued);
    assert!(!second.verdict.fits);
    assert!(
        second
            .verdict
            .reasons
            .iter()
            .any(|reason| reason.starts_with("cores:")),
        "{:?}",
        second.verdict
    );

    let released: ReleaseResponse = daemon
        .release(first.id, "alice")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(released.lease.state, LeaseState::Released);
    assert_eq!(
        daemon.heartbeat_state(second.lease.id, "bob").await,
        LeaseState::Active
    );
    daemon.stop().await;
}

#[tokio::test]
async fn test_acquire_wait_returns_as_soon_as_the_lease_starts() {
    let daemon = Arc::new(Daemon::start().await);
    let first = daemon.acquire_build(alice(), 0).await.lease;

    let waiter = daemon.clone();
    let started = Instant::now();
    let second = tokio::spawn(async move { waiter.acquire_build(bob(), 30).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!second.is_finished());
    daemon.release(first.id, "alice").await;

    let second = second.await.unwrap();
    assert_eq!(second.lease.state, LeaseState::Active);
    assert!(started.elapsed() < Duration::from_secs(5));
    Arc::into_inner(daemon).unwrap().stop().await;
}

#[tokio::test]
async fn test_acquire_wait_longer_than_the_ttl_keeps_the_queued_lease_alive() {
    let mut config = Config::default();
    config.leases.heartbeat_ttl_secs = 1;
    let daemon = Arc::new(Daemon::start_with(config, |_| {}).await);
    let first = daemon.acquire_build(alice(), 0).await.lease;

    let keeper = daemon.clone();
    let heartbeats = tokio::spawn(async move {
        loop {
            keeper.heartbeat(first.id, "alice").await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    let second = daemon.acquire_build(bob(), 3).await;
    heartbeats.abort();
    let _ = heartbeats.await;

    assert_eq!(
        second.lease.state,
        LeaseState::Queued,
        "{:?}",
        second.verdict
    );
    assert_eq!(
        daemon.heartbeat_state(second.lease.id, "bob").await,
        LeaseState::Queued
    );
    Arc::into_inner(daemon).unwrap().stop().await;
}

#[tokio::test]
async fn test_lease_of_a_dead_holder_expires() {
    let daemon = Daemon::start().await;
    let lease = daemon.acquire_build(bob(), 0).await.lease;
    assert_eq!(lease.state, LeaseState::Active);

    daemon
        .machine
        .lock()
        .unwrap()
        .processes
        .retain(|process| process.pid != BOB_PID);
    eventually("the lease to expire", || async {
        daemon.heartbeat_state(lease.id, "bob").await == LeaseState::Expired
    })
    .await;
    let expired = daemon.leases("?state=expired").await;
    assert_eq!(expired[0].id, lease.id);
    daemon.stop().await;
}

#[tokio::test]
async fn test_heartbeat_and_release_check_the_owner() {
    let daemon = Daemon::start().await;
    let lease = daemon.acquire_build(alice(), 0).await.lease;

    assert_eq!(
        daemon.heartbeat(lease.id, "bob").await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        daemon.release(lease.id, "bob").await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        daemon.heartbeat(LeaseId(999), "alice").await.status(),
        StatusCode::NOT_FOUND
    );

    assert_eq!(
        daemon.release(lease.id, "alice").await.status(),
        StatusCode::OK
    );
    let again: ReleaseResponse = daemon
        .release(lease.id, "alice")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(again.lease.state, LeaseState::Released);
    daemon.stop().await;
}

#[tokio::test]
async fn test_invalid_profile_is_unprocessable() {
    let daemon = Daemon::start().await;
    let profile = ProfileSpec::Custom(JobProfile {
        cores: 1.0,
        ram_mb: 100,
        gpu_pct: 150.0,
        vram_mb: 0,
        est_secs: 10,
    });
    let response = daemon.acquire(alice(), profile, 0).await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(response.text().await.unwrap().contains("gpu_pct"));
    daemon.stop().await;
}

#[tokio::test]
async fn test_can_fit_reports_the_shortfall() {
    let daemon = Daemon::start().await;
    let request = CanFitRequest {
        profile: ProfileSpec::Class(JobClass::CpuHeavy),
        kind: LeaseKind::Normal,
        label: None,
        cwd: None,
    };
    let can_fit = || send(daemon.post("/can-fit").json(&request));
    let verdict: Verdict = can_fit().await.json().await.unwrap();
    assert!(verdict.fits);

    daemon.acquire_build(alice(), 0).await;
    let verdict: Verdict = can_fit().await.json().await.unwrap();
    assert!(!verdict.fits);
    assert!(verdict.wait_estimate_secs.is_some());
    daemon.stop().await;
}

/// Stores an active one-core lease of `holder` that a previous run granted at `at_ms`.
fn stored_active_lease(conn: &mut SqliteConnection, holder: Holder, at_ms: i64) -> LeaseId {
    let draft = LeaseDraft {
        holder,
        kind: LeaseKind::Normal,
        class: None,
        profile: JobProfile {
            cores: 1.0,
            ram_mb: 100,
            gpu_pct: 0.0,
            vram_mb: 0,
            est_secs: 10,
        },
        label: "old build".into(),
        cwd: String::new(),
        requested_at_ms: at_ms,
        expires_at_ms: at_ms + 1,
    };
    let lease = repo::insert_lease(conn, &draft).unwrap();
    repo::transition_lease(conn, lease.id, LeaseState::Active, at_ms, "granted").unwrap();
    lease.id
}

#[tokio::test]
async fn test_leases_left_from_a_previous_run_are_restored_and_dead_holders_expire() {
    let mut ids = None;
    let daemon = Daemon::start_with(Config::default(), |conn| {
        let live = stored_active_lease(conn, alice(), 1);
        let dead = stored_active_lease(conn, holder("ghost", 9_999, 1), 1);
        ids = Some((live, dead));
    })
    .await;
    let (live, dead) = ids.unwrap();

    // The stored deadline passed long ago; the restore gave the live holder a fresh one.
    assert_eq!(
        daemon.heartbeat_state(live, "alice").await,
        LeaseState::Active
    );
    let leases = daemon.leases("").await;
    assert_eq!(leases[1].id, dead);
    assert_eq!(leases[1].state, LeaseState::Expired);
    let mut conn = daemon.db();
    let reasons: Vec<String> = [live, dead]
        .map(|id| {
            repo::lease_events(&mut conn, id)
                .unwrap()
                .pop()
                .unwrap()
                .reason
        })
        .into();
    assert_eq!(
        reasons,
        ["restored after a daemon restart", "holder process exited"]
    );
    daemon.stop().await;
}

#[tokio::test]
async fn test_restart_keeps_active_and_queued_leases_in_their_order() {
    let daemon = Daemon::start().await;
    let first = daemon.acquire_build(alice(), 0).await.lease;
    let second = daemon.acquire_build(bob(), 0).await.lease;
    assert_eq!(second.state, LeaseState::Queued);

    let daemon = daemon.restart().await;
    let states: Vec<(LeaseId, LeaseState)> = daemon
        .leases("")
        .await
        .iter()
        .map(|lease| (lease.id, lease.state))
        .collect();
    assert_eq!(
        states,
        [
            (first.id, LeaseState::Active),
            (second.id, LeaseState::Queued)
        ]
    );
    // The queued lease still waits for the restored active one, then starts when it ends.
    let waiting = daemon.wait(second.id, "bob", 0).await;
    let waiting: AcquireResponse = waiting.json().await.unwrap();
    assert_eq!(waiting.lease.state, LeaseState::Queued);
    let released = daemon.release_response(first.id, "alice").await;
    assert_eq!(released.lease.state, LeaseState::Released);
    assert_eq!(
        daemon.heartbeat_state(second.id, "bob").await,
        LeaseState::Active
    );
    // The run spanned the restart, so it left no history.
    let history: HistoryResponse = daemon.get("/history").await.json().await.unwrap();
    assert!(history.jobs.is_empty(), "{history:?}");
    daemon.stop().await;
}

#[tokio::test]
async fn test_startup_prunes_leases_past_their_retention() {
    let day_ms = 86_400_000;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let mut ids = None;
    let daemon = Daemon::start_with(Config::default(), |conn| {
        let old = stored_active_lease(conn, alice(), now - 40 * day_ms);
        repo::transition_lease(conn, old, LeaseState::Released, now - 40 * day_ms, "done").unwrap();
        let recent = stored_active_lease(conn, alice(), now - day_ms);
        repo::transition_lease(conn, recent, LeaseState::Released, now - day_ms, "done").unwrap();
        ids = Some((old, recent));
    })
    .await;
    let (old, recent) = ids.unwrap();

    eventually("the old lease to be pruned", || async {
        daemon.leases("").await.iter().all(|lease| lease.id != old)
    })
    .await;
    assert_eq!(daemon.leases("").await[0].id, recent);
    daemon.stop().await;
}

#[tokio::test]
async fn test_config_change_takes_effect_and_a_rejected_one_is_reported() {
    let daemon = Daemon::start().await;
    daemon.write_config("[classes.cpu_heavy]\ncores = 2\n");
    eventually("the new class profile", || async {
        let acquired = daemon.acquire_build(alice(), 0).await.lease;
        daemon.release(acquired.id, "alice").await;
        acquired.profile.cores == 2.0
    })
    .await;

    daemon.write_config("[classes.cpu_heavy]\ncores = 2\n[leases]\nheartbeat_ttl_secs = 0\n");
    eventually("the rejection in /health", || async {
        daemon
            .health()
            .await
            .config_error
            .is_some_and(|error| error.contains("leases.heartbeat_ttl_secs"))
    })
    .await;
    let kept = daemon.acquire_build(alice(), 0).await.lease;
    assert_eq!(kept.profile.cores, 2.0);
    daemon.release(kept.id, "alice").await;

    daemon.write_config("[classes.cpu_heavy]\ncores = 3\n");
    eventually("the fixed config", || async {
        daemon.health().await.config_error.is_none()
    })
    .await;
    assert_eq!(
        daemon.acquire_build(alice(), 0).await.lease.profile.cores,
        3.0
    );
    daemon.stop().await;
}

#[tokio::test]
async fn test_wait_resumes_a_queued_lease_and_checks_the_owner() {
    let daemon = Arc::new(Daemon::start().await);
    let first = daemon.acquire_build(alice(), 0).await.lease;
    let second = daemon.acquire_build(bob(), 0).await.lease;
    assert_eq!(second.state, LeaseState::Queued);

    assert_eq!(
        daemon.wait(second.id, "alice", 0).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        daemon.wait(LeaseId(999), "bob", 0).await.status(),
        StatusCode::NOT_FOUND
    );

    let waiter = daemon.clone();
    let resumed = tokio::spawn(async move {
        let response = waiter.wait(second.id, "bob", 30).await;
        response.json::<AcquireResponse>().await.unwrap()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!resumed.is_finished());
    daemon.release(first.id, "alice").await;

    let resumed = resumed.await.unwrap();
    assert_eq!(resumed.lease.id, second.id);
    assert_eq!(resumed.lease.state, LeaseState::Active);
    Arc::into_inner(daemon).unwrap().stop().await;
}

fn quick_benchmark_config(max_quiet_wait_secs: u64) -> Config {
    let mut config = Config::default();
    config.benchmark.quiet_secs = 1;
    config.benchmark.max_quiet_wait_secs = max_quiet_wait_secs;
    config
}

#[tokio::test]
async fn test_benchmark_on_a_quiet_machine_reports_and_stores_its_noise() {
    let daemon = Daemon::start_with(quick_benchmark_config(5), |_| {}).await;
    let lease = daemon.acquire_benchmark(alice(), 5).await.lease;
    assert_eq!(lease.state, LeaseState::Active);
    // About ten samples while the benchmark runs.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let released = daemon.release_response(lease.id, "alice").await;
    let noise = released.noise.expect("a noise report");
    assert!(noise.quiet_at_start);
    assert_eq!((noise.avg_cpu_pct, noise.peak_cpu_pct), (0.0, 0.0));

    let pool = open_pool(&daemon._dir.path().join("rh.db")).unwrap();
    let mut conn = pool.get().unwrap();
    let runs = repo::recent_job_runs(&mut conn, "cargo bench|", 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].lease_id, Some(lease.id));
    assert_eq!(runs[0].noise, Some(noise));
    let events = repo::lease_events(&mut conn, lease.id).unwrap();
    assert!(
        events[1].reason.starts_with("granted after 1s of quiet"),
        "{events:?}"
    );

    let again = daemon.release_response(lease.id, "alice").await;
    assert_eq!(again.noise, None);
    daemon.stop().await;
}

#[tokio::test]
async fn test_benchmark_on_a_busy_machine_is_granted_without_quiet() {
    let daemon = Arc::new(Daemon::start_with(quick_benchmark_config(2), |_| {}).await);
    daemon.machine.lock().unwrap().cpu_total_pct = 50.0;

    let queued = daemon.acquire_benchmark(alice(), 0).await;
    assert_eq!(queued.lease.state, LeaseState::Queued);
    assert!(
        matches!(
            queued.verdict.benchmark,
            Some(BenchmarkProgress::Measuring { .. } | BenchmarkProgress::Noisy { .. })
        ),
        "{:?}",
        queued.verdict
    );

    let response = daemon.wait(queued.lease.id, "alice", 10).await;
    let resumed: AcquireResponse = response.json().await.unwrap();
    assert_eq!(resumed.lease.state, LeaseState::Active);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let released = daemon.release_response(queued.lease.id, "alice").await;
    assert!(!released.noise.expect("a noise report").quiet_at_start);
    let pool = open_pool(&daemon._dir.path().join("rh.db")).unwrap();
    let events = repo::lease_events(&mut pool.get().unwrap(), queued.lease.id).unwrap();
    assert!(
        events[1]
            .reason
            .starts_with("granted without quiet after 2s"),
        "{events:?}"
    );
    Arc::into_inner(daemon).unwrap().stop().await;
}

#[tokio::test]
async fn test_released_run_raises_the_next_estimate_and_shows_in_history() {
    let daemon = Daemon::start().await;
    let set_alice_usage = |cores, ram_mb| {
        let mut machine = daemon.machine.lock().unwrap();
        machine.processes[0].usage = ResourceUsage {
            cores,
            ram_mb,
            ..ResourceUsage::default()
        };
    };
    // Set before the grant, so every sample of the run sees the load.
    set_alice_usage(12.0, 6_000);
    let first = daemon.acquire_build(alice(), 0).await;
    assert_eq!(first.lease.profile.cores, 8.0);
    assert!(first.verdict.reasons.is_empty(), "{:?}", first.verdict);
    // About ten samples while the build runs.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let released = daemon.release_response(first.lease.id, "alice").await;
    // Only after the release, or a sample of the run may see the reset.
    set_alice_usage(0.0, 0);
    assert_eq!(released.noise, None);

    let history: HistoryResponse = daemon.get("/history").await.json().await.unwrap();
    assert_eq!(history.jobs.len(), 1);
    let job = &history.jobs[0];
    assert_eq!((job.fingerprint.as_str(), job.runs), ("cargo build|", 1));
    assert_eq!(job.learned.map(|profile| profile.ram_mb), Some(6_000));

    let path = "/history/runs?fingerprint=cargo%20build%7C&limit=5";
    let runs: HistoryRunsResponse = daemon.get(path).await.json().await.unwrap();
    assert_eq!(runs.runs.len(), 1);
    assert_eq!(runs.runs[0].lease_id, Some(first.lease.id));
    assert!(!runs.runs[0].overlapped);

    // A differently written command line of the same job finds the run.
    let request = CanFitRequest {
        profile: ProfileSpec::Class(JobClass::CpuHeavy),
        kind: LeaseKind::Normal,
        label: Some("cd app && cargo build 2>&1".into()),
        cwd: None,
    };
    let verdict: Verdict = send(daemon.post("/can-fit").json(&request))
        .await
        .json()
        .await
        .unwrap();
    assert!(verdict.fits, "{verdict:?}");
    assert_eq!(
        verdict.reasons,
        [
            "cores 12.0 from p90 of 1 past run(s) (declared 8.0)",
            "RAM 6000 MB from p90 of 1 past run(s) (declared 4096 MB)",
        ]
    );

    let second = daemon.acquire_build(bob(), 0).await;
    assert_eq!(second.lease.profile.cores, 12.0);
    assert_eq!(second.lease.profile.ram_mb, 6_000);
    assert_eq!(second.verdict.reasons.len(), 2, "{:?}", second.verdict);
    daemon.stop().await;
}

#[tokio::test]
async fn test_short_burst_in_a_long_lease_raises_the_next_estimate_to_the_burst() {
    let daemon = Daemon::start().await;
    let set_alice_cores = |cores| {
        daemon.machine.lock().unwrap().processes[0].usage.cores = cores;
    };
    // Like lease 35: idle around a short build, so the whole-lease average stays low.
    set_alice_cores(0.03);
    let first = daemon.acquire_build(alice(), 0).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    set_alice_cores(12.0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    set_alice_cores(0.03);
    tokio::time::sleep(Duration::from_millis(200)).await;
    daemon.release_response(first.lease.id, "alice").await;

    let path = "/history/runs?fingerprint=cargo%20build%7C&limit=5";
    let runs: HistoryRunsResponse = daemon.get(path).await.json().await.unwrap();
    let run = &runs.runs[0];
    assert!(run.avg.cores < 8.0, "{run:?}");
    assert_eq!(run.busy.map(|busy| busy.cores), Some(12.0), "{run:?}");

    let second = daemon.acquire_build(bob(), 0).await;
    assert_eq!(second.lease.profile.cores, 12.0);
    assert_eq!(
        second.verdict.reasons,
        ["cores 12.0 from p90 of 1 past run(s) (declared 8.0)"]
    );
    daemon.stop().await;
}

/// Holds an active lease for alice and starts bob's acquire, which waits up to 60 s behind it.
async fn open_acquire_wait(daemon: &Daemon) -> JoinHandle<Response> {
    daemon.acquire_build(alice(), 0).await;
    let profile = ProfileSpec::Class(JobClass::CpuHeavy);
    let waiting = tokio::spawn(send(daemon.acquire_request(bob(), profile, 60)));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!waiting.is_finished());
    waiting
}

/// The answer to an acquire wait cut short by a shutdown: still queued, with a resume hint.
async fn assert_answered_for_restart(waiting: JoinHandle<Response>) {
    let response = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let answer: AcquireResponse = response.json().await.unwrap();
    assert_eq!(answer.lease.state, LeaseState::Queued);
    let reasons = answer.verdict.reasons.join(" | ");
    assert!(reasons.contains("rhd is restarting"), "{reasons}");
}

#[tokio::test]
async fn test_shutdown_route_answers_open_acquire_waits_at_once() {
    let mut daemon = Daemon::start().await;
    let waiting = open_acquire_wait(&daemon).await;
    let started = Instant::now();

    let response = send(daemon.post("/shutdown")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let server = daemon.server.take().unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(5), server).await;
    stopped.unwrap().unwrap().unwrap();

    assert!(started.elapsed() < Duration::from_secs(5));
    assert_answered_for_restart(waiting).await;
}

#[tokio::test]
async fn test_external_shutdown_answers_open_acquire_waits_at_once() {
    let daemon = Daemon::start().await;
    let waiting = open_acquire_wait(&daemon).await;
    let started = Instant::now();

    tokio::time::timeout(Duration::from_secs(5), daemon.stop())
        .await
        .unwrap();

    assert!(started.elapsed() < Duration::from_secs(5));
    assert_answered_for_restart(waiting).await;
}

#[tokio::test]
async fn test_shutdown_route_stops_the_server() {
    let mut daemon = Daemon::start().await;
    let anonymous = daemon
        .client
        .post(daemon.url("/shutdown"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    let response = send(daemon.post("/shutdown")).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let server = daemon.server.take().unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(5), server).await;
    stopped.unwrap().unwrap().unwrap();
}

/// Makes the test binary, run as `sleep_helper`, sleep instead of returning at once.
const HELPER_VAR: &str = "RH_DAEMON_TEST_HELPER";

/// How long a helper lives when nobody kills it, so a failed test leaves nothing behind.
const HELPER_LIFETIME: Duration = Duration::from_secs(60);

#[test]
#[ignore = "helper process for the kill test; does nothing without RH_DAEMON_TEST_HELPER"]
fn sleep_helper() {
    if env::var_os(HELPER_VAR).is_some() {
        thread::sleep(HELPER_LIFETIME);
    }
}

/// A sleeping copy of this test binary, the only kind of process these tests kill. Dropping it
/// terminates it if it still runs.
struct Helper {
    child: Child,
    start_time: u64,
}

impl Helper {
    fn spawn() -> Self {
        let child = Command::new(env::current_exe().unwrap())
            .args(["sleep_helper", "--exact", "--ignored"])
            .env(HELPER_VAR, "1")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let start_time = rh_sampler::process_table()
            .get(child.id())
            .expect("the helper is running")
            .start_time;
        Self { child, start_time }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// The helper as a fake-sampler process started by `parent_pid`.
    fn as_child_of(&self, parent_pid: u32) -> RawProcess {
        RawProcess {
            parent_pid: Some(parent_pid),
            name: helper_name(),
            ..process(self.pid(), self.start_time)
        }
    }

    /// Waits up to 5 s for the helper to exit and returns its exit code.
    fn exit_code(&mut self) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("helper {} is still running", self.pid());
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn helper_name() -> String {
    env::current_exe()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

impl Daemon {
    async fn bind(&self, id: LeaseId, session: &str, tool_use_id: &str) -> Response {
        let path = format!("/leases/{}/bind?session={session}", id.0);
        let request = BindRequest {
            tool_use_id: tool_use_id.into(),
            command: Some("cargo build".into()),
        };
        send(self.post(&path).json(&request)).await
    }

    async fn kill(&self, id: LeaseId) -> Response {
        send(self.post(&format!("/leases/{}/kill", id.0))).await
    }

    async fn remove(&self, id: LeaseId) -> Response {
        send(self.post(&format!("/leases/{}/remove", id.0))).await
    }

    async fn notices(&self, session: &str) -> Vec<Notice> {
        let response = self.get(&format!("/notices?session={session}")).await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<NoticesResponse>().await.unwrap().notices
    }

    async fn deliver(&self, id: LeaseId, session: &str) -> Response {
        send(self.post(&format!("/notices/{}/delivered?session={session}", id.0))).await
    }

    async fn ui_state(&self) -> UiState {
        let response = self.get("/ui/state").await;
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    fn add_process(&self, process: RawProcess) {
        self.machine.lock().unwrap().processes.push(process);
    }
}

#[tokio::test]
async fn test_kill_terminates_only_the_bound_tree_and_leaves_a_notice() {
    let daemon = Daemon::start().await;
    let lease = daemon.acquire_build(alice(), 0).await.lease;
    let bound = daemon.bind(lease.id, "alice", "toolu_01Kill").await;
    assert_eq!(bound.status(), StatusCode::NO_CONTENT);

    let mut command = Helper::spawn();
    daemon.add_process(command.as_child_of(ALICE_PID));
    eventually("the command's process to be bound", || async {
        let state = daemon.ui_state().await;
        state.active[0]
            .process
            .as_ref()
            .is_some_and(|process| process.bound && process.pid == command.pid())
    })
    .await;
    // Started after the binding, so it is another command of the same session.
    let mut other = Helper::spawn();
    daemon.add_process(other.as_child_of(ALICE_PID));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let response = daemon.kill(lease.id).await;
    assert_eq!(response.status(), StatusCode::OK);
    let killed: KillResponse = response.json().await.unwrap();
    let terminated: Vec<u32> = killed
        .terminated
        .iter()
        .map(|process| process.pid)
        .collect();
    assert_eq!(terminated, [command.pid()]);
    assert!(killed.failed.is_empty(), "{killed:?}");
    assert_eq!(killed.untouched_reason, None);
    assert_eq!(killed.lease.state, LeaseState::Expired);
    assert_eq!(killed.lease.end_cause, Some(EndCause::Killed));

    assert_eq!(command.exit_code(), Some(-1));
    assert!(other.child.try_wait().unwrap().is_none());
    let notices = daemon.notices("alice").await;
    assert_eq!(notices.len(), 1);
    assert_eq!(
        (notices[0].lease_id, notices[0].cause),
        (lease.id, EndCause::Killed)
    );
    assert!(daemon.leases("?state=active").await.is_empty());
    let recent = daemon.ui_state().await.recent;
    assert_eq!(recent[0].lease_id, Some(lease.id));
    assert_eq!(recent[0].end_cause, Some(EndCause::Killed));
    daemon.stop().await;
}

#[tokio::test]
async fn test_kill_without_a_bound_tree_ends_the_lease_and_terminates_nothing() {
    let daemon = Daemon::start().await;
    let lease = daemon.acquire_build(alice(), 0).await.lease;

    let killed: KillResponse = daemon.kill(lease.id).await.json().await.unwrap();
    assert!(killed.terminated.is_empty());
    assert_eq!(
        killed.untouched_reason.as_deref(),
        Some("the lease has no bound process tree")
    );
    assert_eq!(killed.lease.end_cause, Some(EndCause::Killed));
    assert_eq!(daemon.kill(lease.id).await.status(), StatusCode::CONFLICT);
    assert_eq!(
        daemon.kill(LeaseId(404)).await.status(),
        StatusCode::NOT_FOUND
    );
    daemon.stop().await;
}

#[tokio::test]
async fn test_remove_answers_the_acquire_waiting_on_the_lease() {
    let daemon = Daemon::start().await;
    let waiting = open_acquire_wait(&daemon).await;
    let queued = daemon.leases("?state=queued").await;
    let id = queued[0].id;
    let active = daemon.leases("?state=active").await[0].id;
    assert_eq!(daemon.remove(active).await.status(), StatusCode::CONFLICT);
    assert_eq!(daemon.kill(id).await.status(), StatusCode::CONFLICT);

    let started = Instant::now();
    let response = daemon.remove(id).await;
    assert_eq!(response.status(), StatusCode::OK);
    let removed: RemoveResponse = response.json().await.unwrap();
    assert_eq!(removed.lease.end_cause, Some(EndCause::Removed));

    let answer = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    let answer: AcquireResponse = answer.json().await.unwrap();
    assert_eq!(answer.lease.id, id);
    assert_eq!(answer.lease.state, LeaseState::Expired);
    assert_eq!(answer.lease.end_cause, Some(EndCause::Removed));
    assert!(!answer.verdict.fits);
    let reasons = answer.verdict.reasons.join(" | ");
    assert!(
        reasons.contains("removed this request from the queue"),
        "{reasons}"
    );

    let resumed: AcquireResponse = daemon.wait(id, "bob", 30).await.json().await.unwrap();
    assert_eq!(resumed.lease.end_cause, Some(EndCause::Removed));
    assert_eq!(daemon.remove(id).await.status(), StatusCode::CONFLICT);
    assert_eq!(daemon.notices("bob").await[0].cause, EndCause::Removed);
    daemon.stop().await;
}

#[tokio::test]
async fn test_notice_is_delivered_once_and_only_to_its_session() {
    let daemon = Daemon::start().await;
    let lease = daemon.acquire_build(alice(), 0).await.lease;
    assert_eq!(daemon.kill(lease.id).await.status(), StatusCode::OK);
    assert!(daemon.notices("bob").await.is_empty());
    assert_eq!(
        daemon.deliver(lease.id, "bob").await.status(),
        StatusCode::FORBIDDEN
    );

    let first: DeliveredResponse = daemon
        .deliver(lease.id, "alice")
        .await
        .json()
        .await
        .unwrap();
    assert!(first.delivered);
    let again: DeliveredResponse = daemon
        .deliver(lease.id, "alice")
        .await
        .json()
        .await
        .unwrap();
    assert!(!again.delivered);
    assert!(daemon.notices("alice").await.is_empty());
    let shown = daemon.ui_state().await.notices;
    assert_eq!(shown[0].lease_id, lease.id);
    assert!(shown[0].delivered_at_ms.is_some());
    assert_eq!(
        daemon.deliver(LeaseId(404), "alice").await.status(),
        StatusCode::NOT_FOUND
    );
    daemon.stop().await;
}

#[tokio::test]
async fn test_bind_checks_the_owner_and_the_state() {
    let daemon = Daemon::start().await;
    let active = daemon.acquire_build(alice(), 0).await.lease;
    let queued = daemon.acquire_build(bob(), 0).await.lease;

    let cases = [
        (active.id, "bob", "toolu_1", StatusCode::FORBIDDEN),
        (active.id, "alice", " ", StatusCode::UNPROCESSABLE_ENTITY),
        (queued.id, "bob", "toolu_2", StatusCode::CONFLICT),
        (LeaseId(404), "alice", "toolu_3", StatusCode::NOT_FOUND),
        (active.id, "alice", "toolu_4", StatusCode::NO_CONTENT),
    ];
    for (id, session, tool_use_id, status) in cases {
        let response = daemon.bind(id, session, tool_use_id).await;
        assert_eq!(response.status(), status, "{session} {tool_use_id}");
    }
    let stored = repo::get_lease(&mut daemon.db(), active.id)
        .unwrap()
        .unwrap();
    let binding = stored.binding.unwrap();
    assert_eq!(binding.tool_use_id, "toolu_4");
    assert_eq!(binding.root, None);

    daemon.release_response(active.id, "alice").await;
    let ended = daemon.bind(active.id, "alice", "toolu_5").await;
    assert_eq!(ended.status(), StatusCode::CONFLICT);
    daemon.stop().await;
}

/// Fails when `live` has a key `golden` lacks or the other way round, or a value of another
/// JSON type. A `null` on either side matches anything, and arrays compare their first items.
fn assert_same_shape(path: &str, live: &Value, golden: &Value) {
    match (live, golden) {
        (Value::Null, _) | (_, Value::Null) => {}
        (Value::Object(live), Value::Object(golden)) => {
            let live_keys: Vec<&String> = live.keys().collect();
            let golden_keys: Vec<&String> = golden.keys().collect();
            assert_eq!(live_keys, golden_keys, "keys of {path}");
            for (key, value) in live {
                assert_same_shape(&format!("{path}.{key}"), value, &golden[key]);
            }
        }
        (Value::Array(live), Value::Array(golden)) => {
            if let (Some(live), Some(golden)) = (live.first(), golden.first()) {
                assert_same_shape(&format!("{path}[0]"), live, golden);
            }
        }
        (Value::Number(_), Value::Number(_))
        | (Value::String(_), Value::String(_))
        | (Value::Bool(_), Value::Bool(_)) => {}
        _ => panic!("{path} is {live} here but {golden} in the fixture"),
    }
}

#[tokio::test]
async fn test_ui_state_has_the_shape_of_the_busy_fixture() {
    let daemon = Daemon::start().await;
    daemon.add_process(RawProcess {
        parent_pid: Some(ALICE_PID),
        name: "cargo.exe".into(),
        exe: Some(r"C:\Users\Mehran\.cargo\bin\cargo.exe".into()),
        usage: ResourceUsage {
            cores: 6.0,
            ram_mb: 900,
            gpu_pct: 0.0,
            vram_mb: 0,
        },
        ..process(7_001, 300)
    });
    let request = AcquireRequest {
        holder: alice(),
        profile: ProfileSpec::Class(JobClass::CpuHeavy),
        kind: LeaseKind::Normal,
        label: "cargo build --release".into(),
        cwd: Some(r"E:\work\app".into()),
        wait_timeout_secs: 0,
    };
    let response = send(daemon.post("/leases").json(&request)).await;
    assert_eq!(response.status(), StatusCode::OK);
    daemon.acquire_build(bob(), 0).await;
    let removed = daemon.acquire_build(bob(), 0).await.lease;
    assert_eq!(daemon.remove(removed.id).await.status(), StatusCode::OK);
    eventually("the session's busiest process", || async {
        daemon.ui_state().await.active[0].process.is_some()
    })
    .await;

    let state = daemon.ui_state().await;
    assert_eq!(state.service.pid, process::id());
    assert!(state.first_session_at_ms.is_some());
    assert_eq!(state.active.len(), 1);
    let shown = state.active[0].process.as_ref().unwrap();
    assert_eq!((shown.pid, shown.bound), (7_001, false));
    assert_eq!(state.queue[0].position, 1);
    assert!(!state.queue[0].verdict.fits);
    assert_eq!(state.meters.cpu.reserved_cores, 8.0);
    assert!(!state.meters.cpu.history_pct.is_empty());
    assert!(state.meters.cpu.history_pct.len() <= 24);
    let alice_session = state
        .sessions
        .iter()
        .find(|session| session.session.0 == "alice")
        .unwrap();
    assert_eq!(alice_session.cwd.as_deref(), Some(r"E:\work\app"));
    assert_eq!(state.notices[0].cause, EndCause::Removed);

    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../contract/fixtures/ui-state-busy.json");
    let golden: Value = serde_json::from_str(&fs::read_to_string(fixture).unwrap()).unwrap();
    let live: Value = daemon.get("/ui/state").await.json().await.unwrap();
    assert_same_shape("ui_state", &live, &golden);
    daemon.stop().await;
}

#[tokio::test]
async fn test_ui_state_waits_for_the_first_sample_and_needs_the_token() {
    let daemon = Daemon::start().await;
    let anonymous = daemon
        .client
        .get(daemon.url("/ui/state"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let state = daemon.ui_state().await;
    assert_eq!(state.first_session_at_ms, None);
    assert!(state.active.is_empty() && state.queue.is_empty());
    daemon.stop().await;
}

#[tokio::test]
async fn test_graceful_shutdown_after_the_flyout_routes_takes_milliseconds() {
    let daemon = Daemon::start().await;
    let waiting = open_acquire_wait(&daemon).await;
    daemon.ui_state().await;
    let queued = daemon.acquire_build(alice(), 0).await.lease;
    assert_eq!(daemon.remove(queued.id).await.status(), StatusCode::OK);
    daemon.notices("alice").await;
    let started = Instant::now();

    tokio::time::timeout(Duration::from_secs(5), daemon.stop())
        .await
        .unwrap();

    assert!(
        started.elapsed() < Duration::from_millis(500),
        "shutdown took {:?}",
        started.elapsed()
    );
    assert_answered_for_restart(waiting).await;
}
