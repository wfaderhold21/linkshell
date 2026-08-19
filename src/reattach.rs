use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Bytes of kernel socket buffer we ask for on both ends of the relay.
///
/// The default unix-socket buffer is generous on Linux (~208 KiB) but tiny on
/// macOS/BSD (`net.local.stream.{send,recv}space`, 8 KiB). A single ratatui
/// frame for a wide terminal can exceed that on its own, so a burst of PTY
/// output would fill the pipe faster than a slow terminal emulator drains it,
/// stall the server's bounded relay writes, and trip the "client is wedged"
/// escalation into an involuntary detach. Widening the buffer keeps bursts in
/// the kernel where they belong.
const RELAY_SOCK_BUF: libc::c_int = 512 * 1024;

/// Best-effort widening of a socket's send/receive buffers. Failure is fine:
/// the kernel clamps to `kern.ipc.maxsockbuf` and we simply keep the default.
pub fn widen_socket_buffers<F: std::os::unix::io::AsRawFd>(sock: &F) {
    let fd = sock.as_raw_fd();
    for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                &RELAY_SOCK_BUF as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
}

// ── SwappableWriter ────────────────────────────────────────────────────────
// Allows the ratatui terminal backend to be redirected from stdout to a
// relay socket at runtime, without reconstructing the Terminal object.

pub type WriterBox = Box<dyn Write + Send + 'static>;

pub struct SwappableWriter {
    handle: Arc<Mutex<WriterBox>>,
}

impl SwappableWriter {
    /// A writer that starts pointed at io::sink() — the server's initial
    /// state, before any relay client has attached.
    pub fn with_sink() -> (Self, Arc<Mutex<WriterBox>>) {
        let handle: Arc<Mutex<WriterBox>> = Arc::new(Mutex::new(Box::new(std::io::sink())));
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

// ── SizedBackend ───────────────────────────────────────────────────────────
// CrosstermBackend answers size() with an ioctl on this process's tty, which
// is the terminal linkshell was *started* in. After a detach/reattach the UI
// renders on the relay client's terminal, so that answer is stale — ratatui's
// per-draw autoresize would pin the layout to the original terminal's size
// forever. This wrapper delegates everything to CrosstermBackend except
// size()/window_size(), which report a shared value updated from resize
// events: the local terminal's while attached directly, the relay client's
// (handshake + forwarded resizes) while reattached.

/// Current terminal dimensions as (cols, rows).
pub type SharedSize = Arc<Mutex<(u16, u16)>>;

pub struct SizedBackend {
    inner: ratatui::backend::CrosstermBackend<SwappableWriter>,
    size: SharedSize,
}

impl SizedBackend {
    pub fn new(
        inner: ratatui::backend::CrosstermBackend<SwappableWriter>,
        size: SharedSize,
    ) -> Self {
        Self { inner, size }
    }
}

impl ratatui::backend::Backend for SizedBackend {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor(&mut self) -> io::Result<(u16, u16)> {
        self.inner.get_cursor()
    }
    fn set_cursor(&mut self, x: u16, y: u16) -> io::Result<()> {
        self.inner.set_cursor(x, y)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }
    fn size(&self) -> io::Result<ratatui::layout::Rect> {
        let (cols, rows) = *self.size.lock().unwrap();
        Ok(ratatui::layout::Rect::new(0, 0, cols, rows))
    }
    fn window_size(&mut self) -> io::Result<ratatui::backend::WindowSize> {
        let (cols, rows) = *self.size.lock().unwrap();
        Ok(ratatui::backend::WindowSize {
            columns_rows: ratatui::layout::Size {
                width: cols,
                height: rows,
            },
            pixels: ratatui::layout::Size::default(),
        })
    }
    fn flush(&mut self) -> io::Result<()> {
        ratatui::backend::Backend::flush(&mut self.inner)
    }
}

// ── Session registry ───────────────────────────────────────────────────────
// Each detached server records itself as a JSON entry under
// `<config>/sessions/<id>.json`, screen-style. Multiple servers coexist, each
// with its own id, pid, and per-instance sockets. `linkshell ls` enumerates
// the registry (pruning entries whose pid has died); `linkshell -r <id>`
// attaches to a specific one.

use serde::{Deserialize, Serialize};

/// A registered detached linkshell server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub pid: u32,
    /// IPC socket path.
    pub socket: String,
    /// Reattach (relay) socket path.
    pub reattach: String,
    pub token: String,
    /// Unix timestamp (seconds) when the server started.
    #[serde(default)]
    pub created: u64,
    /// Best-effort: whether a relay client is currently attached. Updated by
    /// the server on attach/detach; may be stale if the server crashed.
    #[serde(default)]
    pub attached: bool,
}

fn linkshell_config_dir() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(std::path::PathBuf::from(p).join("linkshell"));
    }
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|h| h.join(".config").join("linkshell"))
}

fn sessions_dir() -> Option<std::path::PathBuf> {
    linkshell_config_dir().map(|d| d.join("sessions"))
}

fn session_entry_path(id: &str) -> Option<std::path::PathBuf> {
    sessions_dir().map(|d| d.join(format!("{id}.json")))
}

/// True if `pid` is a live process owned by (signalable by) this user.
pub fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// A short, filesystem-safe session id. Derived from the wall clock and pid so
/// concurrent launches don't collide.
pub fn new_session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mix = nanos ^ ((std::process::id() as u128) << 17);
    // 6 base36 characters — plenty of entropy for a per-user session table.
    let mut n = mix;
    let mut s = String::new();
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    for _ in 0..6 {
        s.push(ALPHABET[(n % 36) as usize] as char);
        n /= 36;
    }
    s
}

pub fn reattach_socket_from_ipc(ipc_socket: &str) -> String {
    // Derive the reattach socket alongside the IPC socket.
    if let Some(stem) = ipc_socket.strip_suffix(".sock") {
        format!("{stem}.reattach")
    } else {
        format!("{ipc_socket}.reattach")
    }
}

/// Write (or overwrite) a session's registry entry. The entry carries the
/// reattach token, so keep it readable by the owner only.
pub fn write_session_entry(entry: &SessionEntry) {
    let Some(dir) = sessions_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    let Some(path) = session_entry_path(&entry.id) else {
        return;
    };
    let Ok(json) = serde_json::to_string(entry) else {
        return;
    };
    if std::fs::write(&path, json).is_ok() {
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

/// Read one session entry by id (no liveness check).
pub fn read_session_entry(id: &str) -> Option<SessionEntry> {
    let path = session_entry_path(id)?;
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Update the `attached` flag on a session entry in place. Best-effort.
pub fn set_session_attached(id: &str, attached: bool) {
    if let Some(mut entry) = read_session_entry(id) {
        entry.attached = attached;
        write_session_entry(&entry);
    }
}

pub fn remove_session_entry(id: &str) {
    if let Some(path) = session_entry_path(id) {
        let _ = std::fs::remove_file(path);
    }
}

/// All live sessions, sorted by creation time. Entries whose pid has died are
/// pruned from disk as a side effect.
pub fn list_sessions() -> Vec<SessionEntry> {
    let Some(dir) = sessions_dir() else {
        return Vec::new();
    };
    let Ok(read_dir) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Ok(session) = serde_json::from_slice::<SessionEntry>(&bytes) else {
            // Unparseable entry — drop it so it stops cluttering the list.
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if pid_alive(session.pid) {
            out.push(session);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    out.sort_by_key(|s| s.created);
    out
}

fn server_log_path() -> Option<std::path::PathBuf> {
    linkshell_config_dir().map(|d| d.join("server.log"))
}

pub fn server_log_path_display() -> String {
    server_log_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.config/linkshell/server.log".into())
}

/// Open (create/truncate) the server log for the daemon's stderr.
pub fn open_server_log() -> Option<std::fs::File> {
    let path = server_log_path()?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::File::create(path).ok()
}

// ── Relay client: `linkshell -r` / `linkshell --reattach` ─────────────────

/// Put the terminal back the way we found it. Idempotent and infallible by
/// design — every step is best-effort because the callers that need it most
/// (panic hook, signal handler) cannot propagate an error anywhere useful.
fn restore_terminal(kitty: bool) {
    use crossterm::{event::DisableMouseCapture, execute, terminal::disable_raw_mode};

    let _ = disable_raw_mode();
    let mut stdout = std::io::stdout();
    if kitty {
        // The server pushes kitty flags on our terminal; pop them in case the
        // server-side restore didn't reach us (crash, abort).
        use crossterm::event::PopKeyboardEnhancementFlags;
        let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        stdout,
        crossterm::terminal::LeaveAlternateScreen,
        DisableMouseCapture,
        crossterm::cursor::Show
    );
}

/// Await a signal, or never resolve if the stream failed to register. Keeps the
/// `select!` arms below uniform. `Signal::recv` is cancel-safe, so rebuilding
/// this future each loop iteration loses nothing.
async fn wait_signal(sig: Option<&mut tokio::signal::unix::Signal>) {
    match sig {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending::<()>().await,
    }
}

/// Install a panic hook that restores the terminal before the panic report is
/// printed.
///
/// Without this, a panic anywhere in the client — or in the server-side render
/// path, which surfaces here as a dropped relay — leaves the user's shell in
/// raw mode inside the alternate screen with mouse reporting on: no echo, no
/// visible prompt, and escape garbage on every keystroke, recoverable only by
/// a blind `reset`. Restoring first also means the panic message lands on the
/// normal screen where it can actually be read and reported.
fn install_terminal_panic_hook(kitty: bool) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal(kitty);
        previous(info);
    }));
}

pub async fn run_relay_client(id: &str) -> anyhow::Result<()> {
    use crossterm::{
        event::{self, EnableMouseCapture},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen},
    };

    // ── Find the detached session ─────────────────────────────────────────
    let entry = read_session_entry(id).ok_or_else(|| {
        anyhow::anyhow!("no linkshell session '{}' found (try `linkshell ls`)", id)
    })?;
    let pid = entry.pid as i32;
    let reattach_socket = entry.reattach.clone();
    let token = entry.token.clone();

    // ── Check the process is alive ────────────────────────────────────────
    if !pid_alive(entry.pid) {
        remove_session_entry(id);
        anyhow::bail!("no active linkshell session (pid {} is not running)", pid);
    }

    // ── Probe terminal capabilities ───────────────────────────────────────
    // The kitty keyboard protocol probe writes a query and reads the reply
    // from stdin, which requires raw mode. Enable it for the probe; if the
    // handshake fails below we restore before bailing.
    let (cols, rows) = crossterm::terminal::size()?;
    enable_raw_mode()?;
    let kitty = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);

    // ── Connect ───────────────────────────────────────────────────────────
    let stream = UnixStream::connect(&reattach_socket).await.map_err(|e| {
        let _ = disable_raw_mode();
        anyhow::anyhow!(
            "cannot connect to linkshell reattach socket ({}): {}",
            reattach_socket,
            e
        )
    })?;
    widen_socket_buffers(&stream);
    let (read_half, mut write_half) = stream.into_split();

    // Handshake: tell the server our terminal dimensions.
    let handshake = format!(
        "{}\n",
        serde_json::json!({"type":"reattach","token":token,"rows":rows,"cols":cols,"kitty":kitty})
    );
    if let Err(e) = write_half.write_all(handshake.as_bytes()).await {
        let _ = disable_raw_mode();
        return Err(e.into());
    }

    // Wait for the server's verdict before taking over the terminal.
    let (read_half, ack) = {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(read_half);
        let mut ack = String::new();
        if let Err(e) = reader.read_line(&mut ack).await {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        (reader, ack)
    };
    let ack: serde_json::Value = serde_json::from_str(ack.trim()).map_err(|_| {
        let _ = disable_raw_mode();
        anyhow::anyhow!("unexpected reattach handshake response")
    })?;
    if ack["ok"].as_bool() != Some(true) {
        let _ = disable_raw_mode();
        anyhow::bail!(
            "reattach rejected: {}",
            ack["error"].as_str().unwrap_or("unknown error")
        );
    }

    // ── Set up our terminal ───────────────────────────────────────────────
    // Raw mode is already on (kitty probe). The server re-issues alternate
    // screen / mouse capture / kitty-push sequences over the relay on attach,
    // but we enter the alternate screen locally too so there is no flash of
    // shell content between connect and the server's first frame.
    // Arm the hook before we take over the screen, so every path from here on
    // is covered.
    install_terminal_panic_hook(kitty);
    if let Err(e) = execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture) {
        // Raw mode is already on from the kitty probe; don't hand the user back
        // a half-configured terminal.
        restore_terminal(kitty);
        return Err(e.into());
    }

    // ── Task A: server terminal bytes → our stdout ────────────────────────
    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
    let done_tx2 = done_tx.clone();
    tokio::spawn(async move {
        let mut reader = read_half;
        // Drain in large gulps: the faster this side empties the socket, the
        // less the server's bounded writes stall behind a slow emulator.
        let mut buf = vec![0u8; 64 * 1024];
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
        while let Ok(ev) = event::read() {
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
    // SIGTERM/SIGHUP would otherwise kill us mid-alternate-screen and leave the
    // terminal wrecked; treat them as an ordinary detach so `restore` runs.
    // Registration failure is not worth aborting an otherwise healthy attach,
    // and bailing here with `?` would skip the restore.
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).ok();
    let mut sighup = signal(SignalKind::hangup()).ok();
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
            _ = wait_signal(sigterm.as_mut()) => break,
            _ = wait_signal(sighup.as_mut()) => break,
        }
    }

    restore_terminal(kitty);

    // NOTE: the event-reader task is still parked in a blocking `event::read()`
    // on stdin and cannot be cancelled. Dropping the tokio runtime — which is
    // what returning from `main` does — waits for blocking tasks that have
    // already started, so the caller must `exit_after_detach` rather than fall
    // off the end of `main`, or the process hangs here until the user presses
    // one more key (which the dying reader then swallows).
    Ok(())
}

/// Terminate the client process, bypassing the tokio runtime shutdown that
/// would otherwise block on the parked stdin reader described above.
///
/// Nothing in the client owns unflushed state — stdout is flushed on every
/// relay write and the terminal restore executes synchronously — so this is
/// only skipping a wait we don't want.
pub fn exit_after_detach(result: anyhow::Result<()>) -> ! {
    use std::io::Write;
    let code = match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("linkshell: {:#}", e);
            1
        }
    };
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code)
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
        "r" => Some(crate::events::AppEvent::Resize {
            cols: v["cols"].as_u64()? as u16,
            rows: v["rows"].as_u64()? as u16,
        }),
        "p" => {
            let s = v["s"].as_str().unwrap_or("").to_string();
            Some(crate::events::AppEvent::Paste(s))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::Backend;

    #[test]
    fn session_ids_are_six_base36_chars() {
        let id = new_session_id();
        assert_eq!(id.len(), 6);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn reattach_socket_is_derived_from_ipc_socket() {
        assert_eq!(
            reattach_socket_from_ipc("/run/user/1000/linkshell/42.sock"),
            "/run/user/1000/linkshell/42.reattach"
        );
        assert_eq!(reattach_socket_from_ipc("/tmp/foo"), "/tmp/foo.reattach");
    }

    #[test]
    fn relay_resize_lines_carry_dimensions() {
        let ev = decode_relay_line(r#"{"t":"r","cols":120,"rows":40}"#).unwrap();
        match ev {
            crate::events::AppEvent::Resize { cols, rows } => assert_eq!((cols, rows), (120, 40)),
            _ => panic!("expected Resize"),
        }
    }

    #[test]
    fn sized_backend_reports_the_shared_size_not_the_tty() {
        let (swappable, _handle) = SwappableWriter::with_sink();
        let size: SharedSize = Arc::new(Mutex::new((80, 24)));
        let mut backend = SizedBackend::new(
            ratatui::backend::CrosstermBackend::new(swappable),
            Arc::clone(&size),
        );
        assert_eq!(
            backend.size().unwrap(),
            ratatui::layout::Rect::new(0, 0, 80, 24)
        );

        // A reattach from a larger terminal updates the shared value…
        *size.lock().unwrap() = (200, 60);
        // …and the backend (thus ratatui's autoresize) sees it immediately.
        assert_eq!(
            backend.size().unwrap(),
            ratatui::layout::Rect::new(0, 0, 200, 60)
        );
        assert_eq!(
            backend.window_size().unwrap().columns_rows,
            ratatui::layout::Size {
                width: 200,
                height: 60
            }
        );
    }
}
