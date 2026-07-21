/// Watch ~/.codex/sessions for the rollout JSONL created by Codex CLI and emit
/// authoritative cumulative token/context stats from token_count events.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::time::{sleep, Duration};

use crate::config::Config;
use crate::events::AppEvent;
use crate::session::TokenStats;

pub fn sessions_dir(codex_home: Option<&str>) -> Option<PathBuf> {
    // Precedence: per-session override (inline env prefix or config alias) →
    // $CODEX_HOME in linkshell's own environment → $HOME/.codex.
    let base = match codex_home {
        Some(dir) => PathBuf::from(dir),
        None => std::env::var("CODEX_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".codex")))
            .ok()?,
    };
    Some(base.join("sessions"))
}

fn jsonl_files(dir: &Path) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    collect_jsonl_files(dir, &mut out);
    out
}

fn collect_jsonl_files(dir: &Path, out: &mut HashSet<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            out.insert(path);
        }
    }
}

fn rollout_cwd(path: &Path) -> Option<String> {
    let data = std::fs::read_to_string(path).ok()?;
    for line in data.lines().take(20) {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        if v["type"].as_str() == Some("session_meta") {
            return v["payload"]["cwd"].as_str().map(|s| s.to_string());
        }
    }
    None
}

/// Codex only creates the rollout file when the user submits their first
/// prompt, which can be arbitrarily long after the session spawns — so there
/// is no deadline here; poll until the file appears or the app shuts down.
async fn wait_for_new_rollout(
    dir: &Path,
    existing: &HashSet<PathBuf>,
    cwd: &str,
    tx: &tokio::sync::mpsc::Sender<AppEvent>,
) -> Option<PathBuf> {
    loop {
        let mut candidates: Vec<PathBuf> = jsonl_files(dir)
            .into_iter()
            .filter(|p| !existing.contains(p))
            .collect();
        candidates.sort();

        for path in candidates {
            match rollout_cwd(&path) {
                // Claim so two codex sessions in the same cwd don't both tail
                // the same rollout and double-report its usage.
                Some(path_cwd) if path_cwd == cwd => {
                    if crate::claude_log::claim_jsonl(&path) {
                        return Some(path);
                    }
                }
                Some(_) => {}
                None => {
                    // Codex creates the file before all early metadata is flushed.
                    continue;
                }
            }
        }

        if tx.is_closed() {
            return None;
        }
        sleep(Duration::from_millis(500)).await;
    }
}

/// Model ID from a rollout record. Modern Codex (0.14x) reports it per turn in
/// `turn_context.payload.model`; older builds put it in `session_meta.payload`
/// as `model` or `agent_id`.
fn parse_model(v: &serde_json::Value) -> Option<String> {
    match v["type"].as_str()? {
        "turn_context" => v["payload"]["model"].as_str().map(str::to_owned),
        "session_meta" => v["payload"]["model"]
            .as_str()
            .or_else(|| v["payload"]["agent_id"].as_str())
            .map(str::to_owned),
        _ => None,
    }
}

fn parse_token_count(v: &serde_json::Value, model: &str, config: &Config) -> Option<TokenStats> {
    if v["type"].as_str()? != "event_msg" {
        return None;
    }
    let payload = &v["payload"];
    if payload["type"].as_str()? != "token_count" {
        return None;
    }

    let info = &payload["info"];
    let total = &info["total_token_usage"];
    let last = &info["last_token_usage"];

    let input_tokens = total["input_tokens"].as_u64().unwrap_or(0);
    let cached_input_tokens = total["cached_input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = total["output_tokens"].as_u64().unwrap_or(0);
    let context_tokens = last["input_tokens"].as_u64().unwrap_or(0);

    if input_tokens == 0 && output_tokens == 0 && context_tokens == 0 {
        return None;
    }

    let rate = config.pricing.codex_rate(model);
    let billable_input_tokens = input_tokens.saturating_sub(cached_input_tokens);
    let total_cost_usd = (billable_input_tokens as f64 / 1_000_000.0) * rate.input
        + (cached_input_tokens as f64 / 1_000_000.0) * rate.cache_read
        + (output_tokens as f64 / 1_000_000.0) * rate.output;

    let fresh_input = input_tokens.saturating_sub(cached_input_tokens);
    Some(TokenStats {
        input_tokens: fresh_input,
        output_tokens,
        context_tokens,
        total_cost_usd,
    })
}

async fn tail(
    session_id: usize,
    path: &Path,
    tx: &tokio::sync::mpsc::Sender<AppEvent>,
    config: &Config,
) {
    let mut offset: u64 = 0;
    let mut model = "unknown".to_string();
    let mut acc_input: u64 = 0;
    let mut acc_output: u64 = 0;
    let mut acc_cost: f64 = 0.0;
    let mut context_tokens: u64 = 0;

    // Scan the first 20 lines (session_meta / first turn_context) for the model.
    if let Ok(content) = tokio::fs::read_to_string(path).await {
        for line in content.lines().take(20) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(m) = parse_model(&v) {
                    model = m;
                    break;
                }
            }
        }
    }
    if model != "unknown" {
        let _ = tx
            .send(AppEvent::SessionModel {
                session_id,
                model: model.clone(),
            })
            .await;
    }

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
                            // Don't advance offset; retry the partial bytes next poll.
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
                            // turn_context is written per turn, so this also
                            // tracks a mid-session model switch.
                            if let Some(m) = parse_model(&v) {
                                if m != model {
                                    model = m;
                                    let _ = tx
                                        .send(AppEvent::SessionModel {
                                            session_id,
                                            model: model.clone(),
                                        })
                                        .await;
                                }
                            }
                            if let Some(stats) = parse_token_count(&v, &model, config) {
                                acc_input = stats.input_tokens;
                                acc_output = stats.output_tokens;
                                acc_cost = stats.total_cost_usd;
                                context_tokens = stats.context_tokens;
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
                input_tokens: acc_input,
                output_tokens: acc_output,
                total_cost_usd: acc_cost,
                context_tokens,
            };
            if tx
                .send(AppEvent::SessionStats { session_id, stats })
                .await
                .is_err()
            {
                return;
            }
        }

        sleep(Duration::from_millis(500)).await;
    }
}

/// Spawn a watcher for the next Codex rollout JSONL created in this cwd.
pub fn spawn_watcher(
    session_id: usize,
    cwd: String,
    tx: tokio::sync::mpsc::Sender<AppEvent>,
    config: Arc<Config>,
    codex_home: Option<String>,
) {
    let dir = match sessions_dir(codex_home.as_deref()) {
        Some(d) => d,
        None => return,
    };

    // Snapshot existing rollout files SYNCHRONOUSLY, before the PTY runner is
    // spawned, so a fast-starting Codex can't create its file first and have
    // it land in `existing` (same race claude_log guards against).
    let existing = jsonl_files(&dir);

    tokio::spawn(async move {
        let jsonl = match wait_for_new_rollout(&dir, &existing, &cwd, &tx).await {
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
    fn parses_codex_token_count_event() {
        let v: serde_json::Value = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 99975,
                        "cached_input_tokens": 58752,
                        "output_tokens": 1358,
                        "reasoning_output_tokens": 116,
                        "total_tokens": 101333
                    },
                    "last_token_usage": {
                        "input_tokens": 31619,
                        "cached_input_tokens": 8576,
                        "output_tokens": 285,
                        "reasoning_output_tokens": 10,
                        "total_tokens": 31904
                    },
                    "model_context_window": 258400
                }
            }
        });

        let config = crate::config::Config::default();
        let stats = parse_token_count(&v, "unknown", &config).unwrap();
        assert_eq!(stats.input_tokens, 41223); // fresh input: 99975 - 58752 cached
        assert_eq!(stats.output_tokens, 1358);
        assert_eq!(stats.context_tokens, 31619);
        // "unknown" model rate is 0.0, so cost should be 0.
        assert_eq!(stats.total_cost_usd, 0.0);
    }

    #[test]
    fn computes_codex_token_based_credits_with_cached_input() {
        let v: serde_json::Value = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 1_000_000,
                        "cached_input_tokens": 250_000,
                        "output_tokens": 10_000
                    },
                    "last_token_usage": {
                        "input_tokens": 1_000_000
                    }
                }
            }
        });

        let config = crate::config::Config::default();
        let stats = parse_token_count(&v, "gpt-5.4-mini", &config).unwrap();

        let expected = 0.75 * 18.75 + 0.25 * 1.875 + 0.01 * 113.0;
        assert!((stats.total_cost_usd - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn ignores_non_token_count_events() {
        let v: serde_json::Value = serde_json::json!({
            "type": "event_msg",
            "payload": { "type": "task_started" }
        });

        let config = crate::config::Config::default();
        assert!(parse_token_count(&v, "unknown", &config).is_none());
    }

    #[test]
    fn parses_model_from_session_meta_preferring_model_over_agent_id() {
        let with_model = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "model": "gpt-5.4-mini",
                "agent_id": "fallback"
            }
        });
        let with_agent = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "agent_id": "gpt-5.3-codex"
            }
        });

        assert_eq!(parse_model(&with_model).as_deref(), Some("gpt-5.4-mini"));
        assert_eq!(parse_model(&with_agent).as_deref(), Some("gpt-5.3-codex"));
        assert!(parse_model(&serde_json::json!({"type": "event_msg"})).is_none());
    }

    #[test]
    fn parses_model_from_turn_context() {
        // Modern Codex (0.14x) puts the model in turn_context, not session_meta.
        let v = serde_json::json!({
            "type": "turn_context",
            "payload": {
                "turn_id": "abc",
                "cwd": "/home/u/proj",
                "model": "gpt-5.5"
            }
        });

        assert_eq!(parse_model(&v).as_deref(), Some("gpt-5.5"));

        let meta_without_model = serde_json::json!({
            "type": "session_meta",
            "payload": {"cwd": "/home/u/proj", "model_provider": "openai"}
        });
        assert!(parse_model(&meta_without_model).is_none());
    }

    #[test]
    fn token_count_ignores_zero_usage_records() {
        let v = serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 0,
                        "cached_input_tokens": 0,
                        "output_tokens": 0
                    },
                    "last_token_usage": {
                        "input_tokens": 0
                    }
                }
            }
        });

        assert!(parse_token_count(&v, "gpt-5.4", &Config::default()).is_none());
    }

    #[test]
    fn rollout_cwd_reads_session_meta_from_first_twenty_lines() {
        let path = std::env::temp_dir().join(format!(
            "linkshell-codex-rollout-cwd-{}.jsonl",
            std::process::id()
        ));
        let content = [
            serde_json::json!({"type": "event_msg"}).to_string(),
            serde_json::json!({
                "type": "session_meta",
                "payload": {"cwd": "/tmp/linkshell"}
            })
            .to_string(),
        ]
        .join("\n");
        std::fs::write(&path, content).unwrap();

        let cwd = rollout_cwd(&path);
        let _ = std::fs::remove_file(&path);

        assert_eq!(cwd.as_deref(), Some("/tmp/linkshell"));
    }

    #[test]
    fn sessions_dir_prefers_per_session_codex_home() {
        let dir = sessions_dir(Some("/opt/codex-personal")).unwrap();
        assert_eq!(dir, PathBuf::from("/opt/codex-personal/sessions"));
    }

    fn make_codex_token_event(input: u64, cached: u64, output: u64) -> serde_json::Value {
        serde_json::json!({
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": input,
                        "cached_input_tokens": cached,
                        "output_tokens": output
                    },
                    "last_token_usage": {
                        "input_tokens": input - cached
                    }
                }
            }
        })
    }

    #[test]
    fn cumulative_token_counts_not_inflated_by_naive_sum() {
        let entry1 = make_codex_token_event(10_000, 2_000, 3_000);
        let entry2 = make_codex_token_event(30_000, 8_000, 8_000);
        let entry3 = make_codex_token_event(50_000, 15_000, 12_000);

        let config = Config::default();

        let stats1 = parse_token_count(&entry1, "gpt-5", &config).unwrap();
        let _stats2 = parse_token_count(&entry2, "gpt-5", &config).unwrap();
        let stats3 = parse_token_count(&entry3, "gpt-5", &config).unwrap();

        assert_eq!(stats1.input_tokens, 8_000); // 10k - 2k cached = 8k fresh
        assert_eq!(stats3.input_tokens, 35_000); // 50k - 15k cached = 35k fresh
        assert_eq!(stats3.output_tokens, 12_000);

        let naive_sum_input = stats1.input_tokens + _stats2.input_tokens + stats3.input_tokens;
        assert!(
            naive_sum_input > stats3.input_tokens,
            "naive sum ({}) exceeds correct total",
            naive_sum_input
        );
    }

    #[test]
    fn cached_input_excluded_from_fresh_input_count() {
        let v = make_codex_token_event(100_000, 60_000, 5_000);
        let config = Config::default();
        let stats = parse_token_count(&v, "gpt-5.4-mini", &config).unwrap();

        assert_eq!(stats.input_tokens, 40_000); // only fresh (non-cached) input
        assert_eq!(stats.output_tokens, 5_000);
        assert!(stats.total_cost_usd > 0.0); // cost still includes cache pricing

        let v2 = make_codex_token_event(10_000, 0, 500);
        let stats2 = parse_token_count(&v2, "gpt-5.4-mini", &config).unwrap();
        assert_eq!(stats2.input_tokens, 10_000);
    }
}
