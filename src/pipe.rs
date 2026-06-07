use std::time::Instant;

use tokio::sync::mpsc;

use crate::events::AppEvent;
use crate::session::Session;

#[derive(Debug, Clone)]
pub enum ExtractMode {
    LastBlock,
    LastN(usize),
    Diff,
    Summarize(u32),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PipeTrigger {
    OnReady,
    OnWaiting,
    Manual,
}

#[derive(Debug, Clone)]
pub struct Pipe {
    pub source: usize,
    pub dest: usize,
    pub trigger: PipeTrigger,
    pub extract: ExtractMode,
    pub prefix: Option<String>,
    pub active: bool,
    pub last_fired: Option<Instant>,
}

pub fn extract_from_session(sessions: &[Session], id: usize, mode: &ExtractMode) -> Option<String> {
    let session = sessions.iter().find(|s| s.id == id)?;
    let lines = &session.output_lines;

    match mode {
        ExtractMode::LastN(n) => {
            let start = lines.len().saturating_sub(*n);
            Some(lines.iter().skip(start).cloned().collect::<Vec<_>>().join("\n"))
        }
        ExtractMode::LastBlock => {
            let mut in_block = false;
            let mut block: Vec<&str> = Vec::new();
            for line in lines.iter().rev() {
                if line.starts_with("```") {
                    if in_block { break; }
                    in_block = true;
                } else if in_block {
                    block.push(line.as_str());
                }
            }
            if block.is_empty() { None }
            else { Some(block.into_iter().rev().collect::<Vec<_>>().join("\n")) }
        }
        ExtractMode::Diff => {
            let diff: Vec<&str> = lines.iter()
                .filter(|l| l.starts_with('+') || l.starts_with('-'))
                .map(|s| s.as_str())
                .collect();
            if diff.is_empty() { None } else { Some(diff.join("\n")) }
        }
        ExtractMode::Summarize(_) => {
            Some(lines.iter().cloned().collect::<Vec<_>>().join("\n"))
        }
    }
}

pub fn fire_pipe_task(pipe: Pipe, content: String, tx: mpsc::Sender<AppEvent>) {
    let dest_id = pipe.dest;
    let prefix = pipe.prefix.clone().unwrap_or_default();
    let extract = pipe.extract.clone();

    tokio::spawn(async move {
        let relay = match extract {
            ExtractMode::Summarize(max_tokens) => {
                summarize_for_relay(&content, max_tokens).await.unwrap_or(content)
            }
            _ => content,
        };

        let message = if prefix.is_empty() {
            format!("{}\n", relay)
        } else {
            format!("{}\n{}\n", prefix, relay)
        };

        let _ = tx.send(AppEvent::PipeRelay { dest_id, message }).await;
    });
}

async fn summarize_for_relay(content: &str, max_tokens: u32) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let api_key = std::env::var("ANTHROPIC_API_KEY")?;

    let body = serde_json::json!({
        "model": "claude-haiku-4-5-20251001",
        "max_tokens": max_tokens,
        "system": "Extract only the concrete output, code, or decision from this text. Be terse. No preamble.",
        "messages": [{ "role": "user", "content": content }]
    });

    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    Ok(resp["content"][0]["text"].as_str().unwrap_or("").to_string())
}
