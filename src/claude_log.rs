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

pub fn projects_dir(config_home: Option<&str>) -> Option<PathBuf> {
    let config_base = match config_home {
        Some(dir) => PathBuf::from(dir),
        None => std::env::var("CLAUDE_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".claude")))
            .ok()?,
    };
    Some(config_base.join("projects"))
}

fn project_dir(cwd: &str, config_home: Option<&str>) -> Option<PathBuf> {
    // Precedence: per-session override (inline env prefix or config alias) →
    // $CLAUDE_CONFIG_DIR in linkshell's own environment → $HOME/.claude.
    Some(projects_dir(config_home)?.join(encode_cwd(cwd)))
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

/// Process-wide registry of JSONL files already claimed by a watcher.
/// Multiple claude sessions in the same cwd (orchestrator + workers, or a
/// detecting watcher racing a native one) each wait for "a new file in this
/// dir" — without claiming, two watchers can attach to the same file and
/// report the same usage into two sessions, inflating the totals.
pub(crate) fn claim_jsonl(path: &Path) -> bool {
    static CLAIMED: std::sync::OnceLock<std::sync::Mutex<HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    CLAIMED
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(path.to_path_buf())
}

/// Wait for a JSONL file to appear in `dir` that wasn't in `existing` and
/// isn't already claimed by another session's watcher.
/// Claude only creates the session file when the user submits their first
/// prompt, which can be arbitrarily long after the session spawns — so there
/// is no deadline here; poll until the file appears or the app shuts down.
async fn wait_for_new_jsonl(
    dir: &Path,
    existing: &HashSet<PathBuf>,
    tx: &tokio::sync::mpsc::Sender<AppEvent>,
) -> Option<PathBuf> {
    loop {
        for path in jsonl_files(dir) {
            if !existing.contains(&path) && claim_jsonl(&path) {
                return Some(path);
            }
        }
        if tx.is_closed() {
            return None;
        }
        sleep(Duration::from_millis(500)).await;
    }
}

struct RawUsage {
    input: u64,
    cache_write: u64,
    /// Portion of `cache_write` written with the 1-hour TTL (billed at 2x
    /// input instead of 1.25x). Claude Code uses 1h cache by default, so this
    /// is usually all of `cache_write` when the breakdown is present.
    cache_write_1h: u64,
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
    let cache_write = get("cache_creation_input_tokens");
    let cache_write_1h = usage
        .get("cache_creation")
        .and_then(|c| c.get("ephemeral_1h_input_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(cache_write);
    let raw = RawUsage {
        input: get("input_tokens"),
        cache_write,
        cache_write_1h,
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

/// Dedup key for a usage record. Claude CLI emits one JSONL line per content
/// block of the same API response, and every line repeats the full usage
/// object — summing per line multiplies real usage. Count each response once,
/// keyed on message id + request id (same scheme `/cost` uses). Records
/// without a message id (synthetic entries) get no key and are always counted.
fn usage_key(v: &serde_json::Value) -> Option<String> {
    let msg_id = v["message"]["id"].as_str()?;
    let req_id = v["requestId"].as_str().unwrap_or("");
    Some(format!("{}:{}", msg_id, req_id))
}

fn compute_cost(raw: &RawUsage, config: &Config) -> f64 {
    let rate = config.pricing.claude_rate(&raw.model);
    // Configured cache_write is the 5-minute-TTL rate (1.25x input). 1-hour
    // cache writes bill at 2x input = 1.6x the 5m rate; Claude Code uses the
    // 1h TTL by default, so without this the cost undercounts vs /cost.
    let cw_5m = raw.cache_write - raw.cache_write_1h;
    (raw.input as f64 / 1_000_000.0) * rate.input
        + (cw_5m as f64 / 1_000_000.0) * rate.cache_write
        + (raw.cache_write_1h as f64 / 1_000_000.0) * rate.cache_write * 1.6
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
        "system" => match v["subtype"].as_str().unwrap_or("") {
            "api_error" => Some(SessionState::Error),
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
    let mut acc_output: u64 = 0;
    let mut acc_cost: f64 = 0.0;
    let mut context_tokens: u64 = 0;
    let mut billing_detected: bool = false;
    let mut last_model: Option<String> = None;
    let mut offset: u64 = 0;
    let mut seen_usage: HashSet<String> = HashSet::new();

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
                                if usage_key(&v).is_some_and(|k| !seen_usage.insert(k)) {
                                    continue;
                                }
                                if last_model.as_deref() != Some(&raw.model) {
                                    last_model = Some(raw.model.clone());
                                    let _ = tx
                                        .send(AppEvent::SessionModel {
                                            session_id,
                                            model: raw.model.clone(),
                                        })
                                        .await;
                                }
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
            // input_tokens matches `/cost` semantics: non-cache input only.
            // Cache reads/writes still price into acc_cost and show up in
            // context_tokens (the real context footprint).
            let stats = TokenStats {
                input_tokens: acc_input,
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
    config_home: Option<String>,
) {
    spawn_watcher_inner(session_id, cwd, tx, config, config_home, false);
}

/// Like [`spawn_watcher`], but for sessions whose command didn't resolve to a
/// known CLI (wrapper scripts, arbitrary custom commands). If a new claude
/// JSONL ever appears for this cwd, the session is evidently claude behind a
/// name the classifier can't see through: announce the identity upgrade via
/// `SessionBaseDetected`, then tail the log like a native claude session.
pub fn spawn_detecting_watcher(
    session_id: usize,
    cwd: String,
    tx: tokio::sync::mpsc::Sender<AppEvent>,
    config: Arc<Config>,
    config_home: Option<String>,
) {
    spawn_watcher_inner(session_id, cwd, tx, config, config_home, true);
}

fn spawn_watcher_inner(
    session_id: usize,
    cwd: String,
    tx: tokio::sync::mpsc::Sender<AppEvent>,
    config: Arc<Config>,
    config_home: Option<String>,
    announce_detection: bool,
) {
    let dir = match project_dir(&cwd, config_home.as_deref()) {
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
        while !dir.exists() {
            if tx.is_closed() {
                return;
            }
            sleep(Duration::from_millis(500)).await;
        }

        let jsonl = match wait_for_new_jsonl(&dir, &existing, &tx).await {
            Some(p) => p,
            None => return,
        };

        if announce_detection {
            let _ = tx
                .send(AppEvent::SessionBaseDetected {
                    session_id,
                    base: crate::session::BaseKind::Claude,
                })
                .await;
        }

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
    fn usage_key_dedupes_content_block_lines_of_one_response() {
        let line = |block: &str| {
            serde_json::json!({
                "type": "assistant",
                "requestId": "req_1",
                "message": {
                    "id": "msg_abc",
                    "usage": {"input_tokens": 10, "output_tokens": 40},
                    "content": [{"type": block}]
                }
            })
        };
        let mut seen = HashSet::new();
        assert!(seen.insert(usage_key(&line("text")).unwrap()));
        // Second JSONL line for the same API response: same key, must not count.
        assert!(!seen.insert(usage_key(&line("tool_use")).unwrap()));
        // A different response counts again.
        let other = serde_json::json!({
            "type": "assistant",
            "requestId": "req_2",
            "message": {"id": "msg_def", "usage": {"input_tokens": 5}}
        });
        assert!(seen.insert(usage_key(&other).unwrap()));
        // No message id → no key → always counted.
        assert!(
            usage_key(&serde_json::json!({"type": "assistant", "message": {"usage": {}}}))
                .is_none()
        );
    }

    #[test]
    fn compute_cost_includes_cache_write_and_cache_read_rates() {
        let raw = RawUsage {
            input: 1_000_000,
            cache_write: 1_000_000,
            cache_write_1h: 0,
            cache_read: 1_000_000,
            output: 1_000_000,
            model: "claude-haiku-test".into(),
            service_tier: None,
        };

        let cost = compute_cost(&raw, &Config::default());

        assert!((cost - 5.88).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_cost_matches_claude_code_for_fable_with_1h_cache() {
        // Real numbers from a `claude -p --output-format json` run: Claude Code
        // reported costUSD 0.306436 for this usage (1h cache writes at 2x).
        let raw = RawUsage {
            input: 2291,
            cache_write: 13377,
            cache_write_1h: 13377,
            cache_read: 10986,
            output: 100,
            model: "claude-fable-5".into(),
            service_tier: None,
        };

        let cost = compute_cost(&raw, &Config::default());

        assert!((cost - 0.306436).abs() < 1e-9, "cost = {}", cost);
    }

    #[test]
    fn parse_usage_reads_1h_cache_breakdown() {
        let v = serde_json::json!({
            "type": "assistant",
            "message": {
                "model": "claude-fable-5",
                "usage": {
                    "input_tokens": 5,
                    "cache_creation_input_tokens": 100,
                    "output_tokens": 1,
                    "cache_creation": {
                        "ephemeral_1h_input_tokens": 70,
                        "ephemeral_5m_input_tokens": 30
                    }
                }
            }
        });
        let raw = parse_usage_from_value(&v).unwrap();
        assert_eq!(raw.cache_write, 100);
        assert_eq!(raw.cache_write_1h, 70);
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
        assert_eq!(
            parse_state(&serde_json::json!({"type": "system", "subtype": "api_error"})),
            Some(SessionState::Error)
        );
        assert_eq!(
            parse_state(&serde_json::json!({"type": "system", "subtype": "turn_duration"})),
            None
        );
    }

    #[test]
    fn claim_jsonl_admits_each_file_exactly_once() {
        let p = PathBuf::from("/tmp/linkshell-test-claim-unique-xyz.jsonl");
        assert!(claim_jsonl(&p));
        assert!(!claim_jsonl(&p));
    }

    #[test]
    fn encode_cwd_matches_claude_cli_scheme() {
        assert_eq!(encode_cwd("/home/u/proj"), "-home-u-proj");
    }

    #[test]
    fn project_dir_prefers_per_session_config_home() {
        let dir = project_dir("/home/u/proj", Some("/opt/claude-work")).unwrap();
        assert_eq!(dir, PathBuf::from("/opt/claude-work/projects/-home-u-proj"));
    }
}
