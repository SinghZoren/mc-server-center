//! Database opening and migrations (ADR-006, OPS-02).
//!
//! The schema version lives in SQLite's `user_version`. Before any migration runs on an
//! existing database, a consistent copy is written to `db-backups/` with the SQLite backup API.
//! Each migration runs in its own transaction, so a failure leaves the database at the last
//! good version and the backup path is reported for manual recovery.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

/// A numbered migration. Numbers start at 1 and must be contiguous.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// Target schema version after this migration.
    pub version: u32,
    /// SQL to execute.
    pub sql: &'static str,
}

/// The application's migrations, in order.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: include_str!("../migrations/0001_initial.sql"),
    },
    Migration {
        version: 2,
        sql: include_str!("../migrations/0002_command_macros.sql"),
    },
    Migration {
        version: 3,
        sql: include_str!("../migrations/0003_paired_devices.sql"),
    },
];

/// Database open/migration failure.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// The database was created by a newer version of the app.
    #[error(
        "database schema v{found} is newer than this app supports (v{supported}); downgrade is not supported — restore a copy from db-backups/ or update the app"
    )]
    NewerSchema {
        /// Version found on disk.
        found: u32,
        /// Highest version this build knows.
        supported: u32,
    },
    /// A migration failed and was rolled back.
    #[error(
        "migration to v{version} failed: {source}; database left at v{left_at}; backup: {backup:?}"
    )]
    MigrationFailed {
        /// Migration that failed.
        version: u32,
        /// Version the database is still at.
        left_at: u32,
        /// Pre-migration backup, if one was taken.
        backup: Option<PathBuf>,
        /// Underlying error.
        #[source]
        source: rusqlite::Error,
    },
    /// Any other SQLite error.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    /// File-system error (e.g. creating the backup directory).
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<DbError> for msm_core::MsmError {
    fn from(e: DbError) -> Self {
        msm_core::MsmError::new(msm_core::ErrorCode::Internal, e.to_string())
    }
}

/// Current Unix time in milliseconds.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

fn user_version(conn: &Connection) -> rusqlite::Result<u32> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
}

/// Opens (creating if needed) the database at `path` and applies `migrations`, writing a
/// pre-migration copy into `backup_dir` when an existing database is upgraded.
///
/// # Errors
/// See [`DbError`].
pub fn open_with(
    path: &Path,
    backup_dir: &Path,
    migrations: &[Migration],
) -> Result<Connection, DbError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    configure(&conn)?;
    migrate(&conn, backup_dir, migrations)?;
    Ok(conn)
}

/// Opens the application database with [`MIGRATIONS`].
///
/// # Errors
/// See [`DbError`].
pub fn open(path: &Path, backup_dir: &Path) -> Result<Connection, DbError> {
    open_with(path, backup_dir, MIGRATIONS)
}

/// Opens a second, read-only connection to an already migrated database.
///
/// # Errors
/// Any SQLite error.
pub fn open_read_only(path: &Path) -> Result<Connection, DbError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

fn migrate(conn: &Connection, backup_dir: &Path, migrations: &[Migration]) -> Result<(), DbError> {
    let supported = migrations.last().map_or(0, |m| m.version);
    let found = user_version(conn)?;
    if found > supported {
        return Err(DbError::NewerSchema { found, supported });
    }
    if found == supported {
        return Ok(());
    }

    let backup = if found > 0 {
        std::fs::create_dir_all(backup_dir)?;
        let dest = backup_dir.join(format!("state-v{found}-{}.db", now_ms()));
        conn.backup(
            rusqlite::MAIN_DB,
            &dest,
            None::<fn(rusqlite::backup::Progress)>,
        )?;
        Some(dest)
    } else {
        None
    };

    let mut current = found;
    for m in migrations.iter().filter(|m| m.version > found) {
        debug_assert_eq!(m.version, current + 1, "migrations must be contiguous");
        let result = (|| {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(m.sql)?;
            tx.pragma_update(None, "user_version", m.version)?;
            tx.commit()
        })();
        if let Err(source) = result {
            return Err(DbError::MigrationFailed {
                version: m.version,
                left_at: current,
                backup,
                source,
            });
        }
        current = m.version;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("state.db");
        let backups = dir.path().join("db-backups");
        (dir, db, backups)
    }

    #[test]
    fn fresh_database_gets_latest_schema_without_backup() {
        let (_d, db, backups) = tmp();
        let conn = open(&db, &backups).unwrap();
        assert_eq!(user_version(&conn).unwrap(), 3);
        let fk: i64 = conn
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        assert!(!backups.exists());
        // Reopening is a no-op.
        drop(conn);
        let conn = open(&db, &backups).unwrap();
        assert_eq!(user_version(&conn).unwrap(), 3);
    }

    #[test]
    fn upgrade_takes_a_backup_first() {
        let (_d, db, backups) = tmp();
        let v1 = [Migration {
            version: 1,
            sql: "CREATE TABLE t (x INTEGER);",
        }];
        let conn = open_with(&db, &backups, &v1).unwrap();
        conn.execute("INSERT INTO t VALUES (42)", []).unwrap();
        drop(conn);

        let v2 = [
            v1[0],
            Migration {
                version: 2,
                sql: "ALTER TABLE t ADD COLUMN y TEXT;",
            },
        ];
        let conn = open_with(&db, &backups, &v2).unwrap();
        assert_eq!(user_version(&conn).unwrap(), 2);
        let files: Vec<_> = std::fs::read_dir(&backups).unwrap().collect();
        assert_eq!(files.len(), 1);
        let backup_path = files[0].as_ref().unwrap().path();
        let backup = Connection::open(backup_path).unwrap();
        assert_eq!(user_version(&backup).unwrap(), 1);
        let x: i64 = backup
            .query_row("SELECT x FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(x, 42);
    }

    #[test]
    fn failed_migration_rolls_back_and_reports_backup() {
        let (_d, db, backups) = tmp();
        let v1 = [Migration {
            version: 1,
            sql: "CREATE TABLE t (x INTEGER);",
        }];
        drop(open_with(&db, &backups, &v1).unwrap());

        let bad = [
            v1[0],
            Migration {
                version: 2,
                sql: "CREATE TABLE u (y INTEGER); THIS IS NOT SQL;",
            },
        ];
        let err = open_with(&db, &backups, &bad).unwrap_err();
        match err {
            DbError::MigrationFailed {
                version: 2,
                left_at: 1,
                backup: Some(path),
                ..
            } => {
                assert!(path.exists());
            }
            other => panic!("unexpected {other:?}"),
        }
        // The partial table from the failed migration was rolled back.
        let conn = Connection::open(&db).unwrap();
        assert_eq!(user_version(&conn).unwrap(), 1);
        let has_u: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'u'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_u, 0);
    }

    #[test]
    fn newer_schema_is_refused() {
        let (_d, db, backups) = tmp();
        let conn = Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        drop(conn);
        assert!(matches!(
            open(&db, &backups),
            Err(DbError::NewerSchema {
                found: 99,
                supported: 3
            })
        ));
    }

    #[test]
    fn audit_table_is_append_only() {
        let (_d, db, backups) = tmp();
        let conn = open(&db, &backups).unwrap();
        conn.execute(
            "INSERT INTO audit (id, at, principal, action, outcome) VALUES ('a', 1, 'local-owner', 'x', 'ok')",
            [],
        )
        .unwrap();
        assert!(conn.execute("UPDATE audit SET action = 'y'", []).is_err());
        assert!(conn.execute("DELETE FROM audit", []).is_err());
    }

    #[test]
    fn live_ports_are_unique() {
        let (_d, db, backups) = tmp();
        let conn = open(&db, &backups).unwrap();
        let insert = |id: &str, root: &str, deleted: Option<i64>| {
            conn.execute(
                "INSERT INTO instances (id, name, software, mc_version, install_root, port, memory_json, state, deleted_at, created_at, updated_at)
                 VALUES (?1, 'n', 'vanilla', '26.3', ?2, 25565, '{}', 'stopped', ?3, 0, 0)",
                rusqlite::params![id, root, deleted],
            )
        };
        insert("a", "/a", None).unwrap();
        assert!(insert("b", "/b", None).is_err());
        insert("c", "/c", Some(1)).unwrap();
    }
}
