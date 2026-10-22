//! `rh-mcp`: the stdio MCP server one Claude session talks to. It acts for its parent
//! `claude.exe`, keeps the leases it acquires alive, and releases them when stdin closes.
//! Diagnostics go to stderr; stdout carries the protocol.

mod daemon;
mod held;
mod identity;
mod params;
mod server;
mod summary;
#[cfg(test)]
mod test_support;

use std::{sync::Arc, time::Duration};

use clap::Parser;
use rh_client::Client;
use rh_shared::{config::Config, paths::Paths};
use rmcp::{ServiceExt, transport::stdio};

use crate::{
    held::{HeldLeases, heartbeat_loop, release_all},
    server::RhServer,
};

/// Stdio MCP server exposing system snapshots and heavy-job leases to one Claude session.
#[derive(Debug, Parser)]
#[command(version)]
struct Args {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    Args::parse();
    let holder = identity::current_holder()?;
    let config = Config::load(&Paths::from_env()?.config_file())?;
    let client = Arc::new(Client::discover(true)?);
    let held = Arc::new(HeldLeases::default());

    // Registration may start rhd, which takes seconds; the session must not wait for it.
    let registering = {
        let (client, holder) = (client.clone(), holder.clone());
        tokio::spawn(async move { daemon::register(&*client, &holder).await })
    };
    let heartbeat = {
        let (client, session, held) = (client.clone(), holder.session.clone(), held.clone());
        let every = heartbeat_interval(&config);
        tokio::spawn(async move { heartbeat_loop(&*client, &session, &held, every).await })
    };

    // Claude starts the server in the session's project directory.
    let cwd = std::env::current_dir()
        .map(|dir| dir.display().to_string())
        .unwrap_or_default();
    let server = RhServer::new(client.clone(), holder.clone(), held.clone(), cwd);
    let outcome = match server.serve(stdio()).await {
        Ok(service) => service
            .waiting()
            .await
            .map(drop)
            .map_err(anyhow::Error::from),
        Err(error) => Err(error.into()),
    };

    heartbeat.abort();
    registering.abort();
    release_all(&*client, &holder.session, &held).await;
    outcome
}

/// A third of the lease TTL, so two heartbeats can be lost before a lease expires.
fn heartbeat_interval(config: &Config) -> Duration {
    Duration::from_secs((config.leases.heartbeat_ttl_secs / 3).max(1))
}
