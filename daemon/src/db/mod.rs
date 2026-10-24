//! SQLite persistence for leases, their audit trail, and job history.

mod models;
pub mod repo;
#[rustfmt::skip]
mod schema;
#[cfg(test)]
mod test_support;

use std::{fs, path::Path};

use anyhow::{Context, anyhow};
use diesel::{
    QueryResult,
    connection::SimpleConnection,
    r2d2::{ConnectionManager, CustomizeConnection, Pool},
    sqlite::SqliteConnection,
};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};

const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

// SQLite allows one writer at a time, so more connections only add lock contention.
const MAX_CONNECTIONS: u32 = 4;
const BUSY_TIMEOUT_MS: u32 = 5_000;

/// Pool of connections to the daemon's database.
pub type DbPool = Pool<ConnectionManager<SqliteConnection>>;

/// Opens the database at `path`, creating the file and its directory when missing, and
/// applies every pending migration.
///
/// # Errors
///
/// Returns an error when the directory cannot be created, the path is not UTF-8, the pool
/// cannot connect, or a migration fails.
pub fn open_pool(path: &Path) -> anyhow::Result<DbPool> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating database directory {}", parent.display()))?;
    }
    let url = path.to_str().context("database path is not valid UTF-8")?;
    let pool = Pool::builder()
        .max_size(MAX_CONNECTIONS)
        .connection_customizer(Box::new(ConnectionSetup))
        .build(ConnectionManager::new(url))
        .with_context(|| format!("opening database {}", path.display()))?;
    let mut conn = pool.get()?;
    run_migrations(&mut conn)?;
    drop(conn);
    Ok(pool)
}

/// Applies every migration the database has not seen yet.
///
/// # Errors
///
/// Returns an error when a migration fails; that migration's changes are rolled back.
pub fn run_migrations(conn: &mut SqliteConnection) -> anyhow::Result<()> {
    conn.run_pending_migrations(MIGRATIONS)
        .map_err(|error| anyhow!(error))?;
    Ok(())
}

/// Sets the pragmas every connection needs.
///
/// WAL lets readers continue while another pooled connection writes, and `synchronous =
/// NORMAL` is crash-safe under WAL. SQLite leaves foreign keys off per connection unless
/// asked, which would silently skip the cascades the migrations declare.
pub(crate) fn configure(conn: &mut SqliteConnection) -> QueryResult<()> {
    conn.batch_execute(&format!(
        "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON; \
         PRAGMA busy_timeout = {BUSY_TIMEOUT_MS};"
    ))
}

#[derive(Debug)]
struct ConnectionSetup;

impl CustomizeConnection<SqliteConnection, diesel::r2d2::Error> for ConnectionSetup {
    fn on_acquire(&self, conn: &mut SqliteConnection) -> Result<(), diesel::r2d2::Error> {
        configure(conn).map_err(diesel::r2d2::Error::QueryError)
    }
}

#[cfg(test)]
mod tests {
    use diesel::{
        QueryableByName, RunQueryDsl, sql_query,
        sql_types::{BigInt, Text},
    };
    use tempfile::TempDir;

    use rh_shared::types::{EndCause, LeaseId, SessionKey};

    use super::*;
    use crate::db::{repo, test_support::sample_draft};

    const TABLES: [&str; 4] = ["job_runs", "lease_events", "leases", "meta"];
    // End causes, lease bindings, and the meta table.
    const V0_2_MIGRATIONS: usize = 3;

    #[derive(QueryableByName)]
    struct TableName {
        #[diesel(sql_type = Text)]
        name: String,
    }

    #[derive(QueryableByName)]
    struct JournalMode {
        #[diesel(sql_type = Text)]
        journal_mode: String,
    }

    #[derive(QueryableByName)]
    struct ForeignKeys {
        #[diesel(sql_type = BigInt)]
        foreign_keys: i64,
    }

    fn table_names(conn: &mut SqliteConnection) -> Vec<String> {
        sql_query(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%' AND name != '__diesel_schema_migrations' ORDER BY name",
        )
        .load::<TableName>(conn)
        .unwrap()
        .into_iter()
        .map(|row| row.name)
        .collect()
    }

    #[test]
    fn test_open_pool_missing_directory_creates_database_with_every_table() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("rh.db");
        let pool = open_pool(&path).unwrap();
        assert!(path.is_file());
        assert_eq!(table_names(&mut pool.get().unwrap()), TABLES);
    }

    #[test]
    fn test_migrations_revert_and_rerun_cleanly() {
        let dir = TempDir::new().unwrap();
        let pool = open_pool(&dir.path().join("rh.db")).unwrap();
        let mut conn = pool.get().unwrap();
        conn.revert_all_migrations(MIGRATIONS).unwrap();
        assert!(table_names(&mut conn).is_empty());
        run_migrations(&mut conn).unwrap();
        assert_eq!(table_names(&mut conn), TABLES);
    }

    #[test]
    fn test_open_pool_connections_use_wal_and_foreign_keys() {
        let dir = TempDir::new().unwrap();
        let pool = open_pool(&dir.path().join("rh.db")).unwrap();
        let mut conn = pool.get().unwrap();
        let mode = sql_query("PRAGMA journal_mode").get_result::<JournalMode>(&mut conn);
        assert_eq!(mode.unwrap().journal_mode, "wal");
        let keys = sql_query("PRAGMA foreign_keys").get_result::<ForeignKeys>(&mut conn);
        assert_eq!(keys.unwrap().foreign_keys, 1);
    }

    #[test]
    fn test_open_pool_existing_database_keeps_stored_leases() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("rh.db");
        let pool = open_pool(&path).unwrap();
        let lease = repo::insert_lease(&mut pool.get().unwrap(), &sample_draft("s1", 1_000));
        let lease = lease.unwrap();
        drop(pool);

        let reopened = open_pool(&path).unwrap();
        let stored = repo::get_lease(&mut reopened.get().unwrap(), lease.id).unwrap();
        assert_eq!(stored, Some(lease));
    }

    #[test]
    fn test_v0_2_migrations_fill_in_causes_sessions_and_the_first_session() {
        let dir = TempDir::new().unwrap();
        let pool = open_pool(&dir.path().join("rh.db")).unwrap();
        let mut conn = pool.get().unwrap();
        for _ in 0..V0_2_MIGRATIONS {
            conn.revert_last_migration(MIGRATIONS).unwrap();
        }
        sql_query(
            "INSERT INTO leases (id, session_id, holder_pid, holder_pid_start_time, kind, state, \
             class, cores, ram_mb, gpu_pct, vram_mb, est_secs, label, requested_at_ms, \
             granted_at_ms, ended_at_ms, expires_at_ms) VALUES \
             (1, 's1', 1, 1, 'normal', 'released', NULL, 1, 1, 0, 0, 1, 'a', 4000, 4100, 4200, 9), \
             (2, 's2', 1, 1, 'normal', 'expired', NULL, 1, 1, 0, 0, 1, 'b', 5000, NULL, 5100, 9), \
             (3, 's3', 1, 1, 'normal', 'active', NULL, 1, 1, 0, 0, 1, 'c', 6000, 6100, NULL, 9)",
        )
        .execute(&mut conn)
        .unwrap();
        sql_query(
            "INSERT INTO job_runs (lease_id, fingerprint, command, cwd, declared_cores, \
             declared_ram_mb, declared_gpu_pct, declared_vram_mb, declared_est_secs, peak_cores, \
             avg_cores, peak_ram_mb, avg_ram_mb, peak_gpu_pct, avg_gpu_pct, peak_vram_mb, \
             avg_vram_mb, started_at_ms, ended_at_ms) VALUES \
             (1, 'f', 'a', '', 1, 1, 0, 0, 1, 1, 1, 1, 1, 0, 0, 0, 0, 4100, 4200), \
             (NULL, 'g', 'old', '', 1, 1, 0, 0, 1, 1, 1, 1, 1, 0, 0, 0, 0, 3000, 3500)",
        )
        .execute(&mut conn)
        .unwrap();
        run_migrations(&mut conn).unwrap();

        let causes: Vec<Option<EndCause>> = [1, 2, 3]
            .into_iter()
            .map(|id| {
                repo::get_lease(&mut conn, LeaseId(id))
                    .unwrap()
                    .unwrap()
                    .end_cause
            })
            .collect();
        assert_eq!(
            causes,
            [Some(EndCause::Released), Some(EndCause::Expired), None]
        );
        let runs: Vec<(Option<SessionKey>, Option<EndCause>)> =
            repo::latest_job_runs(&mut conn, 10)
                .unwrap()
                .into_iter()
                .map(|run| (run.session, run.end_cause))
                .collect();
        assert_eq!(
            runs,
            [
                (Some(SessionKey("s1".into())), Some(EndCause::Released)),
                (None, Some(EndCause::Released)),
            ]
        );
        assert_eq!(repo::first_session_at_ms(&mut conn).unwrap(), Some(3_000));
    }
}
