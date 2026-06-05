use tokio::io::AsyncBufReadExt;
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use crate::events::AppEvent;
use crate::session::{SessionState, TokenStats};

pub const SOCKET_PATH: &str = "/tmp/linkshell.sock";

/// Spawn the IPC listener. Binds /tmp/linkshell.sock and accepts JSON-line
/// messages from external orchestrators (council, foundry-x, etc.).
/// `session_id` is the linkshell session that the orchestrator "owns".
pub fn spawn_listener(tx: mpsc::Sender<AppEvent>, session_id: usize) {
    tokio::spawn(async move {
        let _ = std::fs::remove_file(SOCKET_PATH);
        let listener = match UnixListener::bind(SOCKET_PATH) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[ipc] failed to bind {}: {}", SOCKET_PATH, e);
                return;
            }
        };

        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tx = tx.clone();
                    tokio::spawn(handle_connection(stream, tx, session_id));
                }
                Err(_) => break,
            }
        }
    });
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    tx: mpsc::Sender<AppEvent>,
    session_id: usize,
) {
    let mut reader = tokio::io::BufReader::new(stream);
    let mut line = String::new();

    while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
        if let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            dispatch(&msg, &tx, session_id).await;
        }
        line.clear();
    }
}

async fn dispatch(msg: &serde_json::Value, tx: &mpsc::Sender<AppEvent>, default_session_id: usize) {
    let session_id = msg["session_id"].as_u64().map(|v| v as usize).unwrap_or(default_session_id);
    match msg["type"].as_str() {
        Some("state") => {
            let state = parse_state(msg["state"].as_str().unwrap_or(""));
            if let Some(s) = state {
                let _ = tx.send(AppEvent::IpcStateOverride { session_id, state: s }).await;
            }
            if let Some(detail) = msg["detail"].as_str() {
                let _ = tx.send(AppEvent::SessionOutput {
                    session_id,
                    line: format!("[{}]", detail),
                }).await;
            }
        }
        Some("tokens") => {
            let input  = msg["input"].as_u64().unwrap_or(0);
            let output = msg["output"].as_u64().unwrap_or(0);
            let cost   = msg["cost"].as_f64().unwrap_or(0.0);
            let _ = tx.send(AppEvent::IpcTokenUpdate {
                session_id,
                stats: TokenStats {
                    input_tokens: input,
                    output_tokens: output,
                    total_cost_usd: cost,
                    context_tokens: 0,
                },
            }).await;
        }
        Some("output") => {
            if let Some(text) = msg["line"].as_str() {
                let _ = tx.send(AppEvent::SessionOutput {
                    session_id,
                    line: text.to_string(),
                }).await;
            }
        }
        _ => {}
    }
}

fn parse_state(s: &str) -> Option<SessionState> {
    match s.to_uppercase().as_str() {
        "READY"    => Some(SessionState::Ready),
        "THINKING" => Some(SessionState::Thinking),
        "RUNNING"  => Some(SessionState::Running),
        "WAITING"  => Some(SessionState::Waiting),
        "ERROR"    => Some(SessionState::Error),
        _          => None,
    }
}
