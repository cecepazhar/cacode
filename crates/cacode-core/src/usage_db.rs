//! `~/.cacode/usage.db` schema + writer shared by the agent, the monitor proxy
//! and reports.

use crate::types::TokenUsage;
use rusqlite::Connection;
use std::path::PathBuf;

pub fn db_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cacode").join("usage.db"))
}

/// Create / migrate the schema. Idempotent.
pub fn init(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS token_usage (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            model TEXT NOT NULL,
            prompt_tokens INTEGER NOT NULL,
            completion_tokens INTEGER NOT NULL,
            total_tokens INTEGER NOT NULL,
            est_cost REAL NOT NULL
        )",
        [],
    )?;
    let cols: Vec<String> = conn
        .prepare("PRAGMA table_info(token_usage)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(|c| c.ok())
        .collect();
    for (name, ddl) in [
        ("provider", "ALTER TABLE token_usage ADD COLUMN provider TEXT NOT NULL DEFAULT ''"),
        ("source", "ALTER TABLE token_usage ADD COLUMN source TEXT NOT NULL DEFAULT 'agent'"),
        ("estimated", "ALTER TABLE token_usage ADD COLUMN estimated INTEGER NOT NULL DEFAULT 0"),
        ("cost_known", "ALTER TABLE token_usage ADD COLUMN cost_known INTEGER NOT NULL DEFAULT 1"),
    ] {
        if !cols.iter().any(|c| c == name) {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

pub fn open() -> Option<Connection> {
    let p = db_path()?;
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(p).ok()?;
    init(&conn).ok()?;
    Some(conn)
}

pub fn record(conn: &Connection, source: &str, provider: &str, model: &str, u: &TokenUsage) -> rusqlite::Result<()> {
    let cost = crate::pricing::cost_usage_usd(provider, model, u);
    conn.execute(
        "INSERT INTO token_usage (timestamp, model, prompt_tokens, completion_tokens, total_tokens, est_cost, provider, source, estimated, cost_known)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            chrono::Utc::now().to_rfc3339(),
            model,
            u.prompt_tokens,
            u.completion_tokens,
            u.total_tokens,
            cost.unwrap_or(0.0),
            provider,
            source,
            u.estimated as i64,
            cost.is_some() as i64
        ],
    )?;
    Ok(())
}

/// Best-effort write from the agent (never fails the turn).
pub fn record_agent_usage(provider: &str, model: &str, u: &TokenUsage) {
    if u.total_tokens == 0 || std::env::var("CACODE_NO_USAGE_DB").is_ok() {
        return;
    }
    if let Some(conn) = open() {
        let _ = record(&conn, "agent", provider, model, u);
    }
}
