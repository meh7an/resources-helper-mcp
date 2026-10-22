//! `rh daemon start|stop|status`.

use std::{
    thread,
    time::{Duration, Instant},
};

use anyhow::bail;
use rh_client::{BlockingClient, DaemonStart};

const STOP_WAIT: Duration = Duration::from_secs(10);
const STOP_POLL: Duration = Duration::from_millis(100);

/// Starts rhd in the background unless it is already running.
///
/// # Errors
///
/// Fails when rhd cannot be started or does not become ready.
pub fn start() -> anyhow::Result<()> {
    let client = BlockingClient::discover(true)?;
    let started = client.ensure_running()?;
    let health = client.health()?;
    let verb = match started {
        DaemonStart::AlreadyRunning => "already running",
        DaemonStart::Started => "started",
    };
    println!("rhd {verb} at {} (pid {})", client.base_url(), health.pid);
    Ok(())
}

/// Asks rhd to shut down and waits until it stops answering.
///
/// # Errors
///
/// Fails when rhd refuses the request or keeps answering after the wait.
pub fn stop() -> anyhow::Result<()> {
    let client = BlockingClient::discover(false)?;
    match shutdown(&client)? {
        Some(_) => println!("rhd stopped"),
        None => println!("rhd is not running"),
    }
    Ok(())
}

/// Asks the rhd behind `client` to shut down and waits until it stops answering. Returns its
/// pid, or `None` when it was not running.
///
/// # Errors
///
/// Fails when rhd refuses the request or keeps answering after the wait.
pub fn shutdown(client: &BlockingClient) -> anyhow::Result<Option<u32>> {
    let pid = match client.health() {
        Ok(health) => health.pid,
        Err(error) if error.is_unsent() => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match client.shutdown() {
        Err(error) if error.is_unsent() => return Ok(None),
        other => other?,
    }
    let deadline = Instant::now() + STOP_WAIT;
    // A session may start a new rhd the moment the old one exits; only the old one counts.
    while client.health().is_ok_and(|health| health.pid == pid) {
        if Instant::now() >= deadline {
            bail!(
                "rhd is still answering {} s after the shutdown request",
                STOP_WAIT.as_secs()
            );
        }
        thread::sleep(STOP_POLL);
    }
    Ok(Some(pid))
}

/// Reports whether rhd is running, without starting it.
///
/// # Errors
///
/// Fails when the local configuration cannot be read.
pub fn status() -> anyhow::Result<()> {
    let client = BlockingClient::discover(false)?;
    match client.health() {
        Ok(health) => {
            println!(
                "rhd {} running at {} (pid {}, up {} s)",
                health.version,
                client.base_url(),
                health.pid,
                health.uptime_secs
            );
            if let Some(error) = health.config_error {
                println!("config rejected, rhd keeps the previous one: {error}");
            }
        }
        Err(error) if error.is_unreachable() => {
            println!("rhd is not running at {}", client.base_url());
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
