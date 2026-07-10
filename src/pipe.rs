use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::events::AppEvent;
use crate::session::Session;

#[derive(Debug, Clone)]
pub enum ExtractMode {
    LastBlock,
    LastN(usize),
    Diff,
    Summarize(u32),
}

impl ExtractMode {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "last_block" => Ok(Self::LastBlock),
            "diff" => Ok(Self::Diff),
            value if value.starts_with("summarize:") => {
                Ok(Self::Summarize(value["summarize:".len()..].parse()?))
            }
            value if value.starts_with("last:") => Ok(Self::LastN(value["last:".len()..].parse()?)),
            _ => anyhow::bail!("unknown extract mode: {s}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PipeTrigger {
    OnReady,
    OnWaiting,
    Manual,
}

impl PipeTrigger {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "on_ready" => Ok(Self::OnReady),
            "on_waiting" => Ok(Self::OnWaiting),
            "manual" => Ok(Self::Manual),
            _ => anyhow::bail!("unknown trigger: {s}"),
        }
    }
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
    pub condition: Option<String>,
}

pub fn extract_from_session(sessions: &[Session], id: usize, mode: &ExtractMode) -> Option<String> {
    let session = sessions.iter().find(|s| s.id == id)?;
    let lines = &session.output_lines;

    match mode {
        ExtractMode::LastN(n) => {
            let start = lines.len().saturating_sub(*n);
            Some(
                lines
                    .iter()
                    .skip(start)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        }
        ExtractMode::LastBlock => {
            let mut in_block = false;
            let mut block: Vec<&str> = Vec::new();
            for line in lines.iter().rev() {
                if line.starts_with("```") {
                    if in_block {
                        break;
                    }
                    in_block = true;
                } else if in_block {
                    block.push(line.as_str());
                }
            }
            if block.is_empty() {
                None
            } else {
                Some(block.into_iter().rev().collect::<Vec<_>>().join("\n"))
            }
        }
        ExtractMode::Diff => {
            let diff: Vec<&str> = lines
                .iter()
                .filter(|l| l.starts_with('+') || l.starts_with('-'))
                .map(|s| s.as_str())
                .collect();
            if diff.is_empty() {
                None
            } else {
                Some(diff.join("\n"))
            }
        }
        ExtractMode::Summarize(_) => Some(lines.iter().cloned().collect::<Vec<_>>().join("\n")),
    }
}

pub fn fire_pipe_task(
    pipe: Pipe,
    content: String,
    tx: mpsc::Sender<AppEvent>,
    config: Arc<Config>,
) -> JoinHandle<()> {
    let dest_id = pipe.dest;
    let prefix = pipe.prefix.clone().unwrap_or_default();
    let extract = pipe.extract.clone();

    tokio::spawn(async move {
        let relay = match extract {
            ExtractMode::Summarize(max_tokens) => {
                summarize_for_relay(&content, max_tokens, &config)
                    .await
                    .unwrap_or(content)
            }
            _ => content,
        };

        let message = if prefix.is_empty() {
            format!("{}\n", relay)
        } else {
            format!("{}\n{}\n", prefix, relay)
        };

        let _ = tx.send(AppEvent::PipeRelay { dest_id, message }).await;
    })
}

async fn summarize_for_relay(
    content: &str,
    max_tokens: u32,
    config: &Config,
) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let api_key = std::env::var("ANTHROPIC_API_KEY")?;

    let body = serde_json::json!({
        "model": config.pipe.summarize.model,
        "max_tokens": max_tokens,
        "system": config.pipe.summarize.system,
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

    Ok(resp["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionKind, PTY_COLS, PTY_ROWS};

    fn session_with_lines(id: usize, lines: &[&str]) -> Session {
        let mut session = Session::new(
            id,
            format!("s{id}"),
            SessionKind::Shell,
            "/tmp".into(),
            PTY_ROWS,
            PTY_COLS,
            2000,
        );
        for line in lines {
            session.push_output_line((*line).to_string());
        }
        session
    }

    #[test]
    fn profile_pipe_strings_parse_strictly() {
        assert!(matches!(
            ExtractMode::parse("last_block").unwrap(),
            ExtractMode::LastBlock
        ));
        assert!(matches!(
            ExtractMode::parse("diff").unwrap(),
            ExtractMode::Diff
        ));
        assert!(matches!(
            ExtractMode::parse("last:40").unwrap(),
            ExtractMode::LastN(40)
        ));
        assert!(matches!(
            ExtractMode::parse("summarize:500").unwrap(),
            ExtractMode::Summarize(500)
        ));
        assert!(ExtractMode::parse("last:nope").is_err());
        assert!(ExtractMode::parse("everything").is_err());

        assert_eq!(
            PipeTrigger::parse("on_ready").unwrap(),
            PipeTrigger::OnReady
        );
        assert_eq!(
            PipeTrigger::parse("on_waiting").unwrap(),
            PipeTrigger::OnWaiting
        );
        assert_eq!(PipeTrigger::parse("manual").unwrap(), PipeTrigger::Manual);
        assert!(PipeTrigger::parse("sometimes").is_err());
    }

    #[test]
    fn last_n_extracts_tail_lines_and_handles_short_history() {
        let sessions = vec![session_with_lines(1, &["a", "b", "c"])];

        assert_eq!(
            extract_from_session(&sessions, 1, &ExtractMode::LastN(2)).unwrap(),
            "b\nc"
        );
        assert_eq!(
            extract_from_session(&sessions, 1, &ExtractMode::LastN(10)).unwrap(),
            "a\nb\nc"
        );
    }

    #[test]
    fn last_block_extracts_most_recent_fenced_content_without_markers() {
        let sessions = vec![session_with_lines(
            1,
            &[
                "before",
                "```",
                "old",
                "```",
                "middle",
                "```rust",
                "fn main() {}",
                "```",
                "after",
            ],
        )];

        assert_eq!(
            extract_from_session(&sessions, 1, &ExtractMode::LastBlock).unwrap(),
            "fn main() {}"
        );
    }

    #[test]
    fn last_block_returns_none_without_complete_block_content() {
        let sessions = vec![
            session_with_lines(1, &["plain", "text"]),
            session_with_lines(2, &["```", "unterminated"]),
        ];

        assert!(extract_from_session(&sessions, 1, &ExtractMode::LastBlock).is_none());
        assert!(extract_from_session(&sessions, 2, &ExtractMode::LastBlock).is_none());
    }

    #[test]
    fn diff_extract_keeps_added_and_removed_lines_only() {
        let sessions = vec![session_with_lines(
            1,
            &[" context", "-old", "+new", "@@ hunk", "done"],
        )];

        assert_eq!(
            extract_from_session(&sessions, 1, &ExtractMode::Diff).unwrap(),
            "-old\n+new"
        );
    }

    #[test]
    fn summarize_mode_returns_full_content_before_async_summary_step() {
        let sessions = vec![session_with_lines(1, &["one", "two"])];

        assert_eq!(
            extract_from_session(&sessions, 1, &ExtractMode::Summarize(50)).unwrap(),
            "one\ntwo"
        );
    }

    #[test]
    fn missing_session_returns_none_for_all_modes() {
        let sessions = vec![session_with_lines(1, &["a"])];

        assert!(extract_from_session(&sessions, 99, &ExtractMode::LastN(1)).is_none());
        assert!(extract_from_session(&sessions, 99, &ExtractMode::Diff).is_none());
    }

    #[tokio::test]
    async fn fire_pipe_task_formats_relay_with_prefix() {
        let (tx, mut rx) = mpsc::channel(1);
        let pipe = Pipe {
            source: 1,
            dest: 2,
            trigger: PipeTrigger::Manual,
            extract: ExtractMode::LastN(1),
            prefix: Some("Review this".into()),
            active: true,
            last_fired: None,
            condition: None,
        };

        fire_pipe_task(pipe, "payload".into(), tx, Arc::new(Config::default()));

        match rx.recv().await.unwrap() {
            AppEvent::PipeRelay { dest_id, message } => {
                assert_eq!(dest_id, 2);
                assert_eq!(message, "Review this\npayload\n");
            }
            _ => panic!("expected pipe relay"),
        }
    }
}
