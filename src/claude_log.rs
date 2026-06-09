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
    Some(
        PathBuf::from(home)
            .join(".claude")
            .join("projects")
            .join(encode_cwd(cwd)),
    )
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
    input: u64,
    cache_write: u64,
    cache_read: u64,
    output: u64,
    model: String,
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
        input: get("input_tokens"),
        cache_write: get("cache_creation_input_tokens"),
        cache_read: get("cache_read_input_tokens"),
        output: get("output_tokens"),
        model,
        service_tier: usage
            .get("service_tier")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
    };
    if raw.input == 0 && raw.cache_write == 0 && raw.cache_read == 0 && raw.output == 0 {
        return None;
    }
    Some(raw)
}

fn compute_cost(raw: &RawUsage, config: &Config) -> f64 {
    let rate = config.pricing.claude_rate(&raw.model);
    (raw.input as f64 / 1_000_000.0) * rate.input
        + (raw.cache_write as f64 / 1_000_000.0) * rate.cache_write
        + (raw.cache_read as f64 / 1_000_000.0) * rate.cache_read
        + (raw.output as f64 / 1_000_000.0) * rate.output
}

use crate::session::SessionState;

/// Infer session state from a JSONL record.
fn parse_state(v: &serde_json::Value) -> Option<SessionState> {
    match v["type"].as_str()? {
        "user" => Some(SessionState::Thinking),
        "tool" => Some(SessionState::Running),
        "assistant" => match v["message"]["stop_reason"].as_str().unwrap_or("") {
            "tool_use" => Some(SessionState::Running),
            "end_turn" => Some(SessionState::Ready),
            _ => None,
        },
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
    let mut acc_input: u64 = 0;
    let mut acc_cache_write: u64 = 0;
    let mut acc_cache_read: u64 = 0;
    let mut acc_output: u64 = 0;
    let mut acc_cost: f64 = 0.0;
    let mut context_tokens: u64 = 0;
    let mut billing_detected: bool = false;
    let mut offset: u64 = 0;

    loop {
        if tx.is_closed() {
            break;
        }

        let mut new_stats = false;
        if let Ok(meta) = tokio::fs::metadata(path).await {
            if meta.len() == offset {
                sleep(Duration::from_millis(200)).await;
                continue;
            }
        }
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
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                offset += n as u64;
                                continue;
                            }
                            let v: serde_json::Value = match serde_json::from_str(trimmed) {
                                Ok(v) => v,
                                Err(_) => break,
                            };
                            offset += n as u64;
                            if let Some(state) = parse_state(&v) {
                                let _ = tx
                                    .send(AppEvent::IpcStateOverride { session_id, state })
                                    .await;
                            }
                            if let Some(raw) = parse_usage_from_value(&v) {
                                if !billing_detected {
                                    if let Some(ref tier) = raw.service_tier {
                                        billing_detected = true;
                                        let is_pro = tier != "standard";
                                        let _ = tx
                                            .send(AppEvent::SessionBillingKnown {
                                                session_id,
                                                is_pro,
                                            })
                                            .await;
                                    }
                                }
                                acc_cost += compute_cost(&raw, config);
                                acc_input += raw.input;
                                acc_cache_write += raw.cache_write;
                                acc_cache_read += raw.cache_read;
                                acc_output += raw.output;
                                context_tokens = raw.input + raw.cache_write + raw.cache_read;
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
                input_tokens: acc_input + acc_cache_write + acc_cache_read,
                output_tokens: acc_output,
                context_tokens,
                total_cost_usd: acc_cost,
            };
            if tx
                .send(AppEvent::SessionStats { session_id, stats })
                .await
                .is_err()
            {
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
    let existing = if dir.exists() {
        jsonl_files(&dir)
    } else {
        HashSet::new()
    };

    tokio::spawn(async move {
        // Wait for the project dir to exist (first session in this cwd).
        if !dir.exists() {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            loop {
                if dir.exists() {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_cwd_matches_claude_project_directory_convention() {
        assert_eq!(encode_cwd("/tmp/linkshell"), "-tmp-linkshell");
        assert_eq!(encode_cwd("relative/path"), "relative-path");
    }

    #[test]
    fn parse_usage_reads_assistant_usage_and_service_tier() {
        let v = serde_json::json!({
            "type": "assistant",
            "message": {
                "model": "claude-sonnet-4-5",
                "usage": {
                    "input_tokens": 10,
                    "cache_creation_input_tokens": 20,
                    "cache_read_input_tokens": 30,
                    "output_tokens": 40,
                    "service_tier": "standard"
                }
            }
        });

        let raw = parse_usage_from_value(&v).unwrap();

        assert_eq!(raw.input, 10);
        assert_eq!(raw.cache_write, 20);
        assert_eq!(raw.cache_read, 30);
        assert_eq!(raw.output, 40);
        assert_eq!(raw.model, "claude-sonnet-4-5");
        assert_eq!(raw.service_tier.as_deref(), Some("standard"));
    }

    #[test]
    fn parse_usage_ignores_non_assistant_or_zero_usage_records() {
        assert!(parse_usage_from_value(&serde_json::json!({"type": "user"})).is_none());
        assert!(parse_usage_from_value(&serde_json::json!({
            "type": "assistant",
            "message": {"usage": {"input_tokens": 0, "output_tokens": 0}}
        }))
        .is_none());
    }

    #[test]
    fn compute_cost_includes_cache_write_and_cache_read_rates() {
        let raw = RawUsage {
            input: 1_000_000,
            cache_write: 1_000_000,
            cache_read: 1_000_000,
            output: 1_000_000,
            model: "claude-haiku-test".into(),
            service_tier: None,
        };

        let cost = compute_cost(&raw, &Config::default());

        assert!((cost - 5.88).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_state_maps_jsonl_record_types_to_session_states() {
        assert_eq!(
            parse_state(&serde_json::json!({"type": "user"})),
            Some(SessionState::Thinking)
        );
        assert_eq!(
            parse_state(&serde_json::json!({"type": "tool"})),
            Some(SessionState::Running)
        );
        assert_eq!(
            parse_state(&serde_json::json!({
                "type": "assistant",
                "message": {"stop_reason": "tool_use"}
            })),
            Some(SessionState::Running)
        );
        assert_eq!(
            parse_state(&serde_json::json!({
                "type": "assistant",
                "message": {"stop_reason": "end_turn"}
            })),
            Some(SessionState::Ready)
        );
        assert_eq!(
            parse_state(&serde_json::json!({
                "type": "assistant",
                "message": {"stop_reason": "max_tokens"}
            })),
            None
        );
    }
}
