use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

// ── SwappableWriter ────────────────────────────────────────────────────────
// Allows the ratatui terminal backend to be redirected from stdout to a
// relay socket at runtime, without reconstructing the Terminal object.

pub type WriterBox = Box<dyn Write + Send + 'static>;

pub struct SwappableWriter {
    handle: Arc<Mutex<WriterBox>>,
}

impl SwappableWriter {
    pub fn with_stdout() -> (Self, Arc<Mutex<WriterBox>>) {
        let handle: Arc<Mutex<WriterBox>> = Arc::new(Mutex::new(Box::new(std::io::stdout())));
        let sw = Self {
            handle: Arc::clone(&handle),
        };
        (sw, handle)
    }
}

impl Write for SwappableWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.handle.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.handle.lock().unwrap().flush()
    }
}

// ── Session / reattach file ────────────────────────────────────────────────

fn linkshell_config_dir() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(std::path::PathBuf::from(p).join("linkshell"));
    }
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|h| h.join(".config").join("linkshell"))
}

fn reattach_info_path() -> Option<std::path::PathBuf> {
    linkshell_config_dir().map(|d| d.join("reattach"))
}

pub fn reattach_socket_from_ipc(ipc_socket: &str) -> String {
    // Derive the reattach socket alongside the IPC socket.
    if let Some(stem) = ipc_socket.strip_suffix(".sock") {
        format!("{stem}.reattach")
    } else {
        format!("{ipc_socket}.reattach")
    }
}

pub fn write_reattach_info(pid: u32, ipc_socket: &str, token: &str) {
    let Some(path) = reattach_info_path() else {
        return;
    };
    let info = serde_json::json!({
        "pid": pid,
        "socket": ipc_socket,
        "reattach": reattach_socket_from_ipc(ipc_socket),
        "token": token,
    });
    // The file carries the reattach token; keep it readable by the owner only.
    if std::fs::write(&path, info.to_string()).is_ok() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

pub fn clear_reattach_info() {
    if let Some(path) = reattach_info_path() {
        let _ = std::fs::remove_file(path);
    }
}

// ── Relay client: `linkshell -r` / `linkshell --reattach` ─────────────────

pub async fn run_relay_client() -> anyhow::Result<()> {
    use crossterm::{
        event::{self, DisableMouseCapture, EnableMouseCapture},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    };

    // ── Find the detached session ─────────────────────────────────────────
    let info_path =
        reattach_info_path().ok_or_else(|| anyhow::anyhow!("cannot determine config directory"))?;

    let info_bytes = std::fs::read(&info_path).map_err(|_| {
        anyhow::anyhow!(
            "no detached linkshell session found\n  (expected {})",
            info_path.display()
        )
    })?;

    let info: serde_json::Value = serde_json::from_slice(&info_bytes)?;
    let pid = info["pid"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("invalid reattach file: missing pid"))? as i32;
    let reattach_socket = info["reattach"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("invalid reattach file: missing socket"))?
        .to_string();
    let token = info["token"]
        .as_str()
        .ok_or_else(|| {
            anyhow::anyhow!("invalid reattach file: missing token (session started by an older linkshell?)")
        })?
        .to_string();

    // ── Check the process is alive ────────────────────────────────────────
    let alive = unsafe { libc::kill(pid, 0) == 0 };
    if !alive {
        let _ = std::fs::remove_file(&info_path);
        anyhow::bail!("no active linkshell session (pid {} is not running)", pid);
    }

    // ── Connect ───────────────────────────────────────────────────────────
    let (cols, rows) = crossterm::terminal::size()?;
    let stream = UnixStream::connect(&reattach_socket).await.map_err(|e| {
        anyhow::anyhow!(
            "cannot connect to linkshell reattach socket ({}): {}",
            reattach_socket,
            e
        )
    })?;
    let (read_half, mut write_half) = stream.into_split();

    // Handshake: tell the server our terminal dimensions.
    let handshake = format!(
        "{}\n",
        serde_json::json!({"type":"reattach","token":token,"rows":rows,"cols":cols})
    );
    write_half.write_all(handshake.as_bytes()).await?;

    // Wait for the server's verdict before taking over the terminal.
    let (read_half, ack) = {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(read_half);
        let mut ack = String::new();
        reader.read_line(&mut ack).await?;
        (reader, ack)
    };
    let ack: serde_json::Value = serde_json::from_str(ack.trim())
        .map_err(|_| anyhow::anyhow!("unexpected reattach handshake response"))?;
    if ack["ok"].as_bool() != Some(true) {
        anyhow::bail!(
            "reattach rejected: {}",
            ack["error"].as_str().unwrap_or("unknown error")
        );
    }

    // ── Set up our terminal ───────────────────────────────────────────────
    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;

    let restore = || {
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            LeaveAlternateScreen,
            DisableMouseCapture,
            crossterm::cursor::Show
        );
    };

    // ── Task A: server terminal bytes → our stdout ────────────────────────
    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
    let done_tx2 = done_tx.clone();
    tokio::spawn(async move {
        let mut reader = read_half;
        let mut buf = vec![0u8; 4096];
        let mut stdout = tokio::io::stdout();
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdout.write_all(&buf[..n]).await.is_err() || stdout.flush().await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = done_tx2.send(()).await;
    });

    // ── Task B: our crossterm events → server (JSON lines) ────────────────
    // Run crossterm's blocking event::read() on a thread so we don't block
    // the tokio executor.
    let (ev_tx, mut ev_rx) = mpsc::channel::<Vec<u8>>(64);
    let done_tx3 = done_tx.clone();
    tokio::task::spawn_blocking(move || {
        loop {
            let Ok(ev) = event::read() else { break };
            let bytes = encode_relay_event(&ev);
            if bytes.is_empty() {
                continue;
            }
            if ev_tx.blocking_send(bytes).is_err() {
                break;
            }
        }
        let _ = done_tx3.blocking_send(());
    });

    // ── Main relay pump ───────────────────────────────────────────────────
    loop {
        tokio::select! {
            data = ev_rx.recv() => {
                match data {
                    Some(bytes) => {
                        if write_half.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = done_rx.recv() => break,
        }
    }

    restore();
    Ok(())
}

/// Encode a crossterm Event as a compact JSON line for the relay protocol.
/// Returns empty Vec for events we don't forward.
fn encode_relay_event(event: &crossterm::event::Event) -> Vec<u8> {
    // crossterm 0.27 with feature "serde" provides Serialize on KeyEvent and
    // MouseEvent but not on the top-level Event enum, so we tag manually.
    let json = match event {
        crossterm::event::Event::Key(k) => match serde_json::to_value(k) {
            Ok(v) => serde_json::json!({"t":"k","e":v}),
            Err(_) => return vec![],
        },
        crossterm::event::Event::Mouse(m) => match serde_json::to_value(m) {
            Ok(v) => serde_json::json!({"t":"m","e":v}),
            Err(_) => return vec![],
        },
        crossterm::event::Event::Resize(cols, rows) => {
            serde_json::json!({"t":"r","cols":cols,"rows":rows})
        }
        crossterm::event::Event::Paste(s) => {
            serde_json::json!({"t":"p","s":s})
        }
        _ => return vec![],
    };
    let mut bytes = json.to_string().into_bytes();
    bytes.push(b'\n');
    bytes
}

/// Decode a relay JSON line into an AppEvent. Returns None for unknown lines.
pub fn decode_relay_line(line: &str) -> Option<crate::events::AppEvent> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    match v["t"].as_str()? {
        "k" => {
            let key: crossterm::event::KeyEvent = serde_json::from_value(v["e"].clone()).ok()?;
            Some(crate::events::AppEvent::Key(key))
        }
        "m" => {
            let m: crossterm::event::MouseEvent = serde_json::from_value(v["e"].clone()).ok()?;
            Some(crate::events::AppEvent::Mouse(m))
        }
        "r" => Some(crate::events::AppEvent::Resize),
        "p" => {
            let s = v["s"].as_str().unwrap_or("").to_string();
            Some(crate::events::AppEvent::Paste(s))
        }
        _ => None,
    }
}
