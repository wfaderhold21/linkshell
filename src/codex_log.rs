/// Watch ~/.codex/sessions for the rollout JSONL created by Codex CLI and emit
/// authoritative cumulative token/context stats from token_count events.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::time::{sleep, Duration};

use crate::events::AppEvent;
use crate::session::TokenStats;

fn sessions_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".codex").join("sessions"))
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

async fn wait_for_new_rollout(
    dir: &Path,
    existing: &HashSet<PathBuf>,
    cwd: &str,
    timeout: Duration,
) -> Option<PathBuf> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let mut candidates: Vec<PathBuf> = jsonl_files(dir)
            .into_iter()
            .filter(|p| !existing.contains(p))
            .collect();
        candidates.sort();

        for path in candidates {
            match rollout_cwd(&path) {
                Some(path_cwd) if path_cwd == cwd => return Some(path),
                Some(_) => {}
                None => {
                    // Codex creates the file before all early metadata is flushed.
                    continue;
                }
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(200)).await;
    }
}

fn parse_token_count(v: &serde_json::Value) -> Option<TokenStats> {
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
    let output_tokens = total["output_tokens"].as_u64().unwrap_or(0);
    let context_tokens = last["input_tokens"].as_u64().unwrap_or(0);

    if input_tokens == 0 && output_tokens == 0 && context_tokens == 0 {
        return None;
    }

    Some(TokenStats {
        input_tokens,
        output_tokens,
        context_tokens,
        total_cost_usd: 0.0,
    })
}

async fn tail(session_id: usize, path: &Path, tx: &tokio::sync::mpsc::Sender<AppEvent>) {
    let mut offset: u64 = 0;

    loop {
        if tx.is_closed() {
            break;
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
                            offset += n as u64;
                            let v: serde_json::Value = match serde_json::from_str(line.trim()) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            if let Some(stats) = parse_token_count(&v) {
                                if tx
                                    .send(AppEvent::SessionStats { session_id, stats })
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        sleep(Duration::from_millis(500)).await;
    }
}

/// Spawn a watcher for the next Codex rollout JSONL created in this cwd.
pub fn spawn_watcher(session_id: usize, cwd: String, tx: tokio::sync::mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let dir = match sessions_dir() {
            Some(d) => d,
            None => return,
        };

        let existing = jsonl_files(&dir);
        let jsonl = match wait_for_new_rollout(&dir, &existing, &cwd, Duration::from_secs(30)).await
        {
            Some(p) => p,
            None => return,
        };

        tail(session_id, &jsonl, &tx).await;
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

        let stats = parse_token_count(&v).unwrap();
        assert_eq!(stats.input_tokens, 99975);
        assert_eq!(stats.output_tokens, 1358);
        assert_eq!(stats.context_tokens, 31619);
        assert_eq!(stats.total_cost_usd, 0.0);
    }

    #[test]
    fn ignores_non_token_count_events() {
        let v: serde_json::Value = serde_json::json!({
            "type": "event_msg",
            "payload": { "type": "task_started" }
        });

        assert!(parse_token_count(&v).is_none());
    }
}
