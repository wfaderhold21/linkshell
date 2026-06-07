use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use crate::events::{AppEvent, IpcQueryPayload};
use crate::session::{SessionState, TokenStats};

pub const SOCKET_PATH: &str = "/tmp/linkshell.sock";

pub fn spawn_listener(tx: mpsc::Sender<AppEvent>) {
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
                    let (r, w) = stream.into_split();
                    tokio::spawn(handle_stream(Box::new(r), Box::new(w), tx));
                }
                Err(_) => break,
            }
        }
    });
}

pub fn spawn_tcp_listener(tx: mpsc::Sender<AppEvent>, port: u16) {
    tokio::spawn(async move {
        let addr = format!("127.0.0.1:{}", port);
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[ipc] failed to bind TCP {}: {}", addr, e);
                return;
            }
        };
        eprintln!("[ipc] TCP agent listener on {}", addr);
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tx = tx.clone();
                    let (r, w) = stream.into_split();
                    tokio::spawn(handle_stream(Box::new(r), Box::new(w), tx));
                }
                Err(_) => break,
            }
        }
    });
}

async fn handle_stream(
    read_half: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    write_half: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    tx: mpsc::Sender<AppEvent>,
) {
    let (agent_tx, agent_rx) = mpsc::channel::<String>(32);
    tokio::spawn(write_loop(write_half, agent_rx));

    let mut reader = tokio::io::BufReader::new(read_half);
    let mut line = String::new();

    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }
    let first = match serde_json::from_str::<serde_json::Value>(line.trim()) {
        Ok(v) => v,
        Err(_) => return,
    };
    line.clear();

    // If first message is `register`, do handshake and enter persistent mode.
    // Otherwise treat as a legacy fire-and-forget connection (no session assigned).
    let session_id = if first["type"].as_str() == Some("register") {
        let name = first["name"].as_str().unwrap_or("agent").to_string();
        let group = first["group"].as_str().map(|s| s.to_string());
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(AppEvent::IpcQuery {
            payload: IpcQueryPayload::Register { name, group },
            response_tx: resp_tx,
        }).await;
        match tokio::time::timeout(Duration::from_secs(5), resp_rx).await {
            Ok(Ok(resp)) => {
                let sid = resp["session_id"].as_u64().unwrap_or(0) as usize;
                let msg = serde_json::to_string(&resp).unwrap_or_default() + "\n";
                let _ = agent_tx.send(msg).await;
                let _ = tx.send(AppEvent::IpcAgentConnected {
                    session_id: sid,
                    agent_tx: agent_tx.clone(),
                }).await;
                Some(sid)
            }
            _ => return,
        }
    } else {
        dispatch(&first, &tx, None, &agent_tx).await;
        None
    };

    while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
        if let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            dispatch(&msg, &tx, session_id, &agent_tx).await;
        }
        line.clear();
    }

    if let Some(sid) = session_id {
        let _ = tx.send(AppEvent::IpcAgentDisconnected { session_id: sid }).await;
    }
}

async fn write_loop(
    mut write_half: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    mut rx: mpsc::Receiver<String>,
) {
    while let Some(msg) = rx.recv().await {
        if write_half.write_all(msg.as_bytes()).await.is_err() {
            break;
        }
    }
}

async fn dispatch(
    msg: &serde_json::Value,
    tx: &mpsc::Sender<AppEvent>,
    registered_id: Option<usize>,
    writer: &mpsc::Sender<String>,
) {
    // Message-level session_id takes precedence over the registered session.
    let target_id = msg["session_id"]
        .as_u64()
        .map(|v| v as usize)
        .or(registered_id);

    match msg["type"].as_str() {
        Some("state") => {
            let sid = match target_id { Some(s) => s, None => return };
            if let Some(s) = parse_state(msg["state"].as_str().unwrap_or("")) {
                let _ = tx.send(AppEvent::IpcStateOverride { session_id: sid, state: s }).await;
            }
            if let Some(detail) = msg["detail"].as_str() {
                let _ = tx.send(AppEvent::SessionOutput {
                    session_id: sid,
                    line: format!("[{}]", detail),
                }).await;
            }
        }
        Some("tokens") => {
            let sid = match target_id { Some(s) => s, None => return };
            let _ = tx.send(AppEvent::IpcTokenUpdate {
                session_id: sid,
                stats: TokenStats {
                    input_tokens:  msg["input"].as_u64().unwrap_or(0),
                    output_tokens: msg["output"].as_u64().unwrap_or(0),
                    total_cost_usd: msg["cost"].as_f64().unwrap_or(0.0),
                    context_tokens: 0,
                },
            }).await;
        }
        Some("output") => {
            let sid = match target_id { Some(s) => s, None => return };
            if let Some(text) = msg["line"].as_str() {
                let _ = tx.send(AppEvent::SessionOutput {
                    session_id: sid,
                    line: text.to_string(),
                }).await;
            }
        }
        Some("fire_pipe") => {
            if let Some(source) = msg["source"].as_u64() {
                let dest = msg["dest"].as_u64().map(|v| v as usize);
                let _ = tx.send(AppEvent::IpcFirePipe {
                    source: source as usize,
                    dest,
                }).await;
            }
        }
        Some("session_create") => {
            let kind_str = msg["kind"].as_str().unwrap_or("claude").to_string();
            let name     = msg["name"].as_str().unwrap_or("").to_string();
            let cwd      = msg["cwd"].as_str().unwrap_or(".").to_string();
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(AppEvent::IpcQuery {
                payload: IpcQueryPayload::SessionCreate { kind_str, name, cwd },
                response_tx: resp_tx,
            }).await;
            match tokio::time::timeout(Duration::from_secs(10), resp_rx).await {
                Ok(Ok(response)) => {
                    let _ = writer.send(serde_json::to_string(&response).unwrap_or_default() + "\n").await;
                }
                _ => {}
            }
        }
        Some("session_input_wait") => {
            let target_sid = msg["session_id"].as_u64().unwrap_or(0) as usize;
            let text = msg["text"].as_str().unwrap_or("").to_string();
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
            let _ = tx.send(AppEvent::IpcQuery {
                payload: IpcQueryPayload::SessionInputWait { session_id: target_sid, text },
                response_tx: resp_tx,
            }).await;
            match tokio::time::timeout(Duration::from_secs(1200), resp_rx).await {
                Ok(Ok(response)) => {
                    let _ = writer.send(serde_json::to_string(&response).unwrap_or_default() + "\n").await;
                }
                _ => {
                    let err = serde_json::json!({"error": "timeout waiting for session READY"});
                    let _ = writer.send(serde_json::to_string(&err).unwrap_or_default() + "\n").await;
                }
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
