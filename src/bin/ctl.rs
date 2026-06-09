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

fn send_only(sock: &str, msg: &serde_json::Value) {
    let mut stream = connect(sock);
    let line = serde_json::to_string(msg).unwrap() + "\n";
    stream.write_all(line.as_bytes()).unwrap_or_else(|e| {
        eprintln!("linkshell-ctl: write failed: {}", e);
        std::process::exit(1);
    });
}

fn send_and_recv(sock: &str, msg: &serde_json::Value, timeout: Duration) -> String {
    let stream = connect(sock);
    stream.set_read_timeout(Some(timeout)).ok();
    {
        let mut w = &stream;
        let line = serde_json::to_string(msg).unwrap() + "\n";
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
    eprintln!("  linkshell-ctl wait-ready <session_id> [--timeout=<secs>]");
    eprintln!("  linkshell-ctl state <READY|THINKING|RUNNING|WAITING|ERROR>");
    eprintln!("  linkshell-ctl output <text...>");
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
