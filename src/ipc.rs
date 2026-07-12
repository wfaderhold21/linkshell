use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

use crate::auth::{CapSet, Capability};
use crate::config::Config;
use crate::events::{AppEvent, IpcQueryPayload};
use crate::protocol::{self, Envelope, ErrorCode, Message, PROTOCOL_VERSION};
use crate::session::{SessionState, TokenStats};

const IPC_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq)]
pub enum Transport {
    Unix,
    Tcp,
}

fn runtime_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))
        });
    let dir = base.join("linkshell");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    dir
}

pub fn socket_path(config: &Config) -> String {
    if !config.socket.path.is_empty() && config.socket.path != "default" {
        return config
            .socket
            .path
            .replace("{pid}", &std::process::id().to_string());
    }
    runtime_dir()
        .join(format!("{}.sock", std::process::id()))
        .to_string_lossy()
        .into_owned()
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
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        eprintln!("[linkshell] IPC socket: {}", path);
        write_last_socket(&path);
        while let Ok((stream, _)) = listener.accept().await {
            #[cfg(target_os = "linux")]
            if peer_uid(&stream).ok() != Some(unsafe { libc::getuid() }) {
                continue;
            }
            #[cfg(not(target_os = "linux"))]
            {
                eprintln!(
                    "[ipc] warning: SO_PEERCRED unavailable on this platform, skipping uid check"
                );
            }
            let tx = tx.clone();
            let (r, w) = stream.into_split();
            tokio::spawn(handle_stream(
                Transport::Unix,
                Box::new(r),
                Box::new(w),
                tx,
                max_bytes,
            ));
        }
    });
}

#[cfg(target_os = "linux")]
pub(crate) fn peer_uid(stream: &tokio::net::UnixStream) -> std::io::Result<u32> {
    use std::os::unix::io::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(cred.uid)
}

fn write_last_socket(path: &str) {
    let Some(config_dir) = linkshell_config_dir() else {
        return;
    };
    if std::fs::create_dir_all(&config_dir).is_err() {
        return;
    }
    let _ = std::fs::write(config_dir.join("last_socket"), path);
}

pub fn cleanup(config: &Config) {
    let path = socket_path(config);
    let _ = std::fs::remove_file(&path);
    if let Some(config_dir) = linkshell_config_dir() {
        let _ = std::fs::remove_file(config_dir.join("last_socket"));
    }
}

fn linkshell_config_dir() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(std::path::PathBuf::from(path).join("linkshell"));
    }
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|home| home.join(".config").join("linkshell"))
}

#[derive(Debug, PartialEq, Eq)]
enum ReadResult {
    Eof,
    Line,
    OversizeDrained,
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
        while let Ok((stream, _)) = listener.accept().await {
            let tx = tx.clone();
            let (r, w) = stream.into_split();
            tokio::spawn(handle_stream(
                Transport::Tcp,
                Box::new(r),
                Box::new(w),
                tx,
                max_bytes,
            ));
        }
    });
}

async fn read_limited_line(
    reader: &mut tokio::io::BufReader<impl tokio::io::AsyncRead + Unpin>,
    max_bytes: usize,
    line: &mut String,
) -> std::io::Result<ReadResult> {
    if max_bytes == 0 {
        return match reader.read_line(line).await? {
            0 => Ok(ReadResult::Eof),
            _ => Ok(ReadResult::Line),
        };
    }
    let mut byte = [0u8; 1];
    let mut bytes = Vec::new();
    let mut count = 0usize;
    let mut oversize = false;
    loop {
        match reader.read(&mut byte).await? {
            0 if count == 0 => return Ok(ReadResult::Eof),
            0 => {
                line.push_str(&String::from_utf8_lossy(&bytes));
                return if oversize {
                    Ok(ReadResult::OversizeDrained)
                } else {
                    Ok(ReadResult::Line)
                };
            }
            _ => {
                count += 1;
                if byte[0] == b'\n' {
                    if !oversize {
                        bytes.push(b'\n');
                    }
                    line.push_str(&String::from_utf8_lossy(&bytes));
                    return if oversize {
                        Ok(ReadResult::OversizeDrained)
                    } else {
                        Ok(ReadResult::Line)
                    };
                }
                if count > max_bytes {
                    oversize = true;
                } else {
                    bytes.push(byte[0]);
                }
            }
        }
    }
}

async fn send_envelope(w: &mpsc::Sender<String>, env: &Envelope) {
    if let Ok(s) = serde_json::to_string(env) {
        let _ = w.send(s + "\n").await;
    }
}

async fn reply_err(w: &mpsc::Sender<String>, id: Option<u64>, code: ErrorCode, message: &str) {
    let env = Envelope {
        id,
        msg: Message::Error {
            code,
            message: message.to_string(),
        },
    };
    send_envelope(w, &env).await;
}

async fn handshake(
    first: &Envelope,
    transport: Transport,
    tx: &mpsc::Sender<AppEvent>,
    w: &mpsc::Sender<String>,
) -> Option<(Option<usize>, CapSet)> {
    let Message::Hello {
        protocol,
        token,
        name,
        group,
    } = &first.msg
    else {
        reply_err(w, first.id, ErrorCode::BadProtocol, "expected hello").await;
        return None;
    };
    if *protocol != PROTOCOL_VERSION {
        reply_err(w, first.id, ErrorCode::BadProtocol, "version mismatch").await;
        return None;
    }
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    if tx
        .send(AppEvent::Authenticate {
            token: token.clone(),
            transport,
            name: name.clone(),
            group: group.clone(),
            response_tx: resp_tx,
        })
        .await
        .is_err()
    {
        return None;
    }
    let (session_id, caps) = tokio::time::timeout(Duration::from_secs(5), resp_rx)
        .await
        .ok()
        .and_then(|r| r.ok())??;

    let caps_vec: Vec<Capability> = caps.iter().copied().collect();
    let env = Envelope {
        id: first.id,
        msg: Message::Welcome {
            protocol: PROTOCOL_VERSION,
            session_id,
            capabilities: caps_vec,
            server: env!("CARGO_PKG_VERSION").to_string(),
        },
    };
    send_envelope(w, &env).await;
    Some((session_id, caps))
}

async fn handle_stream(
    transport: Transport,
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
        Ok(ReadResult::Eof) => return,
        Ok(ReadResult::OversizeDrained) => {
            reply_err(
                &agent_tx,
                None,
                ErrorCode::Oversize,
                "message exceeds max_ipc_message_bytes",
            )
            .await;
            return;
        }
        Ok(ReadResult::Line) => {}
        Err(_) => return,
    }

    let first_env = match serde_json::from_str::<Envelope>(line.trim()) {
        Ok(e) => e,
        Err(e) => {
            reply_err(&agent_tx, None, ErrorCode::BadRequest, &e.to_string()).await;
            return;
        }
    };
    line.clear();

    let Some((registered_id, caps)) = handshake(&first_env, transport, &tx, &agent_tx).await else {
        return;
    };

    if let Some(sid) = registered_id {
        let _ = tx
            .send(AppEvent::IpcAgentConnected {
                session_id: sid,
                agent_tx: agent_tx.clone(),
            })
            .await;
    }

    loop {
        line.clear();
        match read_limited_line(&mut reader, max_bytes, &mut line).await {
            Ok(ReadResult::Eof) => break,
            Ok(ReadResult::OversizeDrained) => {
                reply_err(
                    &agent_tx,
                    None,
                    ErrorCode::Oversize,
                    "message exceeds max_ipc_message_bytes",
                )
                .await;
                continue;
            }
            Ok(ReadResult::Line) => {
                let env = match serde_json::from_str::<Envelope>(line.trim()) {
                    Ok(e) => e,
                    Err(e) => {
                        reply_err(&agent_tx, None, ErrorCode::BadRequest, &e.to_string()).await;
                        continue;
                    }
                };
                if let Some(cap) = protocol::required_cap(&env.msg) {
                    if !caps.contains(&cap) {
                        reply_err(&agent_tx, env.id, ErrorCode::Forbidden, "capability denied")
                            .await;
                        continue;
                    }
                }
                dispatch_msg(env, &tx, registered_id, &caps, &agent_tx).await;
            }
            Err(_) => break,
        }
    }

    if let Some(sid) = registered_id {
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
        match tokio::time::timeout(IPC_WRITE_TIMEOUT, write_half.write_all(msg.as_bytes())).await {
            Ok(Ok(())) => {}
            _ => break,
        }
    }
}

/// Which session a state/tokens/output message may act on. Explicit
/// `session_id`/`session_name` targeting requires `SignalAny` unless the id
/// is the connection's own registered session; without it a connection is
/// pinned to `registered_id`.
enum SignalTarget {
    Id(usize),
    Name(String),
}

fn resolve_signal_target(
    msg_sid: Option<usize>,
    session_name: Option<String>,
    registered_id: Option<usize>,
    caps: &CapSet,
) -> Result<Option<SignalTarget>, ()> {
    let any = caps.contains(&Capability::SignalAny);
    if let Some(sid) = msg_sid {
        return if any || registered_id == Some(sid) {
            Ok(Some(SignalTarget::Id(sid)))
        } else {
            Err(())
        };
    }
    if let Some(name) = session_name {
        return if any {
            Ok(Some(SignalTarget::Name(name)))
        } else {
            Err(())
        };
    }
    Ok(registered_id.map(SignalTarget::Id))
}

async fn dispatch_msg(
    env: Envelope,
    tx: &mpsc::Sender<AppEvent>,
    registered_id: Option<usize>,
    caps: &CapSet,
    writer: &mpsc::Sender<String>,
) {
    let env_id = env.id;
    match env.msg {
        Message::State {
            state,
            detail,
            session_id: msg_sid,
            session_name,
        } => {
            let target = match resolve_signal_target(msg_sid, session_name, registered_id, caps) {
                Ok(Some(t)) => t,
                Ok(None) => return,
                Err(()) => {
                    reply_err(writer, env_id, ErrorCode::Forbidden, "not your session").await;
                    return;
                }
            };
            match target {
                SignalTarget::Name(name) => {
                    let raw = build_state_legacy_json(&state, detail.as_deref(), None, Some(&name));
                    let _ = tx
                        .send(AppEvent::IpcNamedAction {
                            session_name: name,
                            msg: raw,
                        })
                        .await;
                }
                SignalTarget::Id(sid) => {
                    let _ = tx
                        .send(AppEvent::IpcStateOverride {
                            session_id: sid,
                            state: state.clone(),
                        })
                        .await;
                    if let Some(d) = detail {
                        let _ = tx
                            .send(AppEvent::SessionOutput {
                                session_id: sid,
                                line: format!("[{}]", d),
                            })
                            .await;
                    }
                }
            }
        }
        Message::Tokens {
            input,
            output,
            cost,
            session_id: msg_sid,
            session_name,
        } => {
            let target = match resolve_signal_target(msg_sid, session_name, registered_id, caps) {
                Ok(Some(t)) => t,
                Ok(None) => return,
                Err(()) => {
                    reply_err(writer, env_id, ErrorCode::Forbidden, "not your session").await;
                    return;
                }
            };
            match target {
                SignalTarget::Name(name) => {
                    let raw = serde_json::json!({"type":"tokens","input":input,"output":output,"cost":cost});
                    let _ = tx
                        .send(AppEvent::IpcNamedAction {
                            session_name: name,
                            msg: raw,
                        })
                        .await;
                }
                SignalTarget::Id(sid) => {
                    let _ = tx
                        .send(AppEvent::IpcTokenUpdate {
                            session_id: sid,
                            stats: TokenStats {
                                input_tokens: input,
                                output_tokens: output,
                                total_cost_usd: cost,
                                context_tokens: 0,
                            },
                        })
                        .await;
                }
            }
        }
        Message::Output {
            line,
            session_id: msg_sid,
            session_name,
        } => {
            let target = match resolve_signal_target(msg_sid, session_name, registered_id, caps) {
                Ok(Some(t)) => t,
                Ok(None) => return,
                Err(()) => {
                    reply_err(writer, env_id, ErrorCode::Forbidden, "not your session").await;
                    return;
                }
            };
            match target {
                SignalTarget::Name(name) => {
                    let raw = serde_json::json!({"type":"output","line":line});
                    let _ = tx
                        .send(AppEvent::IpcNamedAction {
                            session_name: name,
                            msg: raw,
                        })
                        .await;
                }
                SignalTarget::Id(sid) => {
                    let _ = tx
                        .send(AppEvent::SessionOutput {
                            session_id: sid,
                            line,
                        })
                        .await;
                }
            }
        }
        Message::AgentSend {
            dest,
            message,
            wait,
        } => {
            let (reply_tx, reply_rx) = if wait {
                let (t, r) = tokio::sync::oneshot::channel();
                (Some(t), Some(r))
            } else {
                (None, None)
            };
            let _ = tx
                .send(AppEvent::AgentDirectMessage {
                    from_session_id: registered_id,
                    dest_name: dest,
                    message,
                    reply_tx,
                })
                .await;
            if let Some(rx) = reply_rx {
                match tokio::time::timeout(Duration::from_secs(5), rx).await {
                    Ok(Ok(response)) => {
                        let _ = writer
                            .send(serde_json::to_string(&response).unwrap_or_default() + "\n")
                            .await;
                    }
                    _ => {
                        let err = serde_json::json!({"error": "agent_send timeout"});
                        let _ = writer
                            .send(serde_json::to_string(&err).unwrap_or_default() + "\n")
                            .await;
                    }
                }
            }
        }
        Message::FirePipe {
            source,
            dest,
            source_group,
        } => {
            if let Some(grp) = source_group {
                let _ = tx.send(AppEvent::IpcGroupFire { source_group: grp }).await;
            } else if let Some(src) = source.or(registered_id) {
                let _ = tx.send(AppEvent::IpcFirePipe { source: src, dest }).await;
            }
        }
        Message::Broadcast { group, message } => {
            let _ = tx
                .send(AppEvent::IpcBroadcast {
                    group,
                    msg: message,
                })
                .await;
        }
        Message::PipeAdd {
            source,
            dest,
            trigger,
            extract,
            prefix,
        } => {
            let _ = tx
                .send(AppEvent::IpcPipeAdd {
                    source,
                    dest,
                    trigger,
                    extract,
                    prefix,
                })
                .await;
        }
        Message::PipeRemove { source, dest } => {
            let _ = tx.send(AppEvent::IpcPipeRemove { source, dest }).await;
        }
        Message::SessionCreate { kind, name, cwd } => {
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
            let _ = tx
                .send(AppEvent::IpcQuery {
                    payload: IpcQueryPayload::SessionCreate {
                        kind_str: kind,
                        name: name.unwrap_or_default(),
                        cwd: cwd.unwrap_or_else(|| ".".to_string()),
                    },
                    response_tx: resp_tx,
                })
                .await;
            if let Ok(Ok(response)) = tokio::time::timeout(Duration::from_secs(10), resp_rx).await {
                let _ = writer
                    .send(serde_json::to_string(&response).unwrap_or_default() + "\n")
                    .await;
            }
        }
        Message::SessionInputWait { session_id, text } => {
            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
            let _ = tx
                .send(AppEvent::IpcQuery {
                    payload: IpcQueryPayload::SessionInputWait { session_id, text },
                    response_tx: resp_tx,
                })
                .await;
            // Long-poll: the reply is held until the session returns to READY
            // (or errors). The client sets its own read timeout; cap ours at
            // 30 minutes so a dead session can't pin the connection forever.
            match tokio::time::timeout(Duration::from_secs(1800), resp_rx).await {
                Ok(Ok(response)) => {
                    let _ = writer
                        .send(serde_json::to_string(&response).unwrap_or_default() + "\n")
                        .await;
                }
                _ => {
                    let err = serde_json::json!({"error": "input_wait timeout"});
                    let _ = writer
                        .send(serde_json::to_string(&err).unwrap_or_default() + "\n")
                        .await;
                }
            }
        }
        Message::Query { what } => {
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
        _ => {}
    }
}

fn build_state_legacy_json(
    state: &SessionState,
    detail: Option<&str>,
    session_id: Option<usize>,
    session_name: Option<&str>,
) -> serde_json::Value {
    let mut v = serde_json::json!({"type": "state", "state": state.label()});
    if let Some(d) = detail {
        v["detail"] = d.into();
    }
    if let Some(sid) = session_id {
        v["session_id"] = sid.into();
    }
    if let Some(name) = session_name {
        v["session_name"] = name.into();
    }
    v
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
        assert_eq!(
            parse_session_state("Thinking"),
            Some(SessionState::Thinking)
        );
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
        let result = read_limited_line(&mut reader, 64, &mut line).await.unwrap();
        assert_eq!(result, ReadResult::Line);
        assert_eq!(line, "{\"ok\":true}\n");
    }

    #[tokio::test]
    async fn read_limited_line_reports_oversize_and_truncated_line_when_oversize() {
        let (mut writer, reader) = tokio::io::duplex(64);
        writer.write_all(b"abcdef\n").await.unwrap();
        drop(writer);
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        let result = read_limited_line(&mut reader, 3, &mut line).await.unwrap();
        assert_eq!(result, ReadResult::OversizeDrained);
        assert_eq!(line, "abc");
    }

    #[tokio::test]
    async fn read_limited_line_reports_oversize_and_recovers_at_next_line() {
        let (reader, mut writer) = tokio::io::duplex(64);
        writer.write_all(b"abcdef\nok\n").await.unwrap();
        drop(writer);
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        assert_eq!(
            read_limited_line(&mut reader, 3, &mut line).await.unwrap(),
            ReadResult::OversizeDrained
        );
        assert_eq!(line, "abc");
        line.clear();
        assert_eq!(
            read_limited_line(&mut reader, 3, &mut line).await.unwrap(),
            ReadResult::Line
        );
        assert_eq!(line, "ok\n");
    }

    #[tokio::test]
    async fn read_limited_line_reports_oversize_when_eof_precedes_newline() {
        let (reader, mut writer) = tokio::io::duplex(64);
        writer.write_all(b"abcdef").await.unwrap();
        drop(writer);
        let mut reader = tokio::io::BufReader::new(reader);
        let mut line = String::new();
        assert_eq!(
            read_limited_line(&mut reader, 3, &mut line).await.unwrap(),
            ReadResult::OversizeDrained
        );
        assert_eq!(line, "abc");
    }

    #[tokio::test]
    async fn dispatch_msg_routes_named_state_to_ipc_named_action() {
        let (tx, mut rx) = mpsc::channel(1);
        let (writer, _writer_rx) = mpsc::channel(1);
        let env = Envelope {
            id: None,
            msg: Message::State {
                state: SessionState::Ready,
                detail: None,
                session_id: None,
                session_name: Some("reviewer".to_string()),
            },
        };
        dispatch_msg(env, &tx, None, &crate::auth::operator_caps(), &writer).await;
        match rx.recv().await.unwrap() {
            AppEvent::IpcNamedAction { session_name, msg } => {
                assert_eq!(session_name, "reviewer");
                assert_eq!(msg["state"], "READY");
            }
            _ => panic!("expected named action"),
        }
    }

    #[tokio::test]
    async fn dispatch_msg_uses_registered_id_for_state_and_tokens() {
        let (tx, mut rx) = mpsc::channel(3);
        let (writer, _writer_rx) = mpsc::channel(1);
        dispatch_msg(
            Envelope {
                id: None,
                msg: Message::State {
                    state: SessionState::Running,
                    detail: Some("busy".to_string()),
                    session_id: None,
                    session_name: None,
                },
            },
            &tx,
            Some(9),
            &crate::auth::worker_caps(),
            &writer,
        )
        .await;
        dispatch_msg(
            Envelope {
                id: None,
                msg: Message::Tokens {
                    input: 3,
                    output: 4,
                    cost: 1.5,
                    session_id: None,
                    session_name: None,
                },
            },
            &tx,
            Some(9),
            &crate::auth::worker_caps(),
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
    async fn dispatch_msg_rejects_cross_session_targeting_without_signal_any() {
        let (tx, mut rx) = mpsc::channel(1);
        let (writer, mut writer_rx) = mpsc::channel(4);
        let worker = crate::auth::worker_caps();

        // Worker registered as session 3 targeting session 7 → Forbidden.
        dispatch_msg(
            Envelope {
                id: Some(1),
                msg: Message::State {
                    state: SessionState::Ready,
                    detail: None,
                    session_id: Some(7),
                    session_name: None,
                },
            },
            &tx,
            Some(3),
            &worker,
            &writer,
        )
        .await;
        let err = writer_rx.recv().await.unwrap();
        assert!(err.contains("forbidden"), "expected forbidden, got: {err}");
        assert!(rx.try_recv().is_err(), "no app event should be emitted");

        // Named targeting is also privileged.
        dispatch_msg(
            Envelope {
                id: Some(2),
                msg: Message::Output {
                    line: "spoofed".into(),
                    session_id: None,
                    session_name: Some("reviewer".into()),
                },
            },
            &tx,
            Some(3),
            &worker,
            &writer,
        )
        .await;
        let err = writer_rx.recv().await.unwrap();
        assert!(err.contains("forbidden"), "expected forbidden, got: {err}");
        assert!(rx.try_recv().is_err(), "no app event should be emitted");

        // Explicitly targeting your own registered session is allowed.
        dispatch_msg(
            Envelope {
                id: None,
                msg: Message::State {
                    state: SessionState::Ready,
                    detail: None,
                    session_id: Some(3),
                    session_name: None,
                },
            },
            &tx,
            Some(3),
            &worker,
            &writer,
        )
        .await;
        assert!(matches!(
            rx.recv().await.unwrap(),
            AppEvent::IpcStateOverride { session_id: 3, .. }
        ));
    }

    #[tokio::test]
    async fn dispatch_msg_allows_cross_session_targeting_with_signal_any() {
        let (tx, mut rx) = mpsc::channel(1);
        let (writer, _writer_rx) = mpsc::channel(1);
        dispatch_msg(
            Envelope {
                id: None,
                msg: Message::Tokens {
                    input: 1,
                    output: 2,
                    cost: 0.1,
                    session_id: Some(7),
                    session_name: None,
                },
            },
            &tx,
            Some(3),
            &crate::auth::operator_caps(),
            &writer,
        )
        .await;
        assert!(matches!(
            rx.recv().await.unwrap(),
            AppEvent::IpcTokenUpdate { session_id: 7, .. }
        ));
    }

    #[tokio::test]
    async fn dispatch_msg_routes_pipe_broadcast_and_group_fire_messages() {
        let (tx, mut rx) = mpsc::channel(4);
        let (writer, _writer_rx) = mpsc::channel(1);
        dispatch_msg(
            Envelope {
                id: None,
                msg: Message::FirePipe {
                    source: None,
                    dest: None,
                    source_group: Some("agents".to_string()),
                },
            },
            &tx,
            None,
            &crate::auth::operator_caps(),
            &writer,
        )
        .await;
        dispatch_msg(
            Envelope {
                id: None,
                msg: Message::FirePipe {
                    source: Some(1),
                    dest: Some(2),
                    source_group: None,
                },
            },
            &tx,
            None,
            &crate::auth::operator_caps(),
            &writer,
        )
        .await;
        dispatch_msg(
            Envelope {
                id: None,
                msg: Message::Broadcast {
                    group: "agents".to_string(),
                    message: serde_json::json!({"hello": true}),
                },
            },
            &tx,
            None,
            &crate::auth::operator_caps(),
            &writer,
        )
        .await;
        dispatch_msg(
            Envelope {
                id: None,
                msg: Message::PipeAdd {
                    source: 1,
                    dest: 2,
                    trigger: "manual".to_string(),
                    extract: "diff".to_string(),
                    prefix: Some("p".to_string()),
                },
            },
            &tx,
            None,
            &crate::auth::operator_caps(),
            &writer,
        )
        .await;
        assert!(matches!(rx.recv().await.unwrap(),
            AppEvent::IpcGroupFire { source_group } if source_group == "agents"));
        assert!(matches!(
            rx.recv().await.unwrap(),
            AppEvent::IpcFirePipe {
                source: 1,
                dest: Some(2)
            }
        ));
        assert!(matches!(rx.recv().await.unwrap(),
            AppEvent::IpcBroadcast { group, msg } if group == "agents" && msg["hello"] == true));
        assert!(matches!(rx.recv().await.unwrap(),
            AppEvent::IpcPipeAdd { source: 1, dest: 2, trigger, extract, prefix }
                if trigger == "manual" && extract == "diff" && prefix.as_deref() == Some("p")));
    }
}
