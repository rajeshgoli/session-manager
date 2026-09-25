//! Reusable connections to the usage database.
//!
//! Opening a SQLite connection and applying its pragmas makes SQLite read and
//! parse the schema, which dominated the server's idle CPU when every usage
//! store call opened a fresh connection (#1546). A pool keeps released
//! connections for reuse while still giving concurrent or nested callers their
//! own connection, so WAL readers never serialize behind one another and a
//! caller that opens a second connection while holding the first cannot
//! deadlock.

use std::{
    fs,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Idle connections kept per pool. Callers beyond this still get a
/// connection; it is closed on release instead of returned.
const MAX_IDLE_CONNECTIONS: usize = 4;

#[derive(Debug, Clone)]
pub(crate) struct UsageDbPool {
    db_path: PathBuf,
    busy_timeout: Duration,
    idle: Arc<Mutex<Vec<Connection>>>,
}

impl UsageDbPool {
    pub(crate) fn new(db_path: PathBuf, busy_timeout: Duration) -> Self {
        Self {
            db_path,
            busy_timeout,
            idle: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Take an idle connection, or open and configure a new one.
    pub(crate) fn get(&self) -> Result<PooledConnection> {
        let reused = self
            .idle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop();
        let connection = match reused {
            Some(connection) => connection,
            None => self.connect()?,
        };
        Ok(PooledConnection {
            connection: Some(connection),
            idle: Arc::clone(&self.idle),
        })
    }

    fn connect(&self) -> Result<Connection> {
        if let Some(parent) = self
            .db_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create usage DB directory {}", parent.display())
            })?;
        }
        let connection = Connection::open(&self.db_path)
            .with_context(|| format!("failed to open usage DB {}", self.db_path.display()))?;
        connection.busy_timeout(self.busy_timeout)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        Ok(connection)
    }
}

/// A connection borrowed from a [`UsageDbPool`]; it returns to the pool on drop.
pub(crate) struct PooledConnection {
    connection: Option<Connection>,
    idle: Arc<Mutex<Vec<Connection>>>,
}

impl Deref for PooledConnection {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.connection
            .as_ref()
            .expect("pooled connection is present until drop")
    }
}

impl DerefMut for PooledConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        self.connection
            .as_mut()
            .expect("pooled connection is present until drop")
    }
}

impl Drop for PooledConnection {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        // A connection left inside a transaction (for example a raw
        // `BEGIN ... COMMIT` batch that failed midway) would leak that
        // transaction into the next caller, so close it instead.
        if !connection.is_autocommit() {
            return;
        }
        let mut idle = self
            .idle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if idle.len() < MAX_IDLE_CONNECTIONS {
            idle.push(connection);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(name: &str) -> (PathBuf, UsageDbPool) {
        let dir = std::env::temp_dir().join(format!(
            "sm-usage-db-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let pool = UsageDbPool::new(dir.join("usage.db"), Duration::from_secs(5));
        (dir, pool)
    }

    /// Temp tables are private to one connection, so a marker table tells
    /// whether two borrows share a connection.
    fn mark(connection: &Connection) {
        connection
            .execute_batch("CREATE TEMP TABLE pool_marker (x INTEGER);")
            .unwrap();
    }

    fn is_marked(connection: &Connection) -> bool {
        connection
            .query_row(
                "SELECT COUNT(*) FROM temp.sqlite_master WHERE name = 'pool_marker'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            == 1
    }

    #[test]
    fn released_connection_is_reused_and_keeps_its_pragmas() {
        let (dir, pool) = pool("reuse");
        let first = pool.get().unwrap();
        mark(&first);
        drop(first);

        let second = pool.get().unwrap();
        assert!(is_marked(&second));
        let foreign_keys: bool = second
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        let journal_mode: String = second
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert!(foreign_keys);
        assert_eq!(journal_mode, "wal");
        drop(second);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn nested_callers_get_distinct_connections() {
        let (dir, pool) = pool("nested");
        let outer = pool.get().unwrap();
        mark(&outer);
        let inner = pool.get().unwrap();
        assert!(!is_marked(&inner));
        drop(inner);
        drop(outer);
        assert_eq!(pool.idle.lock().unwrap().len(), 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn connection_left_in_a_transaction_is_not_reused() {
        let (dir, pool) = pool("open-tx");
        let connection = pool.get().unwrap();
        connection.execute_batch("BEGIN IMMEDIATE;").unwrap();
        drop(connection);
        assert!(pool.idle.lock().unwrap().is_empty());

        let next = pool.get().unwrap();
        assert!(next.is_autocommit());
        drop(next);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn idle_connections_are_capped() {
        let (dir, pool) = pool("cap");
        let held = (0..MAX_IDLE_CONNECTIONS + 2)
            .map(|_| pool.get().unwrap())
            .collect::<Vec<_>>();
        drop(held);
        assert_eq!(pool.idle.lock().unwrap().len(), MAX_IDLE_CONNECTIONS);
        let _ = fs::remove_dir_all(dir);
    }
}
