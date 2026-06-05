/// Watch ~/.claude/projects/<encoded-cwd>/ for the JSONL session file that
/// Claude CLI writes, tail it, and emit authoritative cumulative token stats.
use std::path::{Path, PathBuf};
use std::collections::HashSet;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::time::{sleep, Duration};

use crate::events::AppEvent;
use crate::session::TokenStats;

/// Encode a cwd path the same way Claude CLI does:
/// replace every '/' with '-' (leading slash becomes leading '-').
fn encode_cwd(cwd: &str) -> String {
    cwd.replace('/', "-")
}

fn project_dir(cwd: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(encode_cwd(cwd)))
}

fn jsonl_files(dir: &Path) -> HashSet<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect()
}

/// Wait up to `timeout` for a JSONL file to appear in `dir` that wasn't in
/// `existing`. Returns the path of the first new file found.
async fn wait_for_new_jsonl(
    dir: &Path,
    existing: &HashSet<PathBuf>,
    timeout: Duration,
) -> Option<PathBuf> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        for path in jsonl_files(dir) {
            if !existing.contains(&path) {
                return Some(path);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(200)).await;
    }
}

struct RawUsage {
    input:       u64,
    cache_write: u64,
    cache_read:  u64,
    output:      u64,
}

fn parse_usage_from_value(v: &serde_json::Value) -> Option<RawUsage> {
    if v["type"].as_str()? != "assistant" {
        return None;
    }
    let usage = v["message"]["usage"].as_object()?;
    let get = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let raw = RawUsage {
        input:       get("input_tokens"),
        cache_write: get("cache_creation_input_tokens"),
        cache_read:  get("cache_read_input_tokens"),
        output:      get("output_tokens"),
    };
    if raw.input == 0 && raw.cache_write == 0 && raw.cache_read == 0 && raw.output == 0 {
        return None;
    }
    Some(raw)
}

fn compute_cost(input: u64, cache_write: u64, cache_read: u64, output: u64) -> f64 {
    // Sonnet 4.x pricing
    (input       as f64 / 1_000_000.0) *  3.00
        + (cache_write as f64 / 1_000_000.0) *  3.75
        + (cache_read  as f64 / 1_000_000.0) *  0.30
        + (output      as f64 / 1_000_000.0) * 15.00
}

use crate::session::SessionState;

/// Infer session state from a JSONL record.
fn parse_state(v: &serde_json::Value) -> Option<SessionState> {
    match v["type"].as_str()? {
        "user" => Some(SessionState::Thinking),
        "tool" => Some(SessionState::Running),
        "assistant" => {
            match v["message"]["stop_reason"].as_str().unwrap_or("") {
                "tool_use" => Some(SessionState::Running),
                "end_turn" => Some(SessionState::Ready),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Tail a JSONL file from `offset`, emitting token stats and state transitions.
async fn tail(
    session_id: usize,
    path: &Path,
    tx: &tokio::sync::mpsc::Sender<AppEvent>,
) {
    let mut acc_input:       u64 = 0;
    let mut acc_cache_write: u64 = 0;
    let mut acc_cache_read:  u64 = 0;
    let mut acc_output:      u64 = 0;
    let mut context_tokens:  u64 = 0;
    let mut offset: u64 = 0;

    loop {
        if tx.is_closed() {
            break;
        }

        let mut new_stats = false;
        if let Ok(file) = tokio::fs::File::open(path).await {
            let mut file = file;
            if file.seek(std::io::SeekFrom::Start(offset)).await.is_ok() {
                let mut reader = BufReader::new(file);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(n) => {
                            offset += n as u64;
                            let v: serde_json::Value = match serde_json::from_str(line.trim()) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            if let Some(state) = parse_state(&v) {
                                let _ = tx.send(AppEvent::IpcStateOverride {
                                    session_id,
                                    state,
                                }).await;
                            }
                            if let Some(raw) = parse_usage_from_value(&v) {
                                acc_input       += raw.input;
                                acc_cache_write += raw.cache_write;
                                acc_cache_read  += raw.cache_read;
                                acc_output      += raw.output;
                                context_tokens   = raw.input + raw.cache_write + raw.cache_read;
                                new_stats = true;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        if new_stats {
            let stats = TokenStats {
                input_tokens:  acc_input + acc_cache_write + acc_cache_read,
                output_tokens: acc_output,
                context_tokens,
                total_cost_usd: compute_cost(acc_input, acc_cache_write, acc_cache_read, acc_output),
            };
            if tx.send(AppEvent::SessionStats { session_id, stats }).await.is_err() {
                break;
            }
        }

        sleep(Duration::from_millis(500)).await;
    }
}

/// Spawn a background task that watches the Claude CLI project JSONL for this
/// session and emits `SessionStats` events with the true cumulative totals.
pub fn spawn_watcher(
    session_id: usize,
    cwd: String,
    tx: tokio::sync::mpsc::Sender<AppEvent>,
) {
    tokio::spawn(async move {
        let dir = match project_dir(&cwd) {
            Some(d) => d,
            None => return,
        };

        // Wait for the project dir to exist (may be first session in this cwd)
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if dir.exists() { break; }
            if tokio::time::Instant::now() >= deadline { return; }
            sleep(Duration::from_millis(200)).await;
        }

        let existing = jsonl_files(&dir);

        let jsonl = match wait_for_new_jsonl(&dir, &existing, Duration::from_secs(30)).await {
            Some(p) => p,
            None => return,
        };

        tail(session_id, &jsonl, &tx).await;
    });
}
