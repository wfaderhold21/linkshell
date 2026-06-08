/// Watch ~/.claude/projects/<encoded-cwd>/ for the JSONL session file that
/// Claude CLI writes, tail it, and emit authoritative cumulative token stats.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::time::{sleep, Duration};

use crate::config::Config;
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
    input:        u64,
    cache_write:  u64,
    cache_read:   u64,
    output:       u64,
    model:        String,
    service_tier: Option<String>,
}

fn parse_usage_from_value(v: &serde_json::Value) -> Option<RawUsage> {
    if v["type"].as_str()? != "assistant" {
        return None;
    }
    let model = v["message"]["model"]
        .as_str()
        .unwrap_or("claude-sonnet")
        .to_string();
    let usage = v["message"]["usage"].as_object()?;
    let get = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
    let raw = RawUsage {
        input:        get("input_tokens"),
        cache_write:  get("cache_creation_input_tokens"),
        cache_read:   get("cache_read_input_tokens"),
        output:       get("output_tokens"),
        model,
        service_tier: usage.get("service_tier").and_then(|v| v.as_str()).map(str::to_owned),
    };
    if raw.input == 0 && raw.cache_write == 0 && raw.cache_read == 0 && raw.output == 0 {
        return None;
    }
    Some(raw)
}

fn compute_cost(raw: &RawUsage, config: &Config) -> f64 {
    let rate = config.pricing.claude_rate(&raw.model);
    (raw.input       as f64 / 1_000_000.0) * rate.input
        + (raw.cache_write as f64 / 1_000_000.0) * rate.cache_write
        + (raw.cache_read  as f64 / 1_000_000.0) * rate.cache_read
        + (raw.output      as f64 / 1_000_000.0) * rate.output
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
    config: &Config,
) {
    let mut acc_input:        u64  = 0;
    let mut acc_cache_write:  u64  = 0;
    let mut acc_cache_read:   u64  = 0;
    let mut acc_output:       u64  = 0;
    let mut acc_cost:         f64  = 0.0;
    let mut context_tokens:   u64  = 0;
    let mut billing_detected: bool = false;
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
                            // If the line has no trailing newline we hit EOF mid-write.
                            // Don't advance offset — retry the partial bytes next poll.
                            if !line.ends_with('\n') {
                                break;
                            }
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
                                if !billing_detected {
                                    if let Some(ref tier) = raw.service_tier {
                                        billing_detected = true;
                                        let is_pro = tier != "standard";
                                        let _ = tx.send(AppEvent::SessionBillingKnown {
                                            session_id,
                                            is_pro,
                                        }).await;
                                    }
                                }
                                acc_cost        += compute_cost(&raw, config);
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
                total_cost_usd: acc_cost,
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
    config: Arc<Config>,
) {
    let dir = match project_dir(&cwd) {
        Some(d) => d,
        None => return,
    };

    // Snapshot existing JSONL files SYNCHRONOUSLY here, before the PTY runner is
    // even spawned. This closes the race where Claude creates its new session file
    // before the async watcher task gets to run on a multi-threaded executor.
    let existing = if dir.exists() { jsonl_files(&dir) } else { HashSet::new() };

    tokio::spawn(async move {
        // Wait for the project dir to exist (first session in this cwd).
        if !dir.exists() {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                if dir.exists() { break; }
                if tokio::time::Instant::now() >= deadline { return; }
                sleep(Duration::from_millis(200)).await;
            }
        }

        let jsonl = match wait_for_new_jsonl(&dir, &existing, Duration::from_secs(30)).await {
            Some(p) => p,
            None => return,
        };

        tail(session_id, &jsonl, &tx, &config).await;
    });
}
