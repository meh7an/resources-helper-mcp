// No console window when the logon task starts rhd. Output goes to rhd.log, not the console.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{fs, net::Ipv4Addr, time::Duration};

use anyhow::Context;
use clap::Parser;
use rh_core::sampler::Sampler;
use rh_sampler::SystemSampler;
use rh_shared::{config::Config, paths::Paths};
use rhd::{DaemonOptions, Maintenance, db, startup};
use tokio::net::TcpListener;

/// Resource coordinator daemon: samples system usage and arbitrates heavy-job leases.
#[derive(Debug, Parser)]
#[command(version)]
struct Args {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Args::parse();
    let paths = Paths::from_env()?;
    fs::create_dir_all(paths.root())
        .with_context(|| format!("creating {}", paths.root().display()))?;
    let _lock = startup::InstanceLock::acquire(&paths.lock_file())?;
    startup::init_logging(&paths.log_dir())?;

    // A detached rhd has no console, so the log is the only place a failure shows up.
    let result = run(&paths).await;
    if let Err(error) = &result {
        tracing::error!("daemon failed: {error:#}");
    }
    result
}

async fn run(paths: &Paths) -> anyhow::Result<()> {
    let config_file = paths.config_file();
    let config = Config::load(&config_file)?;
    let token = startup::load_or_create_token(&paths.token_file())?;
    let pool = db::open_pool(&paths.database_file())?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, config.daemon.port))
        .await
        .with_context(|| {
            format!(
                "binding 127.0.0.1:{}; another program may use the port (set daemon.port in {})",
                config.daemon.port,
                config_file.display()
            )
        })?;

    let options = DaemonOptions {
        config,
        token,
        pool,
        sampler: Box::new(|| Ok(Box::new(SystemSampler::new()?) as Box<dyn Sampler>)),
        sample_interval: Duration::from_secs(1),
        maintenance: Maintenance::standard(config_file),
    };
    rhd::serve(listener, options, shutdown_signal()).await
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!("listening for Ctrl+C failed: {error}");
        std::future::pending::<()>().await;
    }
    tracing::info!("shutdown requested");
}
