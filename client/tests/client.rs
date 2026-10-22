//! The client against a real in-process daemon with a fake sampler.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use rh_client::{
    BlockingClient, Client, ClientError, ClientOptions, DEFAULT_CONNECT_TIMEOUT,
    DEFAULT_REQUEST_TIMEOUT, DaemonStart, SpawnOptions, TokenSource,
};
use rh_core::sampler::{RawProcess, RawSample, Sampler};
use rh_shared::{
    api::{AcquireRequest, CanFitRequest, LeaseQuery, ProfileSpec},
    config::Config,
    types::{Holder, JobClass, LeaseKind, LeaseState, MemStats, ResourceUsage, SessionKey},
};
use rhd::{DaemonOptions, db::open_pool};
use tempfile::TempDir;
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

const TOKEN: &str = "client-test-token";
const HOLDER_PID: u32 = 4_242;

struct FakeSampler(Arc<Mutex<RawSample>>);

impl Sampler for FakeSampler {
    fn sample(&mut self) -> rh_shared::Result<RawSample> {
        Ok(self.0.lock().unwrap().clone())
    }
}

fn machine() -> RawSample {
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
        processes: vec![RawProcess {
            pid: HOLDER_PID,
            parent_pid: None,
            name: "claude.exe".into(),
            start_time: 100,
            usage: ResourceUsage::default(),
        }],
        gpus: vec![],
    }
}

fn holder() -> Holder {
    Holder {
        session: SessionKey("session one".into()),
        pid: HOLDER_PID,
        pid_start_time: 100,
    }
}

fn build_request(wait_timeout_secs: u64) -> AcquireRequest {
    AcquireRequest {
        holder: holder(),
        profile: ProfileSpec::Class(JobClass::CpuHeavy),
        kind: LeaseKind::Normal,
        label: "cargo build".into(),
        cwd: Some(r"E:\work\app".into()),
        wait_timeout_secs,
    }
}

fn options(addr: SocketAddr, token: TokenSource) -> ClientOptions {
    ClientOptions {
        base_url: format!("http://{addr}"),
        token,
        connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        request_timeout: DEFAULT_REQUEST_TIMEOUT,
        spawn: None,
    }
}

/// An address nothing listens on.
async fn closed_addr() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    listener.local_addr().unwrap()
}

struct Daemon {
    addr: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    server: JoinHandle<()>,
    _dir: TempDir,
}

impl Daemon {
    async fn start() -> Self {
        let dir = TempDir::new().unwrap();
        let pool = open_pool(&dir.path().join("rh.db")).unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sampled = Arc::new(Mutex::new(machine()));
        let options = DaemonOptions {
            config: Config::default(),
            token: TOKEN.into(),
            pool,
            sampler: Box::new(move || Ok(Box::new(FakeSampler(sampled)) as Box<dyn Sampler>)),
            sample_interval: Duration::from_millis(20),
        };
        let (stop, stopped) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let shutdown = async {
                let _ = stopped.await;
            };
            rhd::serve(listener, options, shutdown).await.unwrap();
        });
        let daemon = Self {
            addr,
            stop: Some(stop),
            server,
            _dir: dir,
        };
        let client = daemon.client();
        for _ in 0..300 {
            if client.snapshot().await.is_ok() {
                return daemon;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the daemon never produced a snapshot");
    }

    fn client(&self) -> Client {
        Client::new(options(self.addr, TokenSource::Fixed(TOKEN.into()))).unwrap()
    }

    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.server.await.unwrap();
    }
}

#[tokio::test]
async fn test_client_reads_health_snapshot_and_sessions() {
    let daemon = Daemon::start().await;
    let client = daemon.client();

    assert_eq!(client.health().await.unwrap().pid, std::process::id());
    assert_eq!(client.snapshot().await.unwrap().cpu.logical_cores, 16);
    client.register_session(&holder()).await.unwrap();
    for _ in 0..300 {
        if !client.sessions().await.unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let sessions = client.sessions().await.unwrap();
    assert_eq!(sessions[0].holder, holder());
    daemon.stop().await;
}

#[tokio::test]
async fn test_client_lease_lifecycle() {
    let daemon = Daemon::start().await;
    let client = daemon.client();
    let session = holder().session;

    let request = CanFitRequest {
        profile: ProfileSpec::Class(JobClass::CpuHeavy),
        kind: LeaseKind::Normal,
        label: Some("cargo build".into()),
        cwd: Some(r"E:\work\app".into()),
    };
    assert!(client.can_fit(&request).await.unwrap().fits);

    let acquired = client.acquire(&build_request(0)).await.unwrap();
    assert_eq!(acquired.lease.state, LeaseState::Active);
    let id = acquired.lease.id;

    let mine = LeaseQuery {
        session: Some(session.clone()),
        state: Some(LeaseState::Active),
    };
    assert_eq!(client.leases(&mine).await.unwrap()[0].id, id);
    assert_eq!(
        client.heartbeat(id, &session).await.unwrap().state,
        LeaseState::Active
    );
    let waited = client.wait(id, &session, 5).await.unwrap();
    assert_eq!(waited.lease.state, LeaseState::Active);
    let released = client.release(id, &session).await.unwrap();
    assert_eq!(released.lease.state, LeaseState::Released);
    assert_eq!(released.noise, None);
    assert!(client.leases(&mine).await.unwrap().is_empty());
    daemon.stop().await;
}

#[tokio::test]
async fn test_client_reads_empty_history_with_an_encoded_fingerprint() {
    let daemon = Daemon::start().await;
    let client = daemon.client();
    assert!(client.history().await.unwrap().is_empty());
    let runs = client
        .history_runs(r"cargo build -p a&b|e:\work\app", Some(5))
        .await
        .unwrap();
    assert!(runs.is_empty());
    daemon.stop().await;
}

#[tokio::test]
async fn test_client_maps_error_statuses() {
    let daemon = Daemon::start().await;
    let client = daemon.client();
    let id = client.acquire(&build_request(0)).await.unwrap().lease.id;

    let stranger = SessionKey("someone else".into());
    let error = client.heartbeat(id, &stranger).await.unwrap_err();
    assert_eq!(error.status(), Some(403));
    assert!(!error.is_unreachable());

    let wrong = Client::new(options(daemon.addr, TokenSource::Fixed("wrong".into()))).unwrap();
    assert_eq!(wrong.snapshot().await.unwrap_err().status(), Some(401));
    daemon.stop().await;
}

#[tokio::test]
async fn test_client_without_daemon_is_unreachable() {
    let addr = closed_addr().await;
    let client = Client::new(options(addr, TokenSource::Fixed(TOKEN.into()))).unwrap();
    let error = client.snapshot().await.unwrap_err();
    assert!(error.is_unreachable(), "{error}");
    assert!(client.health().await.unwrap_err().is_unreachable());
}

#[tokio::test]
async fn test_client_missing_token_file_counts_as_unreachable() {
    let daemon = Daemon::start().await;
    let missing = PathBuf::from("does-not-exist").join("token");
    let client = Client::new(options(daemon.addr, TokenSource::File(missing))).unwrap();
    let error = client.snapshot().await.unwrap_err();
    assert!(matches!(error, ClientError::NoToken(_)), "{error}");
    assert!(error.is_unreachable());
    daemon.stop().await;
}

#[tokio::test]
async fn test_client_reads_token_from_file() {
    let daemon = Daemon::start().await;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("token");
    std::fs::write(&path, format!("{TOKEN}\n")).unwrap();
    let client = Client::new(options(daemon.addr, TokenSource::File(path))).unwrap();
    assert!(client.snapshot().await.is_ok());
    daemon.stop().await;
}

#[tokio::test]
async fn test_client_spawn_failure_is_reported() {
    let addr = closed_addr().await;
    let mut options = options(addr, TokenSource::Fixed(TOKEN.into()));
    options.spawn = Some(SpawnOptions {
        daemon_exe: PathBuf::from("Z:\\no\\such\\rhd.exe"),
        ready_timeout: Duration::from_millis(200),
    });
    let client = Client::new(options).unwrap();
    let error = client.snapshot().await.unwrap_err();
    assert!(matches!(error, ClientError::Spawn(_)), "{error}");
}

#[tokio::test]
async fn test_ensure_running_sees_a_running_daemon() {
    let daemon = Daemon::start().await;
    let started = daemon.client().ensure_running().await.unwrap();
    assert_eq!(started, DaemonStart::AlreadyRunning);
    daemon.stop().await;
}

#[tokio::test]
async fn test_shutdown_stops_the_daemon() {
    let daemon = Daemon::start().await;
    daemon.client().shutdown().await.unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(5), daemon.server).await;
    stopped.unwrap().unwrap();
}

#[tokio::test]
async fn test_blocking_client_works_outside_the_runtime() {
    let daemon = Daemon::start().await;
    let options = options(daemon.addr, TokenSource::Fixed(TOKEN.into()));
    let state = tokio::task::spawn_blocking(move || {
        let client = BlockingClient::new(options).unwrap();
        let lease = client.acquire(&build_request(0)).unwrap().lease;
        client
            .release(lease.id, &holder().session)
            .unwrap()
            .lease
            .state
    })
    .await
    .unwrap();
    assert_eq!(state, LeaseState::Released);
    daemon.stop().await;
}
