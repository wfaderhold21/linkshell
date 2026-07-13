use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sock = resolve_socket();
    let session_id: Option<u64> = std::env::var("LINKSHELL_SESSION_ID")
        .ok()
        .and_then(|s| s.parse().ok());

    match args.get(1).map(|s| s.as_str()) {
        // ── Query commands (synchronous: send, read response, exit) ──────────
        Some("list") => {
            let msg = serde_json::json!({"type": "query", "what": "sessions"});
            let resp = send_and_recv(&sock, &msg, Duration::from_secs(5));
            println!("{}", resp);
        }

        Some("wait-ready") => {
            let session_arg = args
                .get(2)
                .and_then(|s| s.parse::<u64>().ok())
                .or(session_id)
                .unwrap_or_else(|| {
                    eprintln!("usage: linkshell-ctl wait-ready <session_id> [--timeout=<secs>]");
                    std::process::exit(1);
                });
            let timeout_secs: u64 = args
                .iter()
                .find_map(|a| a.strip_prefix("--timeout=").and_then(|v| v.parse().ok()))
                .unwrap_or(1200);
            let msg = serde_json::json!({
                "type": "session_input_wait",
                "session_id": session_arg,
                "text": "",
            });
            let resp = send_and_recv(&sock, &msg, Duration::from_secs(timeout_secs + 5));
            println!("{}", resp);
        }

        Some("new") => {
            // new <kind> [name] [--cwd=PATH]
            let kind = args.get(2).cloned().unwrap_or_else(|| {
                eprintln!("usage: linkshell-ctl new <kind> [name] [--cwd=PATH]");
                std::process::exit(1);
            });
            let mut name: Option<String> = None;
            let mut cwd: Option<String> = None;
            for a in &args[3..] {
                if let Some(v) = a.strip_prefix("--cwd=") {
                    cwd = Some(v.to_string());
                } else if name.is_none() {
                    name = Some(a.clone());
                }
            }
            let mut msg = serde_json::json!({"type": "session_create", "kind": kind});
            if let Some(n) = name {
                msg["name"] = n.into();
            }
            if let Some(c) = cwd {
                msg["cwd"] = c.into();
            }
            let resp = send_and_recv(&sock, &msg, Duration::from_secs(15));
            println!("{}", resp);
        }

        Some("input") => {
            // input <id> <text...> [--wait] [--timeout=S]
            let sid = args
                .get(2)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or_else(|| {
                    eprintln!("usage: linkshell-ctl input <id> <text...> [--wait] [--timeout=S]");
                    std::process::exit(1);
                });
            let mut wait = false;
            let mut timeout_secs: u64 = 1200;
            let mut words: Vec<String> = Vec::new();
            for a in &args[3..] {
                if a == "--wait" {
                    wait = true;
                } else if let Some(v) = a.strip_prefix("--timeout=") {
                    timeout_secs = v.parse().unwrap_or(timeout_secs);
                } else {
                    words.push(a.clone());
                }
            }
            if words.is_empty() {
                eprintln!("usage: linkshell-ctl input <id> <text...> [--wait] [--timeout=S]");
                std::process::exit(1);
            }
            let msg = serde_json::json!({
                "type": "session_input_wait",
                "session_id": sid,
                "text": words.join(" "),
            });
            if wait {
                let resp = send_and_recv(&sock, &msg, Duration::from_secs(timeout_secs + 5));
                println!("{}", resp);
            } else {
                // Fire-and-forget: the input is injected immediately; we just
                // don't stick around for the READY reply.
                send_only(&sock, &msg);
            }
        }

        Some("read") => {
            // read <id> [n]
            let sid = args
                .get(2)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or_else(|| {
                    eprintln!("usage: linkshell-ctl read <id> [n]");
                    std::process::exit(1);
                });
            let what = match args.get(3).and_then(|s| s.parse::<u64>().ok()) {
                Some(n) => format!("output:{}:{}", sid, n),
                None => format!("output:{}", sid),
            };
            let msg = serde_json::json!({"type": "query", "what": what});
            let resp = send_and_recv(&sock, &msg, Duration::from_secs(5));
            println!("{}", resp);
        }

        Some("kill") => {
            // kill <id> [reason...] — files a request; the user confirms in the TUI
            let sid = args
                .get(2)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or_else(|| {
                    eprintln!("usage: linkshell-ctl kill <id> [reason...]");
                    std::process::exit(1);
                });
            let mut msg = serde_json::json!({"type": "session_kill_request", "session_id": sid});
            let reason = args[3..].join(" ");
            if !reason.is_empty() {
                msg["reason"] = reason.into();
            }
            let resp = send_and_recv(&sock, &msg, Duration::from_secs(10));
            println!("{}", resp);
        }

        Some("chat") => {
            let text = args[2..].join(" ");
            if text.is_empty() {
                eprintln!("usage: linkshell-ctl chat <text...>");
                std::process::exit(1);
            }
            send_only(
                &sock,
                &serde_json::json!({"type": "chat_post", "text": text}),
            );
        }

        // ── Pipe sub-commands ────────────────────────────────────────────────
        Some("pipe") => match args.get(2).map(|s| s.as_str()) {
            Some("list") => {
                let msg = serde_json::json!({"type": "query", "what": "pipes"});
                let resp = send_and_recv(&sock, &msg, Duration::from_secs(5));
                println!("{}", resp);
            }
            Some("add") => {
                // pipe add <src> <dst> [--extract=X] [--trigger=X] [--prefix=X]
                let src = args
                    .get(3)
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or_else(|| {
                        eprintln!("pipe add: missing src");
                        std::process::exit(1);
                    });
                let dst = args
                    .get(4)
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or_else(|| {
                        eprintln!("pipe add: missing dst");
                        std::process::exit(1);
                    });
                let mut extract = "last-block".to_string();
                let mut trigger = "on_ready".to_string();
                let mut prefix: Option<String> = None;
                let flag_args = &args[5..];
                let mut i = 0;
                while i < flag_args.len() {
                    if let Some(v) = flag_args[i].strip_prefix("--extract=") {
                        extract = v.to_string();
                    } else if let Some(v) = flag_args[i].strip_prefix("--summarize=") {
                        extract = format!("summarize={}", v);
                    } else if let Some(v) = flag_args[i].strip_prefix("--trigger=") {
                        trigger = v.to_string();
                    } else if let Some(v) = flag_args[i].strip_prefix("--prefix=") {
                        let rest = flag_args[i + 1..].join(" ");
                        prefix = Some(if rest.is_empty() {
                            v.trim_matches('"').to_string()
                        } else {
                            format!("{} {}", v, rest).trim_matches('"').to_string()
                        });
                        break;
                    }
                    i += 1;
                }
                let mut msg = serde_json::json!({
                    "type":    "pipe_add",
                    "source":  src,
                    "dest":    dst,
                    "trigger": trigger,
                    "extract": extract,
                });
                if let Some(p) = prefix {
                    msg["prefix"] = p.into();
                }
                send_only(&sock, &msg);
            }
            Some("remove") => {
                // pipe remove <src> [dst]
                let src = args
                    .get(3)
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or_else(|| {
                        eprintln!("pipe remove: missing src");
                        std::process::exit(1);
                    });
                let mut msg = serde_json::json!({"type": "pipe_remove", "source": src});
                if let Some(dst) = args.get(4).and_then(|s| s.parse::<u64>().ok()) {
                    msg["dest"] = dst.into();
                }
                send_only(&sock, &msg);
            }
            Some("fire") => {
                let source: u64 = args
                    .get(3)
                    .and_then(|s| s.parse().ok())
                    .or(session_id)
                    .unwrap_or(0);
                let mut msg = serde_json::json!({"type": "fire_pipe", "source": source});
                if let Some(dst) = args.get(4).and_then(|s| s.parse::<u64>().ok()) {
                    msg["dest"] = dst.into();
                }
                send_only(&sock, &msg);
            }
            _ => usage(),
        },

        // ── Fire-and-forget commands ─────────────────────────────────────────
        Some("state") => {
            let state = match args.get(2) {
                Some(s) => s.to_uppercase(),
                None => usage(),
            };
            let mut msg = serde_json::json!({"type": "state", "state": state});
            if let Some(sid) = session_id {
                msg["session_id"] = sid.into();
            }
            send_only(&sock, &msg);
        }

        Some("output") => {
            let text = args[2..].join(" ");
            let mut msg = serde_json::json!({"type": "output", "line": text});
            if let Some(sid) = session_id {
                msg["session_id"] = sid.into();
            }
            send_only(&sock, &msg);
        }

        Some("send") => {
            let wait = args.get(2).map(|s| s.as_str()) == Some("--wait");
            let dest_idx = if wait { 3 } else { 2 };
            let dest = args.get(dest_idx).unwrap_or_else(|| {
                eprintln!("usage: linkshell-ctl send [--wait] <dest_name> <message...>");
                std::process::exit(1);
            });
            if args.len() <= dest_idx + 1 {
                eprintln!("usage: linkshell-ctl send [--wait] <dest_name> <message...>");
                std::process::exit(1);
            }
            let message = args[dest_idx + 1..].join(" ");
            let mut msg = serde_json::json!({
                "type": "agent_send",
                "dest": dest,
                "message": message,
            });
            if wait {
                msg["wait"] = true.into();
                let resp = send_and_recv(&sock, &msg, Duration::from_secs(5));
                println!("{}", resp);
            } else {
                send_only(&sock, &msg);
            }
        }

        _ => usage(),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn resolve_socket() -> String {
    std::env::var("LINKSHELL_SOCK")
        .ok()
        .or_else(read_last_socket)
        .unwrap_or_else(|| "/tmp/linkshell.sock".to_string())
}

fn read_last_socket() -> Option<String> {
    let path = linkshell_config_dir()?.join("last_socket");
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn linkshell_config_dir() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(std::path::PathBuf::from(path).join("linkshell"));
    }
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|home| home.join(".config").join("linkshell"))
}

fn connect(sock: &str) -> UnixStream {
    UnixStream::connect(sock).unwrap_or_else(|e| {
        eprintln!("linkshell-ctl: cannot connect to {}: {}", sock, e);
        std::process::exit(1);
    })
}

/// Send the Hello handshake and read the Welcome response.
fn do_handshake(stream: &UnixStream) {
    // Present this session's capability token if we were spawned by linkshell.
    // Without it a same-uid Unix peer is treated as the operator; with it the
    // connection is bound to the owning session and its granted capabilities.
    let mut hello_msg = serde_json::json!({"type": "hello", "protocol": 1});
    if let Ok(token) = std::env::var("LINKSHELL_TOKEN") {
        if !token.is_empty() {
            hello_msg["token"] = token.into();
        }
    }
    let hello = serde_json::json!({"msg": hello_msg});
    let hello_line = serde_json::to_string(&hello).unwrap() + "\n";
    {
        let mut w = stream;
        w.write_all(hello_line.as_bytes()).unwrap_or_else(|e| {
            eprintln!("linkshell-ctl: handshake write failed: {}", e);
            std::process::exit(1);
        });
    }
    // Read Welcome response (discard it — we just need the handshake to complete)
    let mut reader = BufReader::new(stream);
    let mut welcome = String::new();
    reader.read_line(&mut welcome).ok();
}

fn send_only(sock: &str, msg: &serde_json::Value) {
    let stream = connect(sock);
    // Perform Hello/Welcome handshake first.
    do_handshake(&stream);
    let wrapped = serde_json::json!({"msg": msg});
    let line = serde_json::to_string(&wrapped).unwrap() + "\n";
    let mut w = &stream;
    w.write_all(line.as_bytes()).unwrap_or_else(|e| {
        eprintln!("linkshell-ctl: write failed: {}", e);
        std::process::exit(1);
    });
}

fn send_and_recv(sock: &str, msg: &serde_json::Value, timeout: Duration) -> String {
    let stream = connect(sock);
    stream.set_read_timeout(Some(timeout)).ok();
    // Perform Hello/Welcome handshake first (no timeout on handshake read).
    do_handshake(&stream);
    {
        let mut w = &stream;
        let wrapped = serde_json::json!({"id": 1, "msg": msg});
        let line = serde_json::to_string(&wrapped).unwrap() + "\n";
        w.write_all(line.as_bytes()).unwrap_or_else(|e| {
            eprintln!("linkshell-ctl: write failed: {}", e);
            std::process::exit(1);
        });
    }
    let mut reader = BufReader::new(&stream);
    let mut resp = String::new();
    reader.read_line(&mut resp).unwrap_or_else(|e| {
        eprintln!("linkshell-ctl: read failed: {}", e);
        std::process::exit(1);
    });
    resp.trim().to_string()
}

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  linkshell-ctl list");
    eprintln!("  linkshell-ctl new <kind> [name] [--cwd=PATH]");
    eprintln!("  linkshell-ctl input <id> <text...> [--wait] [--timeout=<secs>]");
    eprintln!("  linkshell-ctl read <id> [n]");
    eprintln!("  linkshell-ctl kill <id> [reason...]   (asks the user to confirm)");
    eprintln!("  linkshell-ctl chat <text...>");
    eprintln!("  linkshell-ctl wait-ready <session_id> [--timeout=<secs>]");
    eprintln!("  linkshell-ctl state <READY|THINKING|RUNNING|WAITING|ERROR>");
    eprintln!("  linkshell-ctl output <text...>");
    eprintln!("  linkshell-ctl send [--wait] <dest_name> <message...>");
    eprintln!("  linkshell-ctl pipe list");
    eprintln!("  linkshell-ctl pipe add <src> <dst> [--extract=X] [--trigger=X] [--prefix=X]");
    eprintln!("  linkshell-ctl pipe remove <src> [dst]");
    eprintln!("  linkshell-ctl pipe fire [src] [dst]");
    eprintln!();
    eprintln!("environment variables:");
    eprintln!(
        "  LINKSHELL_SOCK        IPC socket path; defaults to last daemon socket if available"
    );
    eprintln!("  LINKSHELL_SESSION_ID  default session id for session-scoped commands");
    std::process::exit(1);
}
