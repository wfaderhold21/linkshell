//! Watch `~/.omp/agent/sessions` for the transcript JSONL oh-my-pi writes and
//! emit cumulative token/context/cost stats from its per-turn `usage` records.
//!
//! omp starts a *new* transcript file whenever the user runs `/new`, so the
//! watcher follows the chain rather than a single file: totals from the file
//! it was tailing are carried forward as a base when it rolls to the next one.
//! Tailing one file would zero the session's tokens on every `/new`.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tokio::time::{sleep, Duration};

use crate::events::AppEvent;
use crate::session::TokenStats;

pub fn sessions_dir(omp_home: Option<&str>) -> Option<PathBuf> {
    let base = match omp_home {
        Some(dir) => PathBuf::from(dir),
        None => std::env::var("OMP_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".omp")))
            .ok()?,
    };
    Some(base.join("agent").join("sessions"))
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

/// The `session` header record carries the working directory omp was started
/// in — the only reliable way to tell one transcript from another, since the
/// directory name is a lossy slug of that path.
fn transcript_cwd(path: &Path) -> Option<String> {
    let data = std::fs::read_to_string(path).ok()?;
    for line in data.lines().take(20) {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["type"].as_str() == Some("session") {
            return v["cwd"].as_str().map(str::to_owned);
        }
    }
    None
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Per-turn usage from an assistant message. omp reports each API call
/// separately, so these are turn deltas that the tail accumulates — except
/// `context_tokens`, which is a snapshot of the whole prompt.
struct RawUsage {
    input: u64,
    output: u64,
    cost: f64,
    context: u64,
    model: Option<String>,
}

fn parse_usage(v: &serde_json::Value) -> Option<RawUsage> {
    if v["type"].as_str()? != "message" {
        return None;
    }
    let message = &v["message"];
    if message["role"].as_str()? != "assistant" {
        return None;
    }
    let usage = &message["usage"];
    let input = usage["input"].as_u64().unwrap_or(0);
    let output = usage["output"].as_u64().unwrap_or(0);
    let cache_read = usage["cacheRead"].as_u64().unwrap_or(0);
    let cache_write = usage["cacheWrite"].as_u64().unwrap_or(0);
    if input == 0 && output == 0 && cache_read == 0 {
        return None;
    }
    // A local model reports zero cost forever; that is a real answer, not a
    // missing one, so it stays zero rather than being estimated.
    let cost = usage["cost"]["total"].as_f64().unwrap_or(0.0);
    let context = message["contextSnapshot"]["promptTokens"]
        .as_u64()
        .unwrap_or(input + cache_read + cache_write);
    Some(RawUsage {
        input,
        output,
        cost,
        context,
        model: message["model"].as_str().map(str::to_owned),
    })
}

/// Model id, either from a standalone `model_change` record or from the turn
/// that used it. omp prefixes the provider (`lm-studio/thinkingcap-…`); the
/// status panel has room for the model, not the routing.
fn parse_model(v: &serde_json::Value) -> Option<String> {
    if v["type"].as_str()? != "model_change" {
        return None;
    }
    v["model"].as_str().map(strip_provider)
}

fn strip_provider(model: &str) -> String {
    model
        .rsplit_once('/')
        .map(|(_, m)| m)
        .unwrap_or(model)
        .to_string()
}

/// Totals carried out of one transcript when omp rolls to the next.
#[derive(Default, Clone, Copy)]
struct Carried {
    input: u64,
    output: u64,
    cost: f64,
}

/// The transcript for this cwd that omp is writing to now, if it is newer than
/// the one we are on. `/new` writes a fresh file rather than truncating.
fn newer_transcript(dir: &Path, cwd: &str, current: &Path) -> Option<PathBuf> {
    let current_mtime = mtime(current)?;
    let mut candidates: Vec<PathBuf> = jsonl_files(dir)
        .into_iter()
        .filter(|p| p != current)
        .filter(|p| mtime(p).is_some_and(|m| m > current_mtime))
        .filter(|p| transcript_cwd(p).as_deref() == Some(cwd))
        .collect();
    candidates.sort();
    candidates.pop()
}

/// Wait for a transcript in `cwd` that this watcher can claim: either one omp
/// created after we spawned, or a pre-existing one it resumed into (mtime past
/// our spawn time). omp only writes the file once the first prompt is sent, so
/// there is no deadline.
async fn wait_for_transcript(
    dir: &Path,
    existing: &HashSet<PathBuf>,
    spawn_time: std::time::SystemTime,
    cwd: &str,
    tx: &tokio::sync::mpsc::Sender<AppEvent>,
) -> Option<PathBuf> {
    loop {
        let mut candidates: Vec<PathBuf> = jsonl_files(dir)
            .into_iter()
            .filter(|p| !existing.contains(p) || mtime(p).is_some_and(|m| m > spawn_time))
            .collect();
        candidates.sort();

        for path in candidates {
            if transcript_cwd(&path).as_deref() == Some(cwd)
                && crate::claude_log::claim_jsonl(&path)
            {
                return Some(path);
            }
        }

        if tx.is_closed() {
            return None;
        }
        sleep(Duration::from_millis(500)).await;
    }
}

/// Tail one transcript until omp rolls to the next one for this cwd (or the
/// app shuts down), reporting `base` plus this file's totals as it goes.
/// Returns the totals to carry into the next file.
async fn tail(
    session_id: usize,
    dir: &Path,
    path: &Path,
    cwd: &str,
    base: Carried,
    tx: &tokio::sync::mpsc::Sender<AppEvent>,
) -> Option<(PathBuf, Carried)> {
    let mut offset: u64 = 0;
    let mut acc = Carried::default();
    let mut context_tokens: u64 = 0;
    let mut model: Option<String> = None;

    loop {
        if tx.is_closed() {
            return None;
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
                            // No trailing newline means we caught omp mid-write;
                            // leave the offset so the next poll re-reads it whole.
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
                            let turn_model = parse_model(&v);
                            let usage = parse_usage(&v);
                            let seen_model = turn_model.or_else(|| {
                                usage
                                    .as_ref()
                                    .and_then(|u| u.model.as_deref().map(strip_provider))
                            });
                            if let Some(m) = seen_model {
                                if model.as_deref() != Some(&m) {
                                    model = Some(m.clone());
                                    let _ = tx
                                        .send(AppEvent::SessionModel {
                                            session_id,
                                            model: m,
                                        })
                                        .await;
                                }
                            }
                            if let Some(u) = usage {
                                acc.input += u.input;
                                acc.output += u.output;
                                acc.cost += u.cost;
                                context_tokens = u.context;
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
                input_tokens: base.input + acc.input,
                output_tokens: base.output + acc.output,
                total_cost_usd: base.cost + acc.cost,
                context_tokens,
            };
            if tx
                .send(AppEvent::SessionStats { session_id, stats })
                .await
                .is_err()
            {
                return None;
            }
        }

        // `/new` rolls the transcript. Follow it, keeping what this file spent
        // so the session's tokens climb instead of resetting to zero.
        if let Some(next) = newer_transcript(dir, cwd, path) {
            if crate::claude_log::claim_jsonl(&next) {
                return Some((
                    next,
                    Carried {
                        input: base.input + acc.input,
                        output: base.output + acc.output,
                        cost: base.cost + acc.cost,
                    },
                ));
            }
        }

        sleep(Duration::from_millis(500)).await;
    }
}

/// Spawn a watcher for the omp transcripts written in this cwd.
pub fn spawn_watcher(
    session_id: usize,
    cwd: String,
    tx: tokio::sync::mpsc::Sender<AppEvent>,
    omp_home: Option<String>,
) {
    let dir = match sessions_dir(omp_home.as_deref()) {
        Some(d) => d,
        None => return,
    };

    // Snapshot before the PTY starts, like the other watchers, so a fast omp
    // can't create its transcript and have it counted as pre-existing.
    let existing = jsonl_files(&dir);
    let spawn_time = std::time::SystemTime::now();

    tokio::spawn(async move {
        let mut path = match wait_for_transcript(&dir, &existing, spawn_time, &cwd, &tx).await {
            Some(p) => p,
            None => return,
        };
        let mut base = Carried::default();
        while let Some((next, carried)) = tail(session_id, &dir, &path, &cwd, base, &tx).await {
            path = next;
            base = carried;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    fn transcript(cwd: &str, input: u64, output: u64) -> String {
        format!(
            "{{\"type\":\"session\",\"version\":3,\"cwd\":\"{cwd}\"}}\n\
             {{\"type\":\"model_change\",\"model\":\"lm-studio/qwen3.6-27b\"}}\n\
             {{\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\
               \"usage\":{{\"input\":{input},\"output\":{output},\"cacheRead\":0,\
                          \"cacheWrite\":0,\"cost\":{{\"total\":0}}}},\
               \"contextSnapshot\":{{\"promptTokens\":{input}}}}}}}\n"
        )
    }

    /// `/new` starts a fresh transcript. The session's tokens must keep
    /// climbing across that roll — resetting to the new file's totals is the
    /// bug this watcher exists to avoid.
    #[tokio::test]
    async fn a_new_transcript_carries_the_previous_ones_totals_forward() {
        let home = std::env::temp_dir().join(format!(
            "omp-home-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let dir = home.join("agent").join("sessions").join("-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = "/home/u/proj";

        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        spawn_watcher(7, cwd.into(), tx, Some(home.to_string_lossy().into_owned()));

        std::fs::write(dir.join("a.jsonl"), transcript(cwd, 1000, 200)).unwrap();

        let first = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(AppEvent::SessionStats { stats, .. }) = rx.recv().await {
                    return stats;
                }
            }
        })
        .await
        .expect("stats for the first transcript");
        assert_eq!(first.input_tokens, 1000);
        assert_eq!(first.output_tokens, 200);
        assert_eq!(first.context_tokens, 1000);

        // Newer mtime, as `/new` produces.
        sleep(Duration::from_millis(1100)).await;
        std::fs::write(dir.join("b.jsonl"), transcript(cwd, 300, 50)).unwrap();

        let rolled = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(AppEvent::SessionStats { stats, .. }) = rx.recv().await {
                    if stats.input_tokens != 1000 {
                        return stats;
                    }
                }
            }
        })
        .await
        .expect("stats after the transcript rolled");
        assert_eq!(rolled.input_tokens, 1300, "totals carried across /new");
        assert_eq!(rolled.output_tokens, 250);
        // Context is a snapshot of the live prompt, so it does follow /new down.
        assert_eq!(rolled.context_tokens, 300);

        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn assistant_usage_becomes_a_turn_delta_with_a_context_snapshot() {
        let u = parse_usage(&value(
            r#"{"type":"message","message":{"role":"assistant",
                "model":"lm-studio/thinkingcap-qwen3.6-27b",
                "usage":{"input":28029,"output":160,"cacheRead":0,"cacheWrite":0,
                         "totalTokens":28189,"cost":{"total":0}},
                "contextSnapshot":{"promptTokens":28029}}}"#,
        ))
        .expect("assistant usage");
        assert_eq!(u.input, 28029);
        assert_eq!(u.output, 160);
        assert_eq!(u.cost, 0.0);
        assert_eq!(u.context, 28029);
        assert_eq!(
            u.model.as_deref(),
            Some("lm-studio/thinkingcap-qwen3.6-27b")
        );
    }

    #[test]
    fn records_without_assistant_usage_are_ignored() {
        assert!(parse_usage(&value(
            r#"{"type":"message","message":{"role":"user","content":[]}}"#
        ))
        .is_none());
        assert!(parse_usage(&value(
            r#"{"type":"custom","customType":"session_exit","data":{}}"#
        ))
        .is_none());
        assert!(parse_usage(&value(
            r#"{"type":"message","message":{"role":"assistant","usage":{"input":0,"output":0}}}"#
        ))
        .is_none());
    }

    #[test]
    fn the_model_loses_its_provider_prefix() {
        assert_eq!(
            parse_model(&value(
                r#"{"type":"model_change","model":"lm-studio/thinkingcap-qwen3.6-27b"}"#
            ))
            .as_deref(),
            Some("thinkingcap-qwen3.6-27b")
        );
        assert_eq!(strip_provider("qwen3.6-27b"), "qwen3.6-27b");
        assert!(parse_model(&value(r#"{"type":"session","cwd":"/tmp"}"#)).is_none());
    }

    #[test]
    fn the_session_header_identifies_the_transcripts_cwd() {
        let dir = std::env::temp_dir().join(format!("omp-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"title\",\"title\":\"x\"}\n\
             {\"type\":\"session\",\"version\":3,\"cwd\":\"/home/u/proj\"}\n",
        )
        .unwrap();
        assert_eq!(transcript_cwd(&path).as_deref(), Some("/home/u/proj"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sessions_dir_is_under_the_omp_home() {
        assert_eq!(
            sessions_dir(Some("/home/u/.omp")).unwrap(),
            PathBuf::from("/home/u/.omp/agent/sessions")
        );
    }
}
