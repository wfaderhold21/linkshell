use std::time::Duration;
use tokio::sync::mpsc;

use crate::events::AppEvent;
use crate::patterns::PatternMatcher;
use crate::session::{Session, SessionKind, SessionState, MAX_SESSIONS, PTY_ROWS, PTY_COLS};

#[derive(Debug, Clone, PartialEq)]
pub enum NewSessionField {
    Kind,
    Name,
    Cwd,
    CustomCmd,
}

#[derive(Debug, Clone)]
pub struct NewSessionState {
    pub selected_kind: usize,
    pub name: String,
    pub cwd: String,
    pub custom_cmd: String,
    pub active_field: NewSessionField,
}

impl Default for NewSessionState {
    fn default() -> Self {
        Self {
            selected_kind: 0,
            name: String::new(),
            cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "~".to_string()),
            custom_cmd: String::new(),
            active_field: NewSessionField::Kind,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum AppMode {
    Normal,
    NewSession,
    CommandBar,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub active_idx: Option<usize>,
    pub mode: AppMode,
    pub new_session_state: NewSessionState,
    pub command_input: String,
    pub should_quit: bool,
    pub event_tx: mpsc::Sender<AppEvent>,
    matcher: PatternMatcher,
    next_id: usize,
}

impl App {
    pub fn new(event_tx: mpsc::Sender<AppEvent>) -> Self {
        Self {
            sessions: Vec::new(),
            active_idx: None,
            mode: AppMode::Normal,
            new_session_state: NewSessionState::default(),
            command_input: String::new(),
            should_quit: false,
            event_tx,
            matcher: PatternMatcher::new(),
            next_id: 0,
        }
    }

    pub fn active_session(&self) -> Option<&Session> {
        self.active_idx.and_then(|i| self.sessions.get(i))
    }

    pub fn active_session_mut(&mut self) -> Option<&mut Session> {
        self.active_idx.and_then(|i| self.sessions.get_mut(i))
    }

    // ── Session management ─────────────────────────────────────────────────

    pub fn spawn_session(
        &mut self,
        kind: SessionKind,
        name: String,
        cwd: String,
    ) -> anyhow::Result<()> {
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(anyhow::anyhow!("Maximum {} sessions reached", MAX_SESSIONS));
        }

        let id = self.next_id;
        self.next_id += 1;

        let session_name = if name.is_empty() {
            format!("{}-{}", kind.label(), id + 1)
        } else {
            name
        };

        let session = Session::new(id, session_name, kind.clone(), cwd.clone());
        let idx = self.sessions.len();
        self.sessions.push(session);

        if self.active_idx.is_none() {
            self.active_idx = Some(idx);
        }

        let tx = self.event_tx.clone();
        let cmd_str = kind.command();

        tokio::spawn(async move {
            if let Err(e) = run_pty(id, cmd_str, cwd, tx.clone()).await {
                let _ = tx.send(AppEvent::SessionOutput {
                    session_id: id,
                    line: format!("[error: {}]", e),
                }).await;
                let _ = tx.send(AppEvent::SessionDied { session_id: id }).await;
            }
        });

        Ok(())
    }

    pub fn kill_active_session(&mut self) {
        if let Some(idx) = self.active_idx {
            if idx < self.sessions.len() {
                self.sessions[idx].pty_writer = None;
                self.sessions[idx].state = SessionState::Dead;
            }
        }
    }

    pub fn switch_to(&mut self, idx: usize) {
        if idx < self.sessions.len() {
            self.active_idx = Some(idx);
        }
    }

    pub fn next_session(&mut self) {
        let n = self.sessions.len();
        if n == 0 { return; }
        self.active_idx = Some(self.active_idx.map_or(0, |i| (i + 1) % n));
    }

    pub fn prev_session(&mut self) {
        let n = self.sessions.len();
        if n == 0 { return; }
        self.active_idx = Some(self.active_idx.map_or(0, |i| if i == 0 { n - 1 } else { i - 1 }));
    }

    // ── Event handlers ─────────────────────────────────────────────────────

    pub fn handle_session_writer(&mut self, session_id: usize, writer_tx: mpsc::Sender<Vec<u8>>) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            s.pty_writer = Some(writer_tx);
            s.state = SessionState::Ready;
        }
    }

    pub fn handle_session_bytes(&mut self, session_id: usize, data: Vec<u8>) {
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.process_bytes(&data);
        }
    }

    pub fn handle_session_output(&mut self, session_id: usize, line: String) {
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            let stripped = strip_ansi(&line);
            if let Some(new_state) = self.matcher.infer_state(&stripped, &session.kind) {
                session.state = new_state;
            }
            if let Some(stats) = self.matcher.parse_tokens(&stripped) {
                session.stats = stats;
            }
        }
    }

    pub fn handle_session_current_line(&mut self, session_id: usize, text: String) {
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            let stripped = strip_ansi(&text);
            if let Some(new_state) = self.matcher.infer_state(&stripped, &session.kind) {
                session.state = new_state;
            }
        }
    }

    pub fn handle_session_died(&mut self, session_id: usize) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            s.state = SessionState::Dead;
        }
    }

    pub fn handle_tick(&mut self) {
        for session in self.sessions.iter_mut() {
            if let Some(last) = session.last_output_at {
                if last.elapsed() > Duration::from_secs(2)
                    && matches!(session.state, SessionState::Running | SessionState::Thinking)
                {
                    session.state = SessionState::Ready;
                }
            }
        }
    }

    // ── New session dialog ─────────────────────────────────────────────────

    pub fn open_new_session(&mut self) {
        self.new_session_state = NewSessionState::default();
        self.mode = AppMode::NewSession;
    }

    pub fn new_session_tab(&mut self) {
        use NewSessionField::*;
        let is_custom = self.new_session_state.selected_kind == 3;
        self.new_session_state.active_field = match self.new_session_state.active_field {
            Kind      => Name,
            Name      => Cwd,
            Cwd       => if is_custom { CustomCmd } else { Kind },
            CustomCmd => Kind,
        };
    }

    pub fn new_session_input(&mut self, c: char) {
        use NewSessionField::*;
        match self.new_session_state.active_field {
            Kind      => {}
            Name      => self.new_session_state.name.push(c),
            Cwd       => self.new_session_state.cwd.push(c),
            CustomCmd => self.new_session_state.custom_cmd.push(c),
        }
    }

    pub fn new_session_backspace(&mut self) {
        use NewSessionField::*;
        match self.new_session_state.active_field {
            Kind      => {}
            Name      => { self.new_session_state.name.pop(); }
            Cwd       => { self.new_session_state.cwd.pop(); }
            CustomCmd => { self.new_session_state.custom_cmd.pop(); }
        }
    }

    pub fn new_session_select_kind(&mut self, delta: i32) {
        let cur = self.new_session_state.selected_kind as i32;
        self.new_session_state.selected_kind = ((cur + delta).rem_euclid(4)) as usize;
        self.new_session_state.active_field = NewSessionField::Kind;
    }

    pub fn confirm_new_session(&mut self) -> anyhow::Result<()> {
        let ns = self.new_session_state.clone();
        let kind = match ns.selected_kind {
            0 => SessionKind::Claude,
            1 => SessionKind::Codex,
            2 => SessionKind::Shell,
            _ => SessionKind::Custom(ns.custom_cmd.clone()),
        };
        let cwd = if ns.cwd.is_empty() {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "~".to_string())
        } else {
            ns.cwd.clone()
        };
        self.spawn_session(kind, ns.name, cwd)?;
        self.mode = AppMode::Normal;
        Ok(())
    }

    // ── Command bar ────────────────────────────────────────────────────────

    pub fn open_command_bar(&mut self) {
        self.command_input.clear();
        self.mode = AppMode::CommandBar;
    }

    pub fn command_input_char(&mut self, c: char) {
        self.command_input.push(c);
    }

    pub fn command_backspace(&mut self) {
        self.command_input.pop();
    }

    pub fn execute_command(&mut self) {
        let cmd = self.command_input.trim().to_string();
        self.mode = AppMode::Normal;
        self.command_input.clear();

        let parts: Vec<&str> = cmd.splitn(3, ' ').collect();
        match parts.as_slice() {
            ["new", kind_str, rest @ ..] => {
                let name = rest.first().unwrap_or(&"").to_string();
                let kind = match *kind_str {
                    "claude" => SessionKind::Claude,
                    "codex"  => SessionKind::Codex,
                    "shell"  => SessionKind::Shell,
                    other    => SessionKind::Custom(other.to_string()),
                };
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "~".to_string());
                let _ = self.spawn_session(kind, name, cwd);
            }
            ["kill"] => self.kill_active_session(),
            ["kill", n] => {
                if let Ok(idx) = n.parse::<usize>() {
                    if idx >= 1 && idx <= self.sessions.len() {
                        let i = idx - 1;
                        self.sessions[i].pty_writer = None;
                        self.sessions[i].state = SessionState::Dead;
                    }
                }
            }
            ["quit"] | ["q"] => self.should_quit = true,
            _ => {}
        }
    }

    // ── Write to active PTY ────────────────────────────────────────────────

    pub fn write_to_active(&self, data: &[u8]) {
        if let Some(session) = self.active_session() {
            session.write_bytes(data.to_vec());
        }
    }
}

// ── PTY runner task ────────────────────────────────────────────────────────

async fn run_pty(
    session_id: usize,
    cmd: String,
    cwd: String,
    tx: mpsc::Sender<AppEvent>,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let pty = pty_process::Pty::new()?;
    pty.resize(pty_process::Size::new(PTY_ROWS, PTY_COLS))?;
    let pts = pty.pts()?;

    let mut command = pty_process::Command::new(&cmd);
    command.current_dir(&cwd);
    let _child = command.spawn(&pts)?;

    let (read_half, mut write_half) = pty.into_split();

    // Channel for bytes coming from App → PTY
    let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(64);

    // Notify App of the write channel
    tx.send(AppEvent::SessionWriter {
        session_id,
        writer_tx: write_tx,
    }).await?;

    // Writer task: relay bytes from App into the PTY
    tokio::spawn(async move {
        while let Some(bytes) = write_rx.recv().await {
            if write_half.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    // Reader: split PTY output into lines, handling \r\n, \n, and bare \r.
    // Bare \r (carriage return without \n) means "overwrite current line" — used
    // by spinners and progress bars. Partial lines (prompts without \n) are sent
    // as SessionCurrentLine so they appear without creating a new buffer entry.
    use tokio::io::AsyncReadExt;
    let mut reader = read_half;
    let mut buf = [0u8; 4096];
    let mut pending = String::new();

    loop {
        match tokio::time::timeout(
            std::time::Duration::from_millis(20),
            reader.read(&mut buf),
        ).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let data = buf[..n].to_vec();
                // Raw bytes → vt100 screen buffer for display
                if tx.send(AppEvent::SessionBytes { session_id, data: data.clone() }).await.is_err() {
                    return Ok(());
                }
                // Line splitting → state inference only
                pending.push_str(&String::from_utf8_lossy(&data));
                let (complete, partial) = split_pty_lines(&pending);
                pending = partial;
                for line in complete {
                    if tx.send(AppEvent::SessionOutput { session_id, line }).await.is_err() {
                        return Ok(());
                    }
                }
            }
            Ok(Err(_)) => break,
            // Timeout: update current-line display without adding to the line buffer
            Err(_) => {
                if tx.send(AppEvent::SessionCurrentLine {
                    session_id,
                    text: pending.clone(),
                }).await.is_err() {
                    return Ok(());
                }
            }
        }
    }

    let _ = tx.send(AppEvent::SessionDied { session_id }).await;
    Ok(())
}

/// Split PTY bytes into complete lines and a leftover partial.
/// Handles \r\n (line end), \n (line end), and bare \r (carriage return —
/// overwrite current line). A trailing \r is deferred until the next chunk
/// so we can tell whether it's \r\n or a bare CR.
fn split_pty_lines(s: &str) -> (Vec<String>, String) {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                match chars.peek() {
                    Some('\n') => { chars.next(); lines.push(std::mem::take(&mut current)); }
                    Some(_)    => { current.clear(); } // bare CR: overwrite
                    None       => { current.push('\r'); } // defer until next chunk
                }
            }
            '\n' => lines.push(std::mem::take(&mut current)),
            _    => current.push(c),
        }
    }
    (lines, current)
}


fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for nc in chars.by_ref() {
                        if nc.is_ascii_alphabetic() { break; }
                    }
                }
                _ => { chars.next(); }
            }
        } else {
            out.push(c);
        }
    }
    out
}
