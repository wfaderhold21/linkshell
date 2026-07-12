/// Watch the OpenCode SQLite database (~/.local/share/opencode/opencode.db)
/// and emit cumulative token/cost stats for an OpenCode session running in a
/// given cwd. OpenCode persists per-session totals (tokens_*, cost) on the
/// `session` table and per-message usage JSON on the `message` table; the
/// latest assistant message's input-side tokens reflect the current context
/// window size.
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

use crate::events::AppEvent;
use crate::session::TokenStats;

pub fn db_path() -> Option<PathBuf> {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))
        .ok()?;
    Some(base.join("opencode").join("opencode.db"))
}

fn open_db(path: &Path) -> Option<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = conn.busy_timeout(Duration::from_millis(250));
    Some(conn)
}

/// Most recently active top-level OpenCode session in `dir` touched since
/// `since_ms` (epoch millis). Subagent sessions carry a parent_id and are
/// excluded — the TUI's status line reflects the parent session.
fn find_session(conn: &Connection, dir: &str, since_ms: i64) -> Option<String> {
    conn.query_row(
        "SELECT id FROM session \
         WHERE directory = ?1 AND parent_id IS NULL AND time_updated >= ?2 \
         ORDER BY time_updated DESC LIMIT 1",
        rusqlite::params![dir, since_ms],
        |row| row.get(0),
    )
    .ok()
}

/// The session.model column holds JSON like
/// `{"id":"qwen3.6-28b","providerID":"lmstudio"}`; older rows may be a bare id.
fn parse_model_column(raw: String) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(v) => v["id"].as_str().map(str::to_owned),
        Err(_) if !raw.is_empty() => Some(raw),
        Err(_) => None,
    }
}

struct SessionRow {
    input: u64,
    output: u64,
    reasoning: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
    model: Option<String>,
}

fn read_session_row(conn: &Connection, session_id: &str) -> Option<SessionRow> {
    conn.query_row(
        "SELECT tokens_input, tokens_output, tokens_reasoning, \
                tokens_cache_read, tokens_cache_write, cost, model \
         FROM session WHERE id = ?1",
        rusqlite::params![session_id],
        |row| {
            Ok(SessionRow {
                input: row.get::<_, i64>(0)?.max(0) as u64,
                output: row.get::<_, i64>(1)?.max(0) as u64,
                reasoning: row.get::<_, i64>(2)?.max(0) as u64,
                cache_read: row.get::<_, i64>(3)?.max(0) as u64,
                cache_write: row.get::<_, i64>(4)?.max(0) as u64,
                cost: row.get(5)?,
                model: row
                    .get::<_, Option<String>>(6)?
                    .and_then(parse_model_column),
            })
        },
    )
    .ok()
}

/// Context size and model from the newest assistant message: input-side
/// tokens of the latest API call (input + cache read + cache write), matching
/// the claude/codex watchers' definition of `context_tokens`.
fn read_latest_context(conn: &Connection, session_id: &str) -> Option<(u64, Option<String>)> {
    let mut stmt = conn
        .prepare(
            "SELECT data FROM message WHERE session_id = ?1 \
             ORDER BY time_created DESC, id DESC LIMIT 20",
        )
        .ok()?;
    let rows = stmt
        .query_map(rusqlite::params![session_id], |row| row.get::<_, String>(0))
        .ok()?;
    for data in rows.flatten() {
        let v: serde_json::Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["role"].as_str() != Some("assistant") {
            continue;
        }
        let tokens = &v["tokens"];
        let input = tokens["input"].as_u64().unwrap_or(0);
        let cache_read = tokens["cache"]["read"].as_u64().unwrap_or(0);
        let cache_write = tokens["cache"]["write"].as_u64().unwrap_or(0);
        if input + cache_read + cache_write == 0 {
            // In-flight message whose usage hasn't been recorded yet; keep
            // looking at the previous completed one.
            continue;
        }
        let model = v["modelID"].as_str().map(str::to_owned);
        return Some((input + cache_read + cache_write, model));
    }
    None
}

fn read_stats(conn: &Connection, session_id: &str) -> Option<(TokenStats, Option<String>)> {
    let row = read_session_row(conn, session_id)?;
    let (context_tokens, msg_model) = read_latest_context(conn, session_id).unwrap_or((0, None));

    if row.input == 0 && row.output == 0 && context_tokens == 0 {
        return None;
    }

    // Same semantics as claude_log: input includes cache traffic, output
    // includes reasoning. OpenCode computes cost itself (0 for local models),
    // so the db value is authoritative.
    let stats = TokenStats {
        input_tokens: row.input + row.cache_read + row.cache_write,
        output_tokens: row.output + row.reasoning,
        context_tokens,
        total_cost_usd: row.cost,
    };
    Some((stats, msg_model.or(row.model)))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Normalize the linkshell-side cwd for comparison with OpenCode's
/// `session.directory` column (an absolute path without a trailing slash).
fn normalize_cwd(cwd: &str) -> String {
    let canon = std::fs::canonicalize(cwd)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| cwd.to_string());
    if canon.len() > 1 {
        canon.trim_end_matches('/').to_string()
    } else {
        canon
    }
}

/// Spawn a watcher that binds to the next OpenCode session active in `cwd`.
/// rusqlite is blocking, so this runs on a dedicated thread and polls; each
/// query touches a few indexed rows and the connection is read-only.
pub fn spawn_watcher(session_id: usize, cwd: String, tx: tokio::sync::mpsc::Sender<AppEvent>) {
    let Some(path) = db_path() else { return };
    // Sessions resumed with `opencode -c` have an old time_created, but any
    // activity bumps time_updated — so bind on time_updated since launch.
    // Small slack absorbs writes that land while the CLI is still starting.
    let since_ms = now_ms() - 3_000;

    std::thread::spawn(move || {
        let dir = normalize_cwd(&cwd);
        let mut conn: Option<Connection> = None;
        let mut bound: Option<String> = None;
        let mut last_stats: Option<TokenStats> = None;
        let mut last_model: Option<String> = None;

        while !tx.is_closed() {
            std::thread::sleep(Duration::from_millis(1000));

            if conn.is_none() {
                if !path.exists() {
                    continue;
                }
                conn = open_db(&path);
            }
            let Some(c) = conn.as_ref() else { continue };

            // Re-query the binding each poll: the user can switch sessions
            // inside the OpenCode TUI, and the newest active one wins.
            if let Some(id) = find_session(c, &dir, since_ms) {
                bound = Some(id);
            }
            let Some(id) = bound.as_ref() else { continue };

            let Some((stats, model)) = read_stats(c, id) else {
                continue;
            };

            if let Some(model) = model {
                if last_model.as_deref() != Some(model.as_str()) {
                    if tx
                        .blocking_send(AppEvent::SessionModel {
                            session_id,
                            model: model.clone(),
                        })
                        .is_err()
                    {
                        return;
                    }
                    last_model = Some(model);
                }
            }

            if last_stats.as_ref() != Some(&stats) {
                if tx
                    .blocking_send(AppEvent::SessionStats {
                        session_id,
                        stats: stats.clone(),
                    })
                    .is_err()
                {
                    return;
                }
                last_stats = Some(stats);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY,
                directory TEXT NOT NULL,
                parent_id TEXT,
                time_updated INTEGER NOT NULL,
                model TEXT,
                cost REAL DEFAULT 0 NOT NULL,
                tokens_input INTEGER DEFAULT 0 NOT NULL,
                tokens_output INTEGER DEFAULT 0 NOT NULL,
                tokens_reasoning INTEGER DEFAULT 0 NOT NULL,
                tokens_cache_read INTEGER DEFAULT 0 NOT NULL,
                tokens_cache_write INTEGER DEFAULT 0 NOT NULL
            );
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                data TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn insert_session(conn: &Connection, id: &str, dir: &str, parent: Option<&str>, updated: i64) {
        conn.execute(
            "INSERT INTO session (id, directory, parent_id, time_updated, model, cost,
                tokens_input, tokens_output, tokens_reasoning,
                tokens_cache_read, tokens_cache_write)
             VALUES (?1, ?2, ?3, ?4, '{\"id\":\"qwen3.6\",\"providerID\":\"lmstudio\"}',
                     0.5, 1000, 200, 50, 300, 100)",
            rusqlite::params![id, dir, parent, updated],
        )
        .unwrap();
    }

    #[test]
    fn finds_newest_top_level_session_in_directory() {
        let conn = test_db();
        insert_session(&conn, "ses_old", "/home/u/proj", None, 100);
        insert_session(&conn, "ses_new", "/home/u/proj", None, 200);
        insert_session(&conn, "ses_sub", "/home/u/proj", Some("ses_new"), 300);
        insert_session(&conn, "ses_other", "/home/u/other", None, 400);

        assert_eq!(
            find_session(&conn, "/home/u/proj", 150).as_deref(),
            Some("ses_new")
        );
        // Nothing active since the launch timestamp → no binding.
        assert!(find_session(&conn, "/home/u/proj", 250).is_none());
    }

    #[test]
    fn reads_cumulative_stats_and_context_from_latest_assistant_message() {
        let conn = test_db();
        insert_session(&conn, "ses_1", "/home/u/proj", None, 100);
        let older = serde_json::json!({
            "role": "assistant",
            "modelID": "old-model",
            "tokens": {"input": 400, "output": 20, "cache": {"read": 0, "write": 0}}
        });
        let newest = serde_json::json!({
            "role": "assistant",
            "modelID": "qwen3.6",
            "tokens": {"input": 900, "output": 40, "cache": {"read": 60, "write": 10}}
        });
        let user = serde_json::json!({"role": "user"});
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES
             ('m1', 'ses_1', 1, ?1), ('m2', 'ses_1', 2, ?2), ('m3', 'ses_1', 3, ?3)",
            rusqlite::params![older.to_string(), newest.to_string(), user.to_string()],
        )
        .unwrap();

        let (stats, model) = read_stats(&conn, "ses_1").unwrap();
        assert_eq!(stats.input_tokens, 1000 + 300 + 100);
        assert_eq!(stats.output_tokens, 200 + 50);
        assert_eq!(stats.context_tokens, 900 + 60 + 10);
        assert_eq!(stats.total_cost_usd, 0.5);
        assert_eq!(model.as_deref(), Some("qwen3.6"));
    }

    #[test]
    fn skips_assistant_messages_without_recorded_usage() {
        let conn = test_db();
        insert_session(&conn, "ses_1", "/home/u/proj", None, 100);
        let done = serde_json::json!({
            "role": "assistant",
            "modelID": "qwen3.6",
            "tokens": {"input": 500, "output": 10, "cache": {"read": 0, "write": 0}}
        });
        let in_flight = serde_json::json!({
            "role": "assistant",
            "modelID": "qwen3.6",
            "tokens": {"input": 0, "output": 0, "cache": {"read": 0, "write": 0}}
        });
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES
             ('m1', 'ses_1', 1, ?1), ('m2', 'ses_1', 2, ?2)",
            rusqlite::params![done.to_string(), in_flight.to_string()],
        )
        .unwrap();

        let (ctx, _) = read_latest_context(&conn, "ses_1").unwrap();
        assert_eq!(ctx, 500);
    }

    #[test]
    fn read_stats_is_none_for_untouched_session() {
        let conn = test_db();
        conn.execute(
            "INSERT INTO session (id, directory, parent_id, time_updated)
             VALUES ('ses_0', '/p', NULL, 1)",
            [],
        )
        .unwrap();
        assert!(read_stats(&conn, "ses_0").is_none());
    }

    #[test]
    fn falls_back_to_session_model_when_no_assistant_message() {
        let conn = test_db();
        insert_session(&conn, "ses_1", "/home/u/proj", None, 100);
        let (stats, model) = read_stats(&conn, "ses_1").unwrap();
        assert_eq!(stats.context_tokens, 0);
        assert_eq!(model.as_deref(), Some("qwen3.6"));
    }

    #[test]
    fn parses_model_column_json_and_bare_forms() {
        assert_eq!(
            parse_model_column(r#"{"id":"qwen3.6","providerID":"lmstudio"}"#.into()).as_deref(),
            Some("qwen3.6")
        );
        assert_eq!(
            parse_model_column("plain-model-id".into()).as_deref(),
            Some("plain-model-id")
        );
        assert!(parse_model_column(String::new()).is_none());
    }
}
