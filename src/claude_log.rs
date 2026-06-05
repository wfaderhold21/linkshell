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

/// Parse a single JSONL line: if it's a type=assistant record, return the
/// per-turn token delta as a TokenStats.
fn parse_usage(line: &str) -> Option<TokenStats> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v["type"].as_str()? != "assistant" {
        return None;
    }
    let usage = v["message"]["usage"].as_object()?;

    let get = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);

    let input       = get("input_tokens");
    let cache_write = get("cache_creation_input_tokens");
    let cache_read  = get("cache_read_input_tokens");
    let output      = get("output_tokens");

    let input_tokens  = input + cache_write + cache_read;
    let output_tokens = output;

    if input_tokens == 0 && output_tokens == 0 {
        return None;
    }

    // Sonnet 4.x pricing — cache reads are 10x cheaper than regular input
    let cost = (input       as f64 / 1_000_000.0) *  3.00   // standard input
             + (cache_write as f64 / 1_000_000.0) *  3.75   // cache write (5-min)
             + (cache_read  as f64 / 1_000_000.0) *  0.30   // cache read
             + (output      as f64 / 1_000_000.0) * 15.00;  // output

    Some(TokenStats { input_tokens, output_tokens, total_cost_usd: cost })
}

/// Tail a JSONL file from `offset`, accumulating token stats into `total`.
/// Returns when the channel closes or an unrecoverable read error occurs.
async fn tail(
    session_id: usize,
    path: &Path,
    tx: &tokio::sync::mpsc::Sender<AppEvent>,
) {
    let mut total = TokenStats::default();
    let mut offset: u64 = 0;

    loop {
        if tx.is_closed() {
            break;
        }

        let mut new_data = false;
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
                            if let Some(delta) = parse_usage(&line) {
                                total.input_tokens  += delta.input_tokens;
                                total.output_tokens += delta.output_tokens;
                                total.total_cost_usd += delta.total_cost_usd;
                                new_data = true;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        if new_data {
            if tx.send(AppEvent::SessionStats {
                session_id,
                stats: total.clone(),
            }).await.is_err() {
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
