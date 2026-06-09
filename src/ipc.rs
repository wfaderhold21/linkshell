use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::events::{AppEvent, IpcQueryPayload};
use crate::session::{SessionState, TokenStats};

pub fn socket_path(config: &Config) -> String {
    config
        .socket
        .path
        .replace("{pid}", &std::process::id().to_string())
}

pub fn spawn_listener(tx: mpsc::Sender<AppEvent>, config: Arc<Config>) {
    let path = socket_path(&config);
    let max_bytes = config.general.max_ipc_message_bytes;
    tokio::spawn(async move {
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[ipc] failed to bind {}: {}", path, e);
                return;
            }
        };
        eprintln!("[linkshell] IPC socket: {}", path);
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let tx = tx.clone();
                    let (r, w) = stream.into_split();
                    tokio::spawn(handle_stream(Box::new(r), Box::new(w), tx, max_bytes));
                }
                Err(_) => break,
            }
        }
    });
}

pub fn spawn_tcp_listener(tx: mpsc::Sender<AppEvent>, port: u16, config: Arc<Config>) {
    let max_bytes = config.general.max_ipc_message_bytes;
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
                    tokio::spawn(handle_stream(Box::new(r), Box::new(w), tx, max_bytes));
                }
                Err(_) => break,
            }
        }
    });
}

/// Read one newline-terminated line, enforcing an optional byte limit.
/// When max_bytes == 0, behaves identically to read_line (no limit).
/// When the limit is exceeded, reads (and discards) the rest of the line,
/// returns Ok(0) to signal "oversize" (distinguishable from EOF which also
/// returns Ok(0) only when the very first read returns 0).
async fn read_limited_line(
    reader: &mut tokio::io::BufReader<impl tokio::io::AsyncRead + Unpin>,
    max_bytes: usize,
    line: &mut String,
) -> std::io::Result<usize> {
    if max_bytes == 0 {
        return reader.read_line(line).await;
    }
    let mut byte = [0u8; 1];
    let mut count = 0usize;
    let mut oversize = false;
    loop {
        match reader.read(&mut byte).await? {
            0 => return Ok(count),
            _ => {
                count += 1;
                if byte[0] == b'\n' {
                    if !oversize {
                        line.push('\n');
                    }
                    return if oversize { Ok(0) } else { Ok(count) };
                }
                if count > max_bytes {
                    oversize = true;
                } else {
                    line.push(byte[0] as char);
                }
            }
        }
    }
}

async fn handle_stream(
    read_half: Box<dyn tokio::io::AsyncRead + Send + Unpin>,
    write_half: Box<dyn tokio::io::AsyncWrite + Send + Unpin>,
    tx: mpsc::Sender<AppEvent>,
    max_bytes: usize,
) {
    let (agent_tx, agent_rx) = mpsc::channel::<String>(32);
    tokio::spawn(write_loop(write_half, agent_rx));

    let mut reader = tokio::io::BufReader::new(read_half);
    let mut line = String::new();

    match read_limited_line(&mut reader, max_bytes, &mut line).await {
        Ok(0) => {
            // EOF or oversize on the very first message
            if !line.is_empty() {
                // oversize — line was drained but truncated
                let err = serde_json::json!({"error": "message exceeds max_ipc_message_bytes"});
                let _ = agent_tx
                    .send(serde_json::to_string(&err).unwrap_or_default() + "\n")
                    .await;
            }
            return;
        }
        Ok(_) => {}
        Err(_) => return,
    }

    let first = match serde_json::from_str::<serde_json::Value>(line.trim()) {
        Ok(v) => v,
        Err(e) => {
            let err = serde_json::json!({"error": format!("invalid JSON: {}", e)});
            let _ = agent_tx
                .send(serde_json::to_string(&err).unwrap_or_default() + "\n")
                .await;
            return;
        }
    };
    line.clear();

    // `register` → persistent agent handshake.
    // `query`    → synchronous snapshot, then close.
    // Anything else → fire-and-forget (no session assigned).
    let session_id = if first["type"].as_str() == Some("register") {
        let name = first["name"].as_str().unwrap_or("agent").to_string();
        let group = first["group"].as_str().map(|s| s.to_string());
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let _ = tx
            .send(AppEvent::IpcQuery {
                payload: IpcQueryPayload::Register { name, group },
                response_tx: resp_tx,
            })
            .await;
        match tokio::time::timeout(Duration::from_secs(5), resp_rx).await {
            Ok(Ok(resp)) => {
                let sid = resp["session_id"].as_u64().unwrap_or(0) as usize;
                let msg = serde_json::to_string(&resp).unwrap_or_default() + "\n";
                let _ = agent_tx.send(msg).await;
                let _ = tx
                    .send(AppEvent::IpcAgentConnected {
                        session_id: sid,
                        agent_tx: agent_tx.clone(),
                    })
                    .await;
                Some(sid)
            }
            _ => return,
        }
    } else if first["type"].as_str() == Some("query") {
        // Synchronous query: respond and close.
        dispatch_query(&first, &tx, &agent_tx).await;
        return;
    } else {
        dispatch(&first, &tx, None, &agent_tx).await;
        None
    };

    loop {
        line.clear();
        match read_limited_line(&mut reader, max_bytes, &mut line).await {
            Ok(0) if line.is_empty() => break, // EOF
            Ok(0) => {
                // Oversize message in persistent session
                let err = serde_json::json!({"error": "message exceeds max_ipc_message_bytes"});
                let _ = agent_tx
                    .send(serde_json::to_string(&err).unwrap_or_default() + "\n")
                    .await;
                break;
            }
            Ok(_) => {
                if let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                    dispatch(&msg, &tx, session_id, &agent_tx).await;
                }
            }
            Err(_) => break,
        }
    }

    if let Some(sid) = session_id {
        let _ = tx
            .send(AppEvent::IpcAgentDisconnected { session_id: sid })
            .await;
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

/// Handle synchronous query messages — respond then return (caller closes connection).
async fn dispatch_query(
    msg: &serde_json::Value,
    tx: &mpsc::Sender<AppEvent>,
    writer: &mpsc::Sender<String>,
) {
    let what = msg["what"].as_str().unwrap_or("").to_string();
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    let _ = tx
        .send(AppEvent::IpcQuery {
            payload: IpcQueryPayload::Query { what },
            response_tx: resp_tx,
        })
        .await;
    match tokio::time::timeout(Duration::from_secs(5), resp_rx).await {
        Ok(Ok(response)) => {
            let _ = writer
                .send(serde_json::to_string(&response).unwrap_or_default() + "\n")
                .await;
        }
        _ => {
            let err = serde_json::json!({"error": "query timeout"});
            let _ = writer
                .send(serde_json::to_string(&err).unwrap_or_default() + "\n")
                .await;
        }
    }
}

async fn dispatch(
    msg: &serde_json::Value,
    tx: &mpsc::Sender<AppEvent>,
    registered_id: Option<usize>,
    writer: &mpsc::Sender<String>,
) {
    // If session_name is present but no numeric session_id, route to app for name resolution.
    let has_numeric_id = msg["session_id"].as_u64().is_some();
    if !has_numeric_id {
        if let Some(name) = msg["session_name"].as_str() {
            let _ = tx
                .send(AppEvent::IpcNamedAction {
                    session_name: name.to_string(),
                    msg: msg.clone(),
                })
                .await;
            return;
        }
    }

    // Message-level session_id takes precedence over the registered session.
    let target_id = msg["session_id"]
        .as_u64()
        .map(|v| v as usize)
        .or(registered_id);

    match msg["type"].as_str() {
        Some("state") => {
            let sid = match target_id {
                Some(s) => s,
                None => return,
            };
            if let Some(s) = parse_session_state(msg["state"].as_str().unwrap_or("")) {
                let _ = tx
                    .send(AppEvent::IpcStateOverride {
                        session_id: sid,
                        state: s,
                    })
                    .await;
            }
            if let Some(detail) = msg["detail"].as_str() {
                let _ = tx
                    .send(AppEvent::SessionOutput {
                        session_id: sid,
                        line: format!("[{}]", detail),
                    })
                    .await;
            }
        }
        Some("tokens") => {
            let sid = match target_id {
                Some(s) => s,
                None => return,
            };
            let _ = tx
                .send(AppEvent::IpcTokenUpdate {
                    session_id: sid,
                    stats: TokenStats {
                        input_tokens: msg["input"].as_u64().unwrap_or(0),
                        output_tokens: msg["output"].as_u64().unwrap_or(0),
                        total_cost_usd: msg["cost"].as_f64().unwrap_or(0.0),
                        context_tokens: 0,
                    },
                })
                .await;
        }
        Some("output") => {
            let sid = match target_id {
                Some(s) => s,
                None => return,
            };
            if let Some(text) = msg["line"].as_str() {
                let _ = tx
                    .send(AppEvent::SessionOutput {
                        session_id: sid,
                        line: text.to_string(),
                    })
                    .await;
            }
        }
        Some("fire_pipe") => {
            if let Some(grp) = msg["source_group"].as_str() {
                let _ = tx
                    .send(AppEvent::IpcGroupFire {
                        source_group: grp.to_string(),
                    })
                    .await;
            } else if let Some(source) = msg["source"].as_u64() {
                let dest = msg["dest"].as_u64().map(|v| v as usize);
                let _ = tx
                    .send(AppEvent::IpcFirePipe {
                        source: source as usize,
                        dest,
                    })
                    .await;
            }
        }
        Some("broadcast") => {
            if let Some(group) = msg["group"].as_str() {
                let inner = msg["message"].clone();
                let _ = tx
                    .send(AppEvent::IpcBroadcast {
                        group: group.to_string(),
                        msg: inner,
                    })
                    .await;
            }
        }
        Some("pipe_add") => {
            if let (Some(src), Some(dst)) = (msg["source"].as_u64(), msg["dest"].as_u64()) {
                let _ = tx
                    .send(AppEvent::IpcPipeAdd {
                        source: src as usize,
                        dest: dst as usize,
                        trigger: msg["trigger"].as_str().unwrap_or("on_ready").to_string(),
                        extract: msg["extract"].as_str().unwrap_or("last-block").to_string(),
                        prefix: msg["prefix"].as_str().map(|s| s.to_string()),
                    })
                    .await;
            }
        }
        Some("pipe_remove") => {
            if let Some(src) = msg["source"].as_u64() {
                let dest = msg["dest"].as_u64().map(|v| v as usize);
                let _ = tx
                    .send(AppEvent::IpcPipeRemove {
                        source: src as usize,
                        dest,
                    })
                    .await;
            }
        }
        Some("session_create") => {
            let kind_str = msg["kind"].as_str().unwrap_or("claude").to_string();
            let name = msg["name"].as_str().unwrap_or("").to_string();
            let cwd = msg["cwd"].as_str().unwrap_or(".").to_string();
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
            let _ = tx
                .send(AppEvent::IpcQuery {
                    payload: IpcQueryPayload::SessionCreate {
                        kind_str,
                        name,
                        cwd,
                    },
                    response_tx: resp_tx,
                })
                .await;
            match tokio::time::timeout(Duration::from_secs(10), resp_rx).await {
                Ok(Ok(response)) => {
                    let _ = writer
                        .send(serde_json::to_string(&response).unwrap_or_default() + "\n")
                        .await;
                }
                _ => {}
            }
        }
        Some("session_input_wait") => {
            let target_sid = msg["session_id"].as_u64().unwrap_or(0) as usize;
            let text = msg["text"].as_str().unwrap_or("").to_string();
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
            let _ = tx
                .send(AppEvent::IpcQuery {
                    payload: IpcQueryPayload::SessionInputWait {
                        session_id: target_sid,
                        text,
                    },
                    response_tx: resp_tx,
                })
                .await;
            match tokio::time::timeout(Duration::from_secs(1200), resp_rx).await {
                Ok(Ok(response)) => {
                    let _ = writer
                        .send(serde_json::to_string(&response).unwrap_or_default() + "\n")
                        .await;
                }
                _ => {
                    let err = serde_json::json!({"error": "timeout waiting for session READY"});
                    let _ = writer
                        .send(serde_json::to_string(&err).unwrap_or_default() + "\n")
                        .await;
                }
            }
        }
        _ => {}
    }
}

pub fn parse_session_state(s: &str) -> Option<SessionState> {
    match s.to_uppercase().as_str() {
        "READY" => Some(SessionState::Ready),
        "THINKING" => Some(SessionState::Thinking),
        "RUNNING" => Some(SessionState::Running),
        "WAITING" => Some(SessionState::Waiting),
        "ERROR" => Some(SessionState::Error),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn socket_path_replaces_pid_placeholder_and_preserves_literal_paths() {
        let mut cfg = Config::default();
        cfg.socket.path = "/tmp/linkshell-{pid}.sock".into();
        assert_eq!(
            socket_path(&cfg),
            format!("/tmp/linkshell-{}.sock", std::process::id())
        );

        cfg.socket.path = "/tmp/static.sock".into();
        assert_eq!(socket_path(&cfg), "/tmp/static.sock");
    }

    #[test]
    fn parse_session_state_is_case_insensitive_and_rejects_unknown_values() {
        assert_eq!(parse_session_state("ready"), Some(SessionState::Ready));
        assert_eq!(parse_session_state("Thinking"), Some(SessionState::Thinking));
        assert_eq!(parse_session_state("WAITING!"), None);
        assert_eq!(parse_session_state("dead"), None);
    }

    #[tokio::test]
    async fn read_limited_line_reads_complete_line_under_limit() {
        let (mut writer, reader) = tokio::io::duplex(64);
        writer.write_all(b"{\"ok\":true}\n").await.unwrap();
        drop(writer);
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();

        let n = read_limited_line(&mut reader, 64, &mut line).await.unwrap();

        assert_eq!(n, 12);
        assert_eq!(line, "{\"ok\":true}\n");
    }

    #[tokio::test]
    async fn read_limited_line_returns_zero_and_truncated_line_when_oversize() {
        let (mut writer, reader) = tokio::io::duplex(64);
        writer.write_all(b"abcdef\n").await.unwrap();
        drop(writer);
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();

        let n = read_limited_line(&mut reader, 3, &mut line).await.unwrap();

        assert_eq!(n, 0);
        assert_eq!(line, "abc");
    }

    #[tokio::test]
    async fn dispatch_routes_named_actions_before_numeric_id_handling() {
        let (tx, mut rx) = mpsc::channel(1);
        let (writer, _writer_rx) = mpsc::channel(1);
        let msg = serde_json::json!({
            "type": "state",
            "session_name": "reviewer",
            "state": "READY"
        });

        dispatch(&msg, &tx, None, &writer).await;

        match rx.recv().await.unwrap() {
            AppEvent::IpcNamedAction { session_name, msg } => {
                assert_eq!(session_name, "reviewer");
                assert_eq!(msg["state"], "READY");
            }
            _ => panic!("expected named action"),
        }
    }

    #[tokio::test]
    async fn dispatch_uses_registered_id_for_state_tokens_and_output() {
        let (tx, mut rx) = mpsc::channel(3);
        let (writer, _writer_rx) = mpsc::channel(1);

        dispatch(
            &serde_json::json!({"type": "state", "state": "running", "detail": "busy"}),
            &tx,
            Some(9),
            &writer,
        )
        .await;
        dispatch(
            &serde_json::json!({"type": "tokens", "input": 3, "output": 4, "cost": 1.5}),
            &tx,
            Some(9),
            &writer,
        )
        .await;

        match rx.recv().await.unwrap() {
            AppEvent::IpcStateOverride { session_id, state } => {
                assert_eq!(session_id, 9);
                assert_eq!(state, SessionState::Running);
            }
            _ => panic!("expected state override"),
        }
        match rx.recv().await.unwrap() {
            AppEvent::SessionOutput { session_id, line } => {
                assert_eq!(session_id, 9);
                assert_eq!(line, "[busy]");
            }
            _ => panic!("expected detail output"),
        }
        match rx.recv().await.unwrap() {
            AppEvent::IpcTokenUpdate { session_id, stats } => {
                assert_eq!(session_id, 9);
                assert_eq!(stats.input_tokens, 3);
                assert_eq!(stats.output_tokens, 4);
                assert_eq!(stats.total_cost_usd, 1.5);
            }
            _ => panic!("expected token update"),
        }
    }

    #[tokio::test]
    async fn dispatch_routes_pipe_broadcast_and_group_fire_messages() {
        let (tx, mut rx) = mpsc::channel(4);
        let (writer, _writer_rx) = mpsc::channel(1);

        for msg in [
            serde_json::json!({"type": "fire_pipe", "source_group": "agents"}),
            serde_json::json!({"type": "fire_pipe", "source": 1, "dest": 2}),
            serde_json::json!({"type": "broadcast", "group": "agents", "message": {"hello": true}}),
            serde_json::json!({"type": "pipe_add", "source": 1, "dest": 2, "trigger": "manual", "extract": "diff", "prefix": "p"}),
        ] {
            dispatch(&msg, &tx, None, &writer).await;
        }

        assert!(matches!(
            rx.recv().await.unwrap(),
            AppEvent::IpcGroupFire { source_group } if source_group == "agents"
        ));
        assert!(matches!(
            rx.recv().await.unwrap(),
            AppEvent::IpcFirePipe { source: 1, dest: Some(2) }
        ));
        assert!(matches!(
            rx.recv().await.unwrap(),
            AppEvent::IpcBroadcast { group, msg } if group == "agents" && msg["hello"] == true
        ));
        assert!(matches!(
            rx.recv().await.unwrap(),
            AppEvent::IpcPipeAdd { source: 1, dest: 2, trigger, extract, prefix }
                if trigger == "manual" && extract == "diff" && prefix.as_deref() == Some("p")
        ));
    }
}
