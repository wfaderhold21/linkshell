use std::io::Write;
use std::os::unix::net::UnixStream;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let sock = std::env::var("LINKSHELL_SOCK")
        .unwrap_or_else(|_| "/tmp/linkshell.sock".to_string());
    let session_id: Option<u64> = std::env::var("LINKSHELL_SESSION_ID")
        .ok()
        .and_then(|s| s.parse().ok());

    let msg = build_message(&args, session_id).unwrap_or_else(|| {
        eprintln!("usage:");
        eprintln!("  linkshell-ctl state <READY|THINKING|RUNNING|WAITING|ERROR>");
        eprintln!("  linkshell-ctl pipe fire [src] [dst]");
        eprintln!("  linkshell-ctl output <text...>");
        std::process::exit(1);
    });

    let mut stream = UnixStream::connect(&sock).unwrap_or_else(|e| {
        eprintln!("linkshell-ctl: cannot connect to {}: {}", sock, e);
        std::process::exit(1);
    });

    let line = serde_json::to_string(&msg).unwrap() + "\n";
    stream.write_all(line.as_bytes()).unwrap_or_else(|e| {
        eprintln!("linkshell-ctl: write failed: {}", e);
        std::process::exit(1);
    });
}

fn build_message(args: &[String], session_id: Option<u64>) -> Option<serde_json::Value> {
    match args.get(1).map(|s| s.as_str()) {
        Some("state") => {
            let state = args.get(2)?.to_uppercase();
            let mut m = serde_json::json!({"type": "state", "state": state});
            if let Some(sid) = session_id {
                m["session_id"] = sid.into();
            }
            Some(m)
        }
        Some("pipe") if args.get(2).map(|s| s.as_str()) == Some("fire") => {
            // src defaults to LINKSHELL_SESSION_ID if not given
            let source: u64 = args.get(3)
                .and_then(|s| s.parse().ok())
                .or(session_id)
                .unwrap_or(0);
            let mut m = serde_json::json!({"type": "fire_pipe", "source": source});
            if let Some(dst) = args.get(4).and_then(|s| s.parse::<u64>().ok()) {
                m["dest"] = dst.into();
            }
            Some(m)
        }
        Some("output") => {
            let text = args[2..].join(" ");
            let mut m = serde_json::json!({"type": "output", "line": text});
            if let Some(sid) = session_id {
                m["session_id"] = sid.into();
            }
            Some(m)
        }
        _ => None,
    }
}
