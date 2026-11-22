//! Small repositories used across the daemon: audit trail and settings.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::db::now_ms;

/// Outcome recorded in the audit trail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOutcome {
    /// The action completed successfully.
    Ok,
    /// The action failed.
    Error,
    /// A long-running action started (a later row records its end).
    Started,
}

impl AuditOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Started => "started",
        }
    }
}

/// One audit-trail record. `params` must already be redacted (no secrets).
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntry<'a> {
    /// `local-owner`, `device:<id>` or `system`.
    pub principal: &'a str,
    /// Action name, e.g. `instances.accept_eula`.
    pub action: &'a str,
    /// Target resource, e.g. `instance:<id>`.
    pub target: Option<&'a str>,
    /// Redacted parameters.
    pub params: Option<&'a Value>,
    /// Result.
    pub outcome: AuditOutcome,
    /// Stable error code when `outcome` is `Error`.
    pub error_code: Option<msm_core::ErrorCode>,
}

/// Appends an audit row and returns its ID.
///
/// # Errors
/// SQLite errors.
pub fn append_audit(conn: &Connection, e: &AuditEntry<'_>) -> rusqlite::Result<String> {
    let id = ulid_like();
    conn.execute(
        "INSERT INTO audit (id, at, principal, action, target_ref, params_redacted_json, outcome, error_code)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            now_ms(),
            e.principal,
            e.action,
            e.target,
            e.params.map(Value::to_string),
            e.outcome.as_str(),
            e.error_code.map(msm_core::ErrorCode::as_str),
        ],
    )?;
    Ok(id)
}

fn ulid_like() -> String {
    msm_core::ids::RequestId::new().to_string()
}

/// Reads a setting.
///
/// # Errors
/// SQLite errors or invalid stored JSON.
pub fn get_setting(conn: &Connection, key: &str) -> rusqlite::Result<Option<Value>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value_json FROM settings WHERE key = ?1",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|s| {
        serde_json::from_str(&s).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
    })
    .transpose()
}

/// Writes a setting.
///
/// # Errors
/// SQLite errors.
pub fn set_setting(conn: &Connection, key: &str, value: &Value) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value_json) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json",
        params![key, value.to_string()],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("s.db"), &dir.path().join("b")).unwrap();
        (dir, conn)
    }

    #[test]
    fn settings_round_trip() {
        let (_d, conn) = db();
        assert_eq!(get_setting(&conn, "x").unwrap(), None);
        set_setting(&conn, "x", &json!({"a": 1})).unwrap();
        set_setting(&conn, "x", &json!(true)).unwrap();
        assert_eq!(get_setting(&conn, "x").unwrap(), Some(json!(true)));
    }

    #[test]
    fn audit_rows_are_appended_in_order() {
        let (_d, conn) = db();
        let params = json!({"instance": "abc"});
        append_audit(
            &conn,
            &AuditEntry {
                principal: "local-owner",
                action: "instances.accept_eula",
                target: Some("instance:abc"),
                params: Some(&params),
                outcome: AuditOutcome::Ok,
                error_code: None,
            },
        )
        .unwrap();
        append_audit(
            &conn,
            &AuditEntry {
                principal: "device:xyz",
                action: "players.op",
                target: None,
                params: None,
                outcome: AuditOutcome::Error,
                error_code: Some(msm_core::ErrorCode::Forbidden),
            },
        )
        .unwrap();
        let rows: Vec<(String, String, Option<String>)> = conn
            .prepare("SELECT action, outcome, error_code FROM audit ORDER BY seq")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            vec![
                ("instances.accept_eula".into(), "ok".into(), None),
                (
                    "players.op".into(),
                    "error".into(),
                    Some("FORBIDDEN".into())
                ),
            ]
        );
    }
}
