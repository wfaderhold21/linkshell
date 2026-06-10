use ratatui::layout::Rect;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::config::{self, Config};
use crate::events::AppEvent;
use crate::keybindings::{self, Keymap};
use crate::patterns::PatternMatcher;
use crate::pipe::{self, ExtractMode, Pipe, PipeTrigger};
use crate::session::{Session, SessionKind, SessionState, MAX_SESSIONS, PTY_COLS, PTY_ROWS};

/// Screen-coordinate selection (col, row both relative to output area inner content).
#[derive(Debug, Clone)]
pub struct Selection {
    pub start_col: u16,
    pub start_row: u16,
    pub end_col: u16,
    pub end_row: u16,
}

impl Selection {
    /// Returns ((min_row, min_col), (max_row, max_col)) in reading order.
    pub fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        let a = (self.start_row, self.start_col);
        let b = (self.end_row, self.end_col);
        if a <= b {
            (a, b)
        } else {
            (b, a)
        }
    }

    pub fn contains(&self, row: u16, col: u16) -> bool {
        let ((min_row, min_col), (max_row, max_col)) = self.normalized();
        if row < min_row || row > max_row {
            return false;
        }
        if row == min_row && row == max_row {
            return col >= min_col && col <= max_col;
        }
        if row == min_row {
            return col >= min_col;
        }
        if row == max_row {
            return col <= max_col;
        }
        true
    }
}

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
    pub name_cursor: usize,
    pub cwd_cursor: usize,
    pub custom_cmd_cursor: usize,
}

impl NewSessionState {
    /// Returns the cursor position for the currently active text field.
    /// Returns 0 for the Kind field (which has no text cursor).
    pub fn cursor_pos(&self) -> usize {
        match self.active_field {
            NewSessionField::Kind => 0,
            NewSessionField::Name => self.name_cursor,
            NewSessionField::Cwd => self.cwd_cursor,
            NewSessionField::CustomCmd => self.custom_cmd_cursor,
        }
    }

    fn active_field_mut(&mut self) -> Option<(&mut String, &mut usize)> {
        match self.active_field {
            NewSessionField::Kind => None,
            NewSessionField::Name => Some((&mut self.name, &mut self.name_cursor)),
            NewSessionField::Cwd => Some((&mut self.cwd, &mut self.cwd_cursor)),
            NewSessionField::CustomCmd => Some((&mut self.custom_cmd, &mut self.custom_cmd_cursor)),
        }
    }
}

impl Default for NewSessionState {
    fn default() -> Self {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "~".to_string());
        let cwd_cursor = cwd.len();
        Self {
            selected_kind: 0,
            name: String::new(),
            cwd,
            custom_cmd: String::new(),
            active_field: NewSessionField::Kind,
            name_cursor: 0,
            cwd_cursor,
            custom_cmd_cursor: 0,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum AppMode {
    Normal,
    NewSession,
    CommandBar,
    Help,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub active_idx: Option<usize>,
    pub mode: AppMode,
    pub new_session_state: NewSessionState,
    pub command_input: String,
    pub should_quit: bool,
    pub event_tx: mpsc::Sender<AppEvent>,
    pub config: Arc<Config>,
    pub pipes: Vec<Pipe>,
    // Current PTY size derived from the output pane (rows, cols)
    pub pty_size: (u16, u16),
    // Layout cache (updated after each draw, used for mouse hit-testing)
    pub output_area: Rect,
    pub session_bar_area: Rect,
    pub session_slot_areas: Vec<Rect>,
    // Text selection
    pub selection: Option<Selection>,
    matcher: PatternMatcher,
    next_id: usize,
    /// Pending IPC reply channels: session_id → (oneshot sender, line offset when input was sent).
    pub pending_ipc_replies:
        HashMap<usize, (tokio::sync::oneshot::Sender<serde_json::Value>, usize)>,
    /// Write channels to connected persistent agents, keyed by session_id.
    pub agent_writers: HashMap<usize, mpsc::Sender<String>>,
    /// Buffered pipe relays waiting for the dest session to reach READY.
    pub pending_relays: HashMap<usize, Vec<String>>,
    pipe_tasks: HashMap<PipeKey, JoinHandle<()>>,
    pub keymap: Keymap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PipeKey {
    source: usize,
    dest: usize,
    trigger: PipeTrigger,
}

impl PipeKey {
    fn from_pipe(pipe: &Pipe) -> Self {
        Self {
            source: pipe.source,
            dest: pipe.dest,
            trigger: pipe.trigger,
        }
    }
}

impl App {
    pub fn new(event_tx: mpsc::Sender<AppEvent>, config: Arc<Config>) -> Self {
        let keymap = keybindings::build_keymap(&config.keybindings);
        Self {
            sessions: Vec::new(),
            active_idx: None,
            mode: AppMode::Normal,
            new_session_state: NewSessionState::default(),
            command_input: String::new(),
            should_quit: false,
            event_tx,
            config,
            pipes: Vec::new(),
            pty_size: (PTY_ROWS, PTY_COLS),
            output_area: Rect::default(),
            session_bar_area: Rect::default(),
            session_slot_areas: Vec::new(),
            selection: None,
            matcher: PatternMatcher::new(),
            next_id: 0,
            pending_ipc_replies: HashMap::new(),
            agent_writers: HashMap::new(),
            pending_relays: HashMap::new(),
            pipe_tasks: HashMap::new(),
            keymap,
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

        // Resolve CWD: caller-supplied → config default → current dir.
        let cwd = if !cwd.is_empty() && cwd != "." {
            cwd
        } else if !self.config.sessions.default_cwd.is_empty() {
            self.config.sessions.default_cwd.clone()
        } else {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string())
        };

        // Resolve command from config overrides.
        let cmd_str = match &kind {
            SessionKind::Claude => self.config.sessions.commands.claude.clone(),
            SessionKind::Codex => self.config.sessions.commands.codex.clone(),
            SessionKind::Shell => {
                let c = &self.config.sessions.commands.shell;
                if c.is_empty() {
                    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
                } else {
                    c.clone()
                }
            }
            SessionKind::Custom(cmd) => cmd.clone(),
        };

        // Safety: refuse any command containing forbidden flags.
        config::validate_command(&cmd_str).map_err(|e| anyhow::anyhow!("{}", e))?;

        let (pty_rows, pty_cols) = self.pty_size;
        let session = Session::new(
            id,
            session_name,
            kind.clone(),
            cwd.clone(),
            pty_rows,
            pty_cols,
            self.config.general.scroll_buffer_lines,
        );
        let idx = self.sessions.len();
        self.sessions.push(session);

        if self.active_idx.is_none() {
            self.active_idx = Some(idx);
        }

        let tx = self.event_tx.clone();
        let cfg = Arc::clone(&self.config);
        let socket = crate::ipc::socket_path(&self.config);

        if matches!(kind, SessionKind::Claude) {
            crate::claude_log::spawn_watcher(id, cwd.clone(), tx.clone(), Arc::clone(&cfg));
        }
        if matches!(kind, SessionKind::Codex) {
            crate::codex_log::spawn_watcher(id, cwd.clone(), tx.clone(), Arc::clone(&cfg));
        }

        tokio::spawn(async move {
            if let Err(e) = run_pty(id, cmd_str, cwd, pty_rows, pty_cols, tx.clone(), socket).await
            {
                let _ = tx
                    .send(AppEvent::SessionOutput {
                        session_id: id,
                        line: format!("[error: {}]", e),
                    })
                    .await;
                let _ = tx.send(AppEvent::SessionDied { session_id: id }).await;
            }
        });

        Ok(())
    }

    pub fn spawn_headless_session(
        &mut self,
        name: String,
        group: Option<String>,
    ) -> anyhow::Result<usize> {
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(anyhow::anyhow!("Maximum {} sessions reached", MAX_SESSIONS));
        }
        let id = self.next_id;
        self.next_id += 1;
        let label = if name.is_empty() {
            format!("agent-{}", id + 1)
        } else {
            name
        };
        let cwd = if !self.config.sessions.default_cwd.is_empty() {
            self.config.sessions.default_cwd.clone()
        } else {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string())
        };
        let (pty_rows, pty_cols) = self.pty_size;
        let mut session = Session::new(
            id,
            label,
            SessionKind::Shell,
            cwd,
            pty_rows,
            pty_cols,
            self.config.general.scroll_buffer_lines,
        );
        session.headless = true;
        session.group = group;
        session.state = SessionState::Ready;
        self.sessions.push(session);
        Ok(id)
    }

    pub fn kill_active_session(&mut self) {
        if let Some(idx) = self.active_idx {
            self.remove_session(idx);
        }
    }

    fn remove_session(&mut self, idx: usize) {
        if idx >= self.sessions.len() {
            return;
        }
        // Drop the PTY write channel so the background task exits
        self.sessions[idx].pty_writer = None;
        self.sessions.remove(idx);
        let n = self.sessions.len();
        self.active_idx = if n == 0 { None } else { Some(idx.min(n - 1)) };
    }

    pub fn switch_to(&mut self, idx: usize) {
        if idx < self.sessions.len() {
            self.active_idx = Some(idx);
        }
    }

    pub fn next_session(&mut self) {
        let n = self.sessions.len();
        if n == 0 {
            return;
        }
        self.active_idx = Some(self.active_idx.map_or(0, |i| (i + 1) % n));
    }

    pub fn prev_session(&mut self) {
        let n = self.sessions.len();
        if n == 0 {
            return;
        }
        self.active_idx = Some(
            self.active_idx
                .map_or(0, |i| if i == 0 { n - 1 } else { i - 1 }),
        );
    }

    pub fn scroll_up(&mut self, lines: usize) {
        if let Some(idx) = self.active_idx {
            if let Some(session) = self.sessions.get_mut(idx) {
                let current = session.screen.screen().scrollback();
                session.screen.set_scrollback(current + lines);
            }
        }
    }

    pub fn scroll_down(&mut self, lines: usize) {
        if let Some(idx) = self.active_idx {
            if let Some(session) = self.sessions.get_mut(idx) {
                let current = session.screen.screen().scrollback();
                session.screen.set_scrollback(current.saturating_sub(lines));
            }
        }
    }

    pub fn scroll_offset(&self) -> usize {
        self.active_idx
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.screen.screen().scrollback())
            .unwrap_or(0)
    }

    // ── Event handlers ─────────────────────────────────────────────────────

    pub fn handle_session_writer(&mut self, session_id: usize, writer_tx: mpsc::Sender<Vec<u8>>) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            s.pty_writer = Some(writer_tx);
            s.state = SessionState::Ready;
        }
    }

    pub fn handle_session_resizer(
        &mut self,
        session_id: usize,
        resizer_tx: mpsc::Sender<(u16, u16)>,
    ) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            s.pty_resizer = Some(resizer_tx);
        }
    }

    /// Called from the main loop after each draw when the output area changes size.
    pub fn handle_resize(&mut self, rows: u16, cols: u16) {
        if self.pty_size == (rows, cols) {
            return;
        }
        self.pty_size = (rows, cols);
        for session in &mut self.sessions {
            session.resize_screen(rows, cols);
            if let Some(tx) = &session.pty_resizer {
                let _ = tx.try_send((rows, cols));
            }
        }
    }

    pub fn handle_session_bytes(&mut self, session_id: usize, data: Vec<u8>) {
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.process_bytes(&data);
        }
        // Auto-scroll to bottom when the active session receives new output
        let active_is_updated = self
            .active_idx
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.id == session_id)
            .unwrap_or(false);
        if active_is_updated {
            if let Some(idx) = self.active_idx {
                if let Some(session) = self.sessions.get_mut(idx) {
                    session.screen.set_scrollback(0);
                }
            }
        }
    }

    pub fn handle_session_output(&mut self, session_id: usize, line: String) {
        let mut state_before = None;
        let mut state_after = None;

        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            state_before = Some(session.state.clone());
            let stripped = strip_ansi(&line);
            session.push_output_line(stripped.clone());
            if !session.ipc_state {
                if let Some(new_state) = self.matcher.infer_state(&stripped, &session.kind) {
                    session.state = new_state;
                }
            }
            // Claude and Codex stats come from their JSONL watchers; skip terminal scraping.
            if matches!(session.kind, SessionKind::Shell | SessionKind::Custom(_)) {
                if let Some(stats) = self.matcher.parse_tokens(&stripped) {
                    session.accumulate_stats(stats);
                }
            }
            state_after = Some(session.state.clone());
        }

        if let (Some(before), Some(after)) = (state_before, state_after) {
            if before != after {
                self.check_pipes(session_id, &after);
                self.check_ipc_replies(session_id, &after);
                if after == SessionState::Ready {
                    self.flush_pending_relays(session_id);
                }
            }
        }
    }

    fn check_pipes(&mut self, session_id: usize, new_state: &SessionState) {
        let now = std::time::Instant::now();
        let mut to_fire: Vec<Pipe> = Vec::new();

        for p in self.pipes.iter_mut() {
            if p.source != session_id || !p.active {
                continue;
            }
            let fires = match p.trigger {
                PipeTrigger::OnReady => *new_state == SessionState::Ready,
                PipeTrigger::OnWaiting => *new_state == SessionState::Waiting,
                PipeTrigger::Manual => false,
            };
            if fires {
                let cooldown = self.config.pipe.summarize.cooldown_secs as f64;
                if let Some(last) = p.last_fired {
                    if last.elapsed().as_secs_f64() < cooldown {
                        continue;
                    }
                }
                p.last_fired = Some(now);
                to_fire.push(p.clone());
            }
        }

        for p in to_fire {
            if let Some(content) = pipe::extract_from_session(&self.sessions, p.source, &p.extract)
            {
                self.fire_pipe_task(p, content);
            }
        }
    }

    pub fn fire_manual_pipes(&mut self, source: usize, dest: Option<usize>) {
        let now = std::time::Instant::now();
        let mut to_fire: Vec<Pipe> = Vec::new();

        for p in self.pipes.iter_mut() {
            if p.source != source || !p.active {
                continue;
            }
            if p.trigger != PipeTrigger::Manual {
                continue;
            }
            if let Some(d) = dest {
                if p.dest != d {
                    continue;
                }
            }
            p.last_fired = Some(now);
            to_fire.push(p.clone());
        }

        for p in to_fire {
            if let Some(content) = pipe::extract_from_session(&self.sessions, p.source, &p.extract)
            {
                self.fire_pipe_task(p, content);
            }
        }
    }

    fn fire_pipe_task(&mut self, pipe: Pipe, content: String) {
        let key = PipeKey::from_pipe(&pipe);
        if let Some(previous) = self.pipe_tasks.remove(&key) {
            previous.abort();
        }
        let handle = pipe::fire_pipe_task(
            pipe,
            content,
            self.event_tx.clone(),
            Arc::clone(&self.config),
        );
        self.pipe_tasks.insert(key, handle);
    }

    pub fn handle_session_current_line(&mut self, session_id: usize, text: String) {
        let mut state_before = None;
        let mut state_after = None;

        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            state_before = Some(session.state.clone());
            let stripped = strip_ansi(&text);
            if !session.ipc_state {
                if let Some(new_state) = self.matcher.infer_state(&stripped, &session.kind) {
                    // Partial lines can detect Thinking/Waiting/Ready but must not
                    // flip to Running — that requires a complete line.
                    if new_state != SessionState::Running {
                        session.state = new_state;
                    }
                }
            }
            state_after = Some(session.state.clone());
        }

        if let (Some(before), Some(after)) = (state_before, state_after) {
            if before != after {
                self.check_pipes(session_id, &after);
            }
        }
    }

    pub fn handle_session_stats(&mut self, session_id: usize, stats: crate::session::TokenStats) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            s.stats = stats;
        }
    }

    pub fn handle_ipc_state(&mut self, session_id: usize, state: SessionState) {
        let old = self
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| s.state.clone());
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.state = state.clone();
            session.ipc_state = true;
            session.ipc_state_set_at = Some(std::time::Instant::now());
        }
        if old.as_ref() != Some(&state) {
            self.check_pipes(session_id, &state);
            self.check_ipc_replies(session_id, &state);
            if state == SessionState::Ready {
                self.flush_pending_relays(session_id);
            }
        }
    }

    pub fn handle_named_action(&mut self, session_name: String, msg: serde_json::Value) {
        let session_id = match self
            .sessions
            .iter()
            .find(|s| s.name == session_name)
            .map(|s| s.id)
        {
            Some(id) => id,
            None => return,
        };
        match msg["type"].as_str() {
            Some("state") => {
                if let Some(s) =
                    crate::ipc::parse_session_state(msg["state"].as_str().unwrap_or(""))
                {
                    if let Some(detail) = msg["detail"].as_str() {
                        let _ = self.event_tx.try_send(AppEvent::SessionOutput {
                            session_id,
                            line: format!("[{}]", detail),
                        });
                    }
                    self.handle_ipc_state(session_id, s);
                }
            }
            Some("tokens") => {
                use crate::session::TokenStats;
                self.handle_ipc_tokens(
                    session_id,
                    TokenStats {
                        input_tokens: msg["input"].as_u64().unwrap_or(0),
                        output_tokens: msg["output"].as_u64().unwrap_or(0),
                        total_cost_usd: msg["cost"].as_f64().unwrap_or(0.0),
                        context_tokens: 0,
                    },
                );
            }
            Some("output") => {
                if let Some(line) = msg["line"].as_str() {
                    let _ = self.event_tx.try_send(AppEvent::SessionOutput {
                        session_id,
                        line: line.to_string(),
                    });
                }
            }
            Some("fire_pipe") => {
                let dest = msg["dest"].as_u64().map(|v| v as usize);
                self.fire_manual_pipes(session_id, dest);
            }
            _ => {}
        }
    }

    pub fn handle_ipc_pipe_add(
        &mut self,
        source: usize,
        dest: usize,
        trigger: &str,
        extract: &str,
        prefix: Option<String>,
    ) {
        let trig = match trigger {
            "on_waiting" | "waiting" => PipeTrigger::OnWaiting,
            "manual" => PipeTrigger::Manual,
            _ => PipeTrigger::OnReady,
        };
        let ext = if extract == "diff" {
            ExtractMode::Diff
        } else if let Some(n) = extract.strip_prefix("last-n=") {
            ExtractMode::LastN(n.parse().unwrap_or(20))
        } else if let Some(n) = extract.strip_prefix("summarize=") {
            ExtractMode::Summarize(n.parse().unwrap_or(150))
        } else {
            ExtractMode::LastBlock
        };
        self.pipes.push(Pipe {
            source,
            dest,
            trigger: trig,
            extract: ext,
            prefix,
            active: true,
            last_fired: None,
        });
    }

    pub fn handle_ipc_pipe_remove(&mut self, source: usize, dest: Option<usize>) {
        match dest {
            None => {
                self.pipes.retain(|p| p.source != source);
                self.abort_pipe_tasks(|key| key.source == source);
            }
            Some(d) => {
                self.pipes.retain(|p| !(p.source == source && p.dest == d));
                self.abort_pipe_tasks(|key| key.source == source && key.dest == d);
            }
        }
    }

    fn abort_pipe_tasks(&mut self, mut should_abort: impl FnMut(PipeKey) -> bool) {
        let mut keep = HashMap::new();
        for (key, handle) in self.pipe_tasks.drain() {
            if should_abort(key) {
                handle.abort();
            } else {
                keep.insert(key, handle);
            }
        }
        self.pipe_tasks = keep;
    }

    pub fn handle_broadcast(&self, group: &str, msg: serde_json::Value) {
        let line = serde_json::to_string(&msg).unwrap_or_default() + "\n";
        for session in &self.sessions {
            if session.group.as_deref() == Some(group) {
                if let Some(tx) = self.agent_writers.get(&session.id) {
                    let _ = tx.try_send(line.clone());
                }
            }
        }
    }

    pub fn handle_group_fire(&mut self, source_group: &str) {
        let ids: Vec<usize> = self
            .sessions
            .iter()
            .filter(|s| s.group.as_deref() == Some(source_group))
            .map(|s| s.id)
            .collect();
        for id in ids {
            self.fire_manual_pipes(id, None);
        }
    }

    pub fn handle_ipc_tokens(&mut self, session_id: usize, stats: crate::session::TokenStats) {
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.accumulate_stats(stats);
        }
    }

    pub fn handle_ipc_agent_connected(
        &mut self,
        session_id: usize,
        agent_tx: mpsc::Sender<String>,
    ) {
        self.agent_writers.insert(session_id, agent_tx);
    }

    pub fn handle_ipc_agent_disconnected(&mut self, session_id: usize) {
        self.agent_writers.remove(&session_id);
        if let Some(idx) = self
            .sessions
            .iter()
            .position(|s| s.id == session_id && s.headless)
        {
            self.remove_session(idx);
        }
    }

    pub fn handle_ipc_send(&self, session_id: usize, message: serde_json::Value) {
        if let Some(tx) = self.agent_writers.get(&session_id) {
            let line = serde_json::to_string(&message).unwrap_or_default() + "\n";
            let _ = tx.try_send(line);
        }
    }

    pub fn handle_agent_direct_message(
        &self,
        from_session_id: Option<usize>,
        dest_name: &str,
        message: &str,
        reply_tx: Option<tokio::sync::oneshot::Sender<serde_json::Value>>,
    ) {
        let from_name = from_session_id
            .and_then(|id| self.sessions.iter().find(|s| s.id == id))
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "linkshell-ctl".to_string());

        let Some(dest) = self.sessions.iter().find(|s| s.name == dest_name) else {
            if let Some(tx) = reply_tx {
                let _ = tx.send(serde_json::json!({
                    "error": format!("unknown agent: {}", dest_name),
                }));
            }
            return;
        };

        if dest.headless {
            let Some(agent_tx) = self.agent_writers.get(&dest.id) else {
                if let Some(tx) = reply_tx {
                    let _ = tx.send(serde_json::json!({
                        "error": format!("agent {} is not connected", dest_name),
                    }));
                }
                return;
            };

            let envelope = serde_json::json!({
                "type": "agent_message",
                "from": from_name,
                "message": message,
            });
            let line = serde_json::to_string(&envelope).unwrap_or_default() + "\n";
            match agent_tx.try_send(line) {
                Ok(()) => {
                    if let Some(tx) = reply_tx {
                        let _ = tx.send(serde_json::json!({"ok": true}));
                    }
                }
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if let Some(tx) = reply_tx {
                        let _ = tx.send(serde_json::json!({
                            "error": format!("agent {} channel full — retry", dest_name),
                        }));
                    }
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    if let Some(tx) = reply_tx {
                        let _ = tx.send(serde_json::json!({
                            "error": format!("agent {} is disconnected", dest_name),
                        }));
                    }
                }
            }
            return;
        }

        let input = format!("{}\n", message).into_bytes();
        match dest.try_write_bytes(input) {
            Ok(()) => {
                if let Some(tx) = reply_tx {
                    let _ = tx.send(serde_json::json!({"ok": true}));
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                if let Some(tx) = reply_tx {
                    let _ = tx.send(serde_json::json!({
                        "error": format!("session {} channel full — retry", dest_name),
                    }));
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                if let Some(tx) = reply_tx {
                    let _ = tx.send(serde_json::json!({
                        "error": format!("session {} is disconnected", dest_name),
                    }));
                }
            }
        }
    }

    pub fn handle_session_died(&mut self, session_id: usize) {
        if let Some((resp_tx, _)) = self.pending_ipc_replies.remove(&session_id) {
            let _ = resp_tx.send(serde_json::json!({
                "error": "session died before reaching READY",
                "session_id": session_id,
            }));
        }
        self.pending_relays.remove(&session_id);
        if let Some(idx) = self.sessions.iter().position(|s| s.id == session_id) {
            self.remove_session(idx);
        }
    }

    pub fn handle_pipe_relay(&mut self, dest_id: usize, message: String) {
        if let Some(agent_tx) = self.agent_writers.get(&dest_id) {
            let relay = serde_json::json!({"type": "relay", "content": message.clone()});
            let line = serde_json::to_string(&relay).unwrap_or_default() + "\n";
            let _ = agent_tx.try_send(line);
        }
        let dest_ready = self
            .sessions
            .iter()
            .find(|s| s.id == dest_id)
            .map(|s| s.state == SessionState::Ready)
            .unwrap_or(false);
        if dest_ready {
            if let Some(session) = self.sessions.iter().find(|s| s.id == dest_id) {
                session.write_bytes(message.into_bytes());
            }
        } else {
            self.pending_relays
                .entry(dest_id)
                .or_default()
                .push(message);
        }
    }

    pub fn flush_pending_relays(&mut self, session_id: usize) {
        if let Some(msgs) = self.pending_relays.remove(&session_id) {
            if let Some(session) = self.sessions.iter().find(|s| s.id == session_id) {
                for msg in msgs {
                    session.write_bytes(msg.into_bytes());
                }
            }
        }
    }

    fn check_ipc_replies(&mut self, session_id: usize, new_state: &SessionState) {
        if *new_state != SessionState::Ready {
            return;
        }
        if let Some((resp_tx, line_offset)) = self.pending_ipc_replies.remove(&session_id) {
            let lines: Vec<String> = self
                .sessions
                .iter()
                .find(|s| s.id == session_id)
                .map(|s| s.output_lines.iter().skip(line_offset).cloned().collect())
                .unwrap_or_default();
            let _ = resp_tx.send(serde_json::json!({
                "session_id": session_id,
                "lines": lines,
            }));
        }
    }

    pub fn handle_ipc_query(
        &mut self,
        payload: crate::events::IpcQueryPayload,
        response_tx: tokio::sync::oneshot::Sender<serde_json::Value>,
    ) {
        use crate::events::IpcQueryPayload;
        match payload {
            IpcQueryPayload::SessionCreate {
                kind_str,
                name,
                cwd,
            } => {
                let kind = match kind_str.as_str() {
                    "claude" => crate::session::SessionKind::Claude,
                    "codex" => crate::session::SessionKind::Codex,
                    "shell" => crate::session::SessionKind::Shell,
                    other => {
                        let _ = response_tx.send(serde_json::json!({
                            "error": format!("unknown session kind: {}", other)
                        }));
                        return;
                    }
                };
                let new_id = self.next_id;
                match self.spawn_session(kind, name, cwd) {
                    Ok(()) => {
                        let _ = response_tx.send(serde_json::json!({"session_id": new_id}));
                    }
                    Err(e) => {
                        let _ = response_tx.send(serde_json::json!({"error": e.to_string()}));
                    }
                }
            }
            IpcQueryPayload::Register { name, group } => {
                match self.spawn_headless_session(name, group) {
                    Ok(new_id) => {
                        let _ = response_tx
                            .send(serde_json::json!({"type": "registered", "session_id": new_id}));
                    }
                    Err(e) => {
                        let _ = response_tx.send(serde_json::json!({"error": e.to_string()}));
                    }
                }
            }
            IpcQueryPayload::Query { what } => {
                let resp = match what.as_str() {
                    "sessions" => {
                        let arr: Vec<_> = self
                            .sessions
                            .iter()
                            .map(|s| {
                                serde_json::json!({
                                    "id":            s.id,
                                    "name":          s.name,
                                    "kind":          s.kind.label(),
                                    "state":         s.state.label(),
                                    "group":         s.group,
                                    "input_tokens":  s.stats.input_tokens,
                                    "output_tokens": s.stats.output_tokens,
                                    "cost_usd":      s.stats.total_cost_usd,
                                })
                            })
                            .collect();
                        serde_json::Value::Array(arr)
                    }
                    "pipes" => {
                        let arr: Vec<_> = self
                            .pipes
                            .iter()
                            .map(|p| {
                                let trigger = match p.trigger {
                                    PipeTrigger::OnReady => "on_ready",
                                    PipeTrigger::OnWaiting => "on_waiting",
                                    PipeTrigger::Manual => "manual",
                                };
                                let extract = match &p.extract {
                                    ExtractMode::LastBlock => "last-block".to_string(),
                                    ExtractMode::LastN(n) => format!("last-n={}", n),
                                    ExtractMode::Diff => "diff".to_string(),
                                    ExtractMode::Summarize(n) => format!("summarize={}", n),
                                };
                                serde_json::json!({
                                    "source":  p.source,
                                    "dest":    p.dest,
                                    "trigger": trigger,
                                    "extract": extract,
                                    "prefix":  p.prefix,
                                    "active":  p.active,
                                })
                            })
                            .collect();
                        serde_json::Value::Array(arr)
                    }
                    _ => serde_json::json!({"error": "unknown query"}),
                };
                let _ = response_tx.send(resp);
            }
            IpcQueryPayload::SessionInputWait { session_id, text } => {
                if let Some(s) = self.sessions.iter().find(|s| s.id == session_id) {
                    let line_offset = s.output_lines.len();
                    let mut input = text;
                    input.push('\n');
                    s.write_bytes(input.into_bytes());
                    self.pending_ipc_replies
                        .insert(session_id, (response_tx, line_offset));
                } else {
                    let _ = response_tx.send(serde_json::json!({"error": "session not found"}));
                }
            }
        }
    }

    pub fn handle_tick(&mut self) {
        let mut tick_ready: Vec<usize> = Vec::new();

        for session in self.sessions.iter_mut() {
            // Flip Ready → Running when a meaningful volume of bytes arrived this
            // tick. Cursor-blink sequences are ~20 bytes/tick; response streaming
            // is always well above that threshold.
            let bytes = session.bytes_since_last_tick;
            session.bytes_since_last_tick = 0;
            if !session.ipc_state && session.state == SessionState::Ready && bytes > 80 {
                session.state = SessionState::Running;
            }
            // Expire stale ipc_state overrides.
            if session.ipc_state {
                let timeout = self.config.general.ipc_state_override_timeout_secs;
                let expired = session
                    .ipc_state_set_at
                    .map(|t| t.elapsed().as_secs() > timeout)
                    .unwrap_or(false);
                if expired {
                    session.ipc_state = false;
                    session.ipc_state_set_at = None;
                    session.state = SessionState::Ready;
                    tick_ready.push(session.id);
                }
            }
            if !session.ipc_state {
                if let Some(last) = session.last_output_at {
                    let elapsed = last.elapsed();
                    if elapsed > Duration::from_secs(2)
                        && matches!(
                            session.state,
                            SessionState::Running | SessionState::Thinking
                        )
                    {
                        session.state = SessionState::Ready;
                        tick_ready.push(session.id);
                    } else if elapsed > Duration::from_secs(30)
                        && session.state == SessionState::Waiting
                    {
                        session.state = SessionState::Ready;
                        tick_ready.push(session.id);
                    }
                }
            }
        }

        for id in tick_ready {
            self.check_pipes(id, &SessionState::Ready);
            self.check_ipc_replies(id, &SessionState::Ready);
            self.flush_pending_relays(id);
        }
    }

    // ── Mouse handling ─────────────────────────────────────────────────────

    pub fn handle_mouse(&mut self, ev: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};

        let col = ev.column;
        let row = ev.row;

        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // Session bar click → switch session
                for (i, slot) in self.session_slot_areas.iter().enumerate() {
                    if rect_hit(*slot, col, row) {
                        self.switch_to(i);
                        self.selection = None;
                        return;
                    }
                }
                // Output area click → begin selection
                if rect_inner_hit(self.output_area, col, row) {
                    let (c, r) = to_content_coords(self.output_area, col, row);
                    self.selection = Some(Selection {
                        start_col: c,
                        start_row: r,
                        end_col: c,
                        end_row: r,
                    });
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if rect_inner_hit(self.output_area, col, row) {
                    let (c, r) = to_content_coords(self.output_area, col, row);
                    if let Some(sel) = &mut self.selection {
                        sel.end_col = c;
                        sel.end_row = r;
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // Finalize selection; auto-copy to clipboard
                if let Some(sel) = &self.selection {
                    let ((mr, mc), (er, ec)) = sel.normalized();
                    // Don't keep a zero-area click as a selection
                    if mr == er && mc == ec {
                        self.selection = None;
                    } else {
                        self.copy_selection();
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => {
                // Right-click copies current selection (or clears it)
                if self.selection.is_some() {
                    self.copy_selection();
                }
            }
            _ => {}
        }
    }

    fn selected_text(&self) -> Option<String> {
        let sel = self.selection.as_ref()?;
        let session = self.active_session()?;
        let screen = session.screen.screen();
        let (screen_rows, screen_cols) = screen.size();
        let display_rows = self.output_area.height.saturating_sub(2) as u16;
        let start_vt_row = screen_rows.saturating_sub(display_rows);

        let ((min_row, min_col), (max_row, max_col)) = sel.normalized();

        let mut text = String::new();
        for disp_row in min_row..=max_row {
            let vt_row = start_vt_row + disp_row;
            let col_start = if disp_row == min_row { min_col } else { 0 };
            let col_end = if disp_row == max_row {
                max_col
            } else {
                screen_cols.saturating_sub(1)
            };
            let mut row_text = String::new();
            for col in col_start..=col_end {
                if let Some(cell) = screen.cell(vt_row, col) {
                    let s = cell.contents();
                    row_text.push_str(if s.is_empty() { " " } else { &s });
                }
            }
            if disp_row < max_row {
                text.push_str(row_text.trim_end());
                text.push('\n');
            } else {
                text.push_str(row_text.trim_end());
            }
        }
        Some(text)
    }

    fn copy_selection(&self) {
        if let Some(text) = self.selected_text() {
            if !text.is_empty() {
                if let Ok(mut cb) = arboard::Clipboard::new() {
                    let _ = cb.set_text(text);
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
            Kind => Name,
            Name => Cwd,
            Cwd => {
                if is_custom {
                    CustomCmd
                } else {
                    Kind
                }
            }
            CustomCmd => Kind,
        };
    }

    pub fn new_session_input(&mut self, c: char) {
        if let Some((text, cursor)) = self.new_session_state.active_field_mut() {
            text.insert(*cursor, c);
            *cursor += c.len_utf8();
        }
    }

    pub fn new_session_backspace(&mut self) {
        if let Some((text, cursor)) = self.new_session_state.active_field_mut() {
            if *cursor > 0 {
                let prev = text[..*cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                text.remove(prev);
                *cursor = prev;
            }
        }
    }

    pub fn new_session_delete(&mut self) {
        if let Some((text, cursor)) = self.new_session_state.active_field_mut() {
            if *cursor < text.len() {
                text.remove(*cursor);
            }
        }
    }

    pub fn new_session_cursor_left(&mut self) {
        if let Some((text, cursor)) = self.new_session_state.active_field_mut() {
            if *cursor > 0 {
                *cursor = text[..*cursor]
                    .char_indices()
                    .next_back()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
        }
    }

    pub fn new_session_cursor_right(&mut self) {
        if let Some((text, cursor)) = self.new_session_state.active_field_mut() {
            if *cursor < text.len() {
                let ch = text[*cursor..].chars().next().unwrap();
                *cursor += ch.len_utf8();
            }
        }
    }

    pub fn new_session_cursor_home(&mut self) {
        if let Some((_, cursor)) = self.new_session_state.active_field_mut() {
            *cursor = 0;
        }
    }

    pub fn new_session_cursor_end(&mut self) {
        if let Some((text, cursor)) = self.new_session_state.active_field_mut() {
            *cursor = text.len();
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

        // Pipe commands need the full token list; handle before the splitn block.
        let all_parts: Vec<&str> = cmd.split_whitespace().collect();
        match all_parts.first().copied() {
            Some("pipe") => {
                self.execute_pipe_command(&all_parts[1..]);
                return;
            }
            Some("unpipe") => {
                self.execute_unpipe_command(&all_parts[1..]);
                return;
            }
            _ => {}
        }

        let parts: Vec<&str> = cmd.splitn(3, ' ').collect();
        match parts.as_slice() {
            ["new", kind_str, rest @ ..] => {
                let name = rest.first().unwrap_or(&"").to_string();
                let kind = match *kind_str {
                    "claude" => SessionKind::Claude,
                    "codex" => SessionKind::Codex,
                    "shell" => SessionKind::Shell,
                    other => SessionKind::Custom(other.to_string()),
                };
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "~".to_string());
                let _ = self.spawn_session(kind, name, cwd);
            }
            ["kill"] => self.kill_active_session(),
            ["kill", n] => {
                if let Ok(num) = n.parse::<usize>() {
                    if num >= 1 && num <= self.sessions.len() {
                        self.remove_session(num - 1);
                    }
                }
            }
            ["quit"] | ["q"] => self.should_quit = true,
            _ => {}
        }
    }

    fn execute_pipe_command(&mut self, args: &[&str]) {
        if args.first() == Some(&"fire") {
            let src_id = args
                .get(1)
                .and_then(|s| s.parse::<usize>().ok())
                .and_then(|n| self.sessions.get(n.wrapping_sub(1)))
                .map(|s| s.id);
            let dst_id = args
                .get(2)
                .and_then(|s| s.parse::<usize>().ok())
                .and_then(|n| self.sessions.get(n.wrapping_sub(1)))
                .map(|s| s.id);
            if let Some(source) = src_id {
                self.fire_manual_pipes(source, dst_id);
            }
            return;
        }

        if args.len() < 2 {
            return;
        }
        let src_id = args[0]
            .parse::<usize>()
            .ok()
            .and_then(|n| self.sessions.get(n.wrapping_sub(1)))
            .map(|s| s.id);
        let dst_id = args[1]
            .parse::<usize>()
            .ok()
            .and_then(|n| self.sessions.get(n.wrapping_sub(1)))
            .map(|s| s.id);

        let (source, dest) = match (src_id, dst_id) {
            (Some(s), Some(d)) => (s, d),
            _ => return,
        };

        let mut extract = ExtractMode::LastBlock;
        let mut trigger = PipeTrigger::OnReady;
        let mut prefix: Option<String> = None;

        let flags = &args[2..];
        let mut i = 0;
        while i < flags.len() {
            let flag = flags[i];
            if let Some(val) = flag.strip_prefix("--extract=") {
                extract = if val == "diff" {
                    ExtractMode::Diff
                } else if let Some(n) = val.strip_prefix("last-n=") {
                    ExtractMode::LastN(n.parse().unwrap_or(20))
                } else {
                    ExtractMode::LastBlock
                };
            } else if let Some(val) = flag.strip_prefix("--summarize=") {
                extract = ExtractMode::Summarize(val.parse().unwrap_or(150));
            } else if let Some(val) = flag.strip_prefix("--on=") {
                trigger = match val {
                    "waiting" => PipeTrigger::OnWaiting,
                    "manual" => PipeTrigger::Manual,
                    _ => PipeTrigger::OnReady,
                };
            } else if let Some(val) = flag.strip_prefix("--prefix=") {
                // Consume rest of tokens as the prefix value
                let rest = flags[i + 1..].join(" ");
                let full = if rest.is_empty() {
                    val.to_string()
                } else {
                    format!("{} {}", val, rest)
                };
                prefix = Some(full.trim_matches('"').to_string());
                break;
            }
            i += 1;
        }

        self.pipes.push(Pipe {
            source,
            dest,
            trigger,
            extract,
            prefix,
            active: true,
            last_fired: None,
        });
    }

    fn execute_unpipe_command(&mut self, args: &[&str]) {
        match args {
            [src] => {
                if let Ok(n) = src.parse::<usize>() {
                    if let Some(id) = self.sessions.get(n.wrapping_sub(1)).map(|s| s.id) {
                        self.pipes.retain(|p| p.source != id);
                        self.abort_pipe_tasks(|key| key.source == id);
                    }
                }
            }
            [src, dst] => {
                let src_id = src
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| self.sessions.get(n.wrapping_sub(1)))
                    .map(|s| s.id);
                let dst_id = dst
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| self.sessions.get(n.wrapping_sub(1)))
                    .map(|s| s.id);
                if let (Some(s), Some(d)) = (src_id, dst_id) {
                    self.pipes.retain(|p| !(p.source == s && p.dest == d));
                    self.abort_pipe_tasks(|key| key.source == s && key.dest == d);
                }
            }
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
    pty_rows: u16,
    pty_cols: u16,
    tx: mpsc::Sender<AppEvent>,
    linkshell_sock: String,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let pty = pty_process::Pty::new()?;

    // Spawn the child then immediately drop pts so the parent holds no reference
    // to the slave FD. Without this, reading the master never returns EIO after
    // the child exits — SessionDied would never fire.
    // Resize is best-effort: on macOS the TIOCSWINSZ ioctl on the master fd
    // returns ENOTTY in some environments; ignore the error so the session still
    // spawns (the PTY will use the OS default window size).
    let _child = {
        let pts = pty.pts()?;
        let _ = pty.resize(pty_process::Size::new(pty_rows, pty_cols));
        let args: Vec<&str> = cmd.split_whitespace().collect();
        let (bin, cmd_args) = match args.as_slice() {
            [first, rest @ ..] => (*first, rest),
            [] => return Err(anyhow::anyhow!("empty command")),
        };
        let mut command = pty_process::Command::new(bin);
        command.args(cmd_args);
        command.current_dir(&cwd);
        command.env("LINKSHELL_SESSION_ID", session_id.to_string());
        command.env("LINKSHELL_SOCK", &linkshell_sock);
        command.spawn(&pts)?
    };

    let (read_half, mut write_half) = pty.into_split();

    // Channel for bytes coming from App → PTY
    let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(64);
    // Clone for use inside the PTY reader task (kitty protocol responses)
    let write_tx_kitty = write_tx.clone();
    // Channel for resize events coming from App → PTY
    let (resize_tx, mut resize_rx) = mpsc::channel::<(u16, u16)>(4);

    // Notify App of the write and resize channels
    tx.send(AppEvent::SessionWriter {
        session_id,
        writer_tx: write_tx,
    })
    .await?;
    tx.send(AppEvent::SessionResizer {
        session_id,
        resizer_tx: resize_tx,
    })
    .await?;

    // Writer task: relay bytes and resize events from App into the PTY
    tokio::spawn(async move {
        loop {
            tokio::select! {
                Some(bytes) = write_rx.recv() => {
                    if write_half.write_all(&bytes).await.is_err() { break; }
                }
                Some((rows, cols)) = resize_rx.recv() => {
                    let _ = write_half.resize(pty_process::Size::new(rows, cols));
                }
                else => break,
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
        match tokio::time::timeout(std::time::Duration::from_millis(20), reader.read(&mut buf))
            .await
        {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                let data = buf[..n].to_vec();
                // Respond to kitty keyboard protocol query (CSI ? u → CSI ? 1 u).
                // Without this, apps like claude code never enable kitty mode and
                // won't recognize Shift+Enter (ESC [ 13 ; 2 u).
                if data.windows(4).any(|w| w == b"\x1b[?u") {
                    let _ = write_tx_kitty.send(b"\x1b[?1u".to_vec()).await;
                }
                // Raw bytes → vt100 screen buffer for display
                if tx
                    .send(AppEvent::SessionBytes {
                        session_id,
                        data: data.clone(),
                    })
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                // Line splitting → state inference only
                pending.push_str(&String::from_utf8_lossy(&data));
                let (complete, partial) = split_pty_lines(&pending);
                pending = partial;
                for line in complete {
                    if tx
                        .send(AppEvent::SessionOutput { session_id, line })
                        .await
                        .is_err()
                    {
                        return Ok(());
                    }
                }
            }
            Ok(Err(_)) => break,
            // Timeout: update current-line display without adding to the line buffer
            Err(_) => {
                if tx
                    .send(AppEvent::SessionCurrentLine {
                        session_id,
                        text: pending.clone(),
                    })
                    .await
                    .is_err()
                {
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
                    Some('\n') => {
                        chars.next();
                        lines.push(std::mem::take(&mut current));
                    }
                    Some(_) => {
                        current.clear();
                    } // bare CR: overwrite
                    None => {
                        current.push('\r');
                    } // defer until next chunk
                }
            }
            '\n' => lines.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    (lines, current)
}

fn rect_hit(r: Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
}

fn rect_inner_hit(r: Rect, col: u16, row: u16) -> bool {
    col > r.x
        && col < r.x + r.width.saturating_sub(1)
        && row > r.y
        && row < r.y + r.height.saturating_sub(1)
}

/// Convert absolute terminal coords to (content_col, content_row) inside a bordered rect.
fn to_content_coords(area: Rect, col: u16, row: u16) -> (u16, u16) {
    let c = col
        .saturating_sub(area.x + 1)
        .min(area.width.saturating_sub(2));
    let r = row
        .saturating_sub(area.y + 1)
        .min(area.height.saturating_sub(2));
    (c, r)
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
                        if nc.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn make_app() -> App {
        let (tx, _rx) = mpsc::channel::<AppEvent>(8);
        let config = Arc::new(crate::config::Config::default());
        App::new(tx, config)
    }

    fn make_app_with_config(config: crate::config::Config) -> App {
        let (tx, _rx) = mpsc::channel::<AppEvent>(8);
        App::new(tx, Arc::new(config))
    }

    fn cursor_pos(app: &App) -> usize {
        app.new_session_state.cursor_pos()
    }

    #[test]
    fn initial_cursor_at_zero_for_empty_name_field() {
        let mut app = make_app();
        app.open_new_session();
        app.new_session_tab();
        assert_eq!(app.new_session_state.active_field, NewSessionField::Name);
        assert_eq!(cursor_pos(&app), 0);
    }

    #[test]
    fn spawn_headless_session_uses_default_cwd() {
        let mut config = crate::config::Config::default();
        config.sessions.default_cwd = "/work".into();
        let mut app = make_app_with_config(config);

        let id = app.spawn_headless_session("agent".into(), None).unwrap();

        let session = app.sessions.iter().find(|s| s.id == id).unwrap();
        assert_eq!(session.cwd, "/work");
    }

    #[test]
    fn ipc_session_create_rejects_unknown_kind() {
        let mut app = make_app();
        let (tx, rx) = tokio::sync::oneshot::channel();

        app.handle_ipc_query(
            crate::events::IpcQueryPayload::SessionCreate {
                kind_str: "bogus".into(),
                name: String::new(),
                cwd: ".".into(),
            },
            tx,
        );

        let response = rx.blocking_recv().unwrap();
        assert_eq!(response["error"], "unknown session kind: bogus");
        assert!(app.sessions.is_empty());
    }

    #[test]
    fn initial_cursor_at_end_of_prefilled_cwd() {
        let mut app = make_app();
        app.open_new_session();
        app.new_session_tab();
        app.new_session_tab();
        assert_eq!(app.new_session_state.active_field, NewSessionField::Cwd);
        let expected = app.new_session_state.cwd.len();
        assert_eq!(
            cursor_pos(&app),
            expected,
            "cursor should be at the end of the pre-filled cwd"
        );
    }

    #[test]
    fn cursor_at_end_after_typing() {
        let mut app = make_app();
        app.open_new_session();
        app.new_session_tab();
        app.new_session_input('h');
        app.new_session_input('i');
        assert_eq!(app.new_session_state.name, "hi");
        assert_eq!(cursor_pos(&app), 2);
    }

    #[test]
    fn cursor_left_moves_one_position() {
        let mut app = make_app();
        app.open_new_session();
        app.new_session_tab();
        app.new_session_input('a');
        app.new_session_input('b');
        app.new_session_input('c');
        app.new_session_cursor_left();
        assert_eq!(cursor_pos(&app), 2);
        app.new_session_cursor_left();
        assert_eq!(cursor_pos(&app), 1);
    }

    #[test]
    fn cursor_right_moves_one_position() {
        let mut app = make_app();
        app.open_new_session();
        app.new_session_tab();
        app.new_session_input('x');
        app.new_session_input('y');
        app.new_session_cursor_left();
        app.new_session_cursor_left();
        app.new_session_cursor_right();
        assert_eq!(cursor_pos(&app), 1);
        app.new_session_cursor_right();
        assert_eq!(cursor_pos(&app), 2);
    }

    #[test]
    fn insert_at_cursor_not_at_end() {
        let mut app = make_app();
        app.open_new_session();
        app.new_session_tab();
        app.new_session_input('a');
        app.new_session_input('c');
        app.new_session_cursor_left();
        app.new_session_input('b');
        assert_eq!(app.new_session_state.name, "abc");
        assert_eq!(cursor_pos(&app), 2);
    }

    #[test]
    fn backspace_deletes_char_before_cursor_not_last_char() {
        let mut app = make_app();
        app.open_new_session();
        app.new_session_tab();
        app.new_session_input('a');
        app.new_session_input('b');
        app.new_session_input('c');
        app.new_session_cursor_left();
        app.new_session_backspace();
        assert_eq!(app.new_session_state.name, "ac");
        assert_eq!(cursor_pos(&app), 1);
    }

    #[test]
    fn cursor_left_stops_at_zero() {
        let mut app = make_app();
        app.open_new_session();
        app.new_session_tab();
        app.new_session_cursor_left();
        assert_eq!(cursor_pos(&app), 0);
        app.new_session_cursor_left();
        assert_eq!(cursor_pos(&app), 0);
    }

    #[test]
    fn cursor_right_stops_at_text_end() {
        let mut app = make_app();
        app.open_new_session();
        app.new_session_tab();
        app.new_session_input('z');
        app.new_session_cursor_right();
        assert_eq!(cursor_pos(&app), 1);
        app.new_session_cursor_right();
        assert_eq!(cursor_pos(&app), 1);
    }

    #[test]
    fn cursor_is_independent_per_field() {
        let mut app = make_app();
        app.open_new_session();
        app.new_session_tab(); // → Name
        app.new_session_input('h');
        app.new_session_input('i');
        app.new_session_cursor_left(); // Name cursor at 1

        app.new_session_tab(); // → Cwd
        let cwd_len = app.new_session_state.cwd.len();
        assert_eq!(cursor_pos(&app), cwd_len);

        app.new_session_tab(); // → Kind
        app.new_session_tab(); // → Name
        assert_eq!(app.new_session_state.active_field, NewSessionField::Name);
        assert_eq!(cursor_pos(&app), 1);
    }

    #[test]
    fn cursor_left_on_multibyte_char() {
        let mut app = make_app();
        app.open_new_session();
        app.new_session_tab(); // → Name
        app.new_session_input('é'); // 2 bytes in UTF-8
        assert_eq!(
            cursor_pos(&app),
            2,
            "cursor should be at byte 2 after typing é"
        );
        app.new_session_cursor_left();
        assert_eq!(
            cursor_pos(&app),
            0,
            "cursor should land at 0, not byte 1 (which is mid-char)"
        );
        app.new_session_input('x'); // insert before é
        assert_eq!(
            app.new_session_state.name, "xé",
            "should have xé, not corrupt string"
        );
    }

    #[test]
    fn selection_normalizes_and_contains_single_and_multi_line_ranges() {
        let sel = Selection {
            start_col: 5,
            start_row: 2,
            end_col: 1,
            end_row: 1,
        };

        assert_eq!(sel.normalized(), ((1, 1), (2, 5)));
        assert!(sel.contains(1, 1));
        assert!(sel.contains(1, 99));
        assert!(sel.contains(2, 5));
        assert!(!sel.contains(0, 99));
        assert!(!sel.contains(2, 6));

        let one_line = Selection {
            start_col: 2,
            start_row: 3,
            end_col: 4,
            end_row: 3,
        };
        assert!(one_line.contains(3, 3));
        assert!(!one_line.contains(3, 5));
    }

    #[test]
    fn headless_sessions_get_ids_labels_groups_and_ready_state() {
        let mut app = make_app();

        let first = app
            .spawn_headless_session("".into(), Some("agents".into()))
            .unwrap();
        let second = app.spawn_headless_session("named".into(), None).unwrap();

        assert_eq!(first, 0);
        assert_eq!(second, 1);
        assert_eq!(app.sessions[0].name, "agent-1");
        assert_eq!(app.sessions[0].group.as_deref(), Some("agents"));
        assert_eq!(app.sessions[0].state, SessionState::Ready);
        assert_eq!(app.sessions[1].name, "named");
    }

    #[test]
    fn switching_and_removing_sessions_keeps_active_index_valid() {
        let mut app = make_app();
        let _ = app.spawn_headless_session("one".into(), None).unwrap();
        let _ = app.spawn_headless_session("two".into(), None).unwrap();
        let _ = app.spawn_headless_session("three".into(), None).unwrap();

        app.switch_to(2);
        assert_eq!(app.active_idx, Some(2));
        app.kill_active_session();
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.active_idx, Some(1));

        app.prev_session();
        assert_eq!(app.active_idx, Some(0));
        app.next_session();
        assert_eq!(app.active_idx, Some(1));
    }

    #[test]
    fn session_output_strips_ansi_updates_state_and_accumulates_shell_stats() {
        let mut app = make_app();
        let id = app.spawn_headless_session("shell".into(), None).unwrap();
        app.sessions[0].kind = SessionKind::Shell;

        app.handle_session_output(id, "\x1b[31m100 input 200 output $0.01\x1b[0m".into());

        assert_eq!(
            app.sessions[0].output_lines.back().unwrap(),
            "100 input 200 output $0.01"
        );
        assert_eq!(app.sessions[0].state, SessionState::Running);
        assert_eq!(app.sessions[0].stats.input_tokens, 100);
        assert_eq!(app.sessions[0].stats.output_tokens, 200);
        assert_eq!(app.sessions[0].stats.total_cost_usd, 0.01);
    }

    #[test]
    fn partial_current_line_does_not_flip_ready_session_to_running() {
        let mut app = make_app();
        let id = app.spawn_headless_session("shell".into(), None).unwrap();
        app.sessions[0].state = SessionState::Ready;

        app.handle_session_current_line(id, "partial output".into());

        assert_eq!(app.sessions[0].state, SessionState::Ready);
    }

    #[test]
    fn resize_updates_all_sessions_and_notifies_resizers() {
        let mut app = make_app();
        let id = app.spawn_headless_session("shell".into(), None).unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        app.handle_session_resizer(id, tx);

        app.handle_resize(12, 80);

        assert_eq!(app.pty_size, (12, 80));
        assert_eq!(app.sessions[0].screen.screen().size(), (12, 80));
        assert_eq!(rx.try_recv().unwrap(), (12, 80));
    }

    #[test]
    fn pipe_add_parses_extract_and_trigger_variants_and_remove_filters() {
        let mut app = make_app();

        app.handle_ipc_pipe_add(1, 2, "waiting", "last-n=3", Some("p".into()));
        app.handle_ipc_pipe_add(1, 3, "manual", "summarize=42", None);
        app.handle_ipc_pipe_add(4, 5, "ready", "diff", None);

        assert_eq!(app.pipes[0].trigger, PipeTrigger::OnWaiting);
        assert!(matches!(app.pipes[0].extract, ExtractMode::LastN(3)));
        assert_eq!(app.pipes[1].trigger, PipeTrigger::Manual);
        assert!(matches!(app.pipes[1].extract, ExtractMode::Summarize(42)));
        assert!(matches!(app.pipes[2].extract, ExtractMode::Diff));

        app.handle_ipc_pipe_remove(1, Some(2));
        assert_eq!(app.pipes.len(), 2);
        assert!(app.pipes.iter().all(|p| !(p.source == 1 && p.dest == 2)));

        app.handle_ipc_pipe_remove(1, None);
        assert_eq!(app.pipes.len(), 1);
        assert_eq!(app.pipes[0].source, 4);
    }

    #[test]
    fn pipe_relay_writes_ready_dest_and_buffers_non_ready_dest_until_flush() {
        let mut app = make_app();
        let ready_id = app.spawn_headless_session("ready".into(), None).unwrap();
        let waiting_id = app.spawn_headless_session("waiting".into(), None).unwrap();
        let (ready_tx, mut ready_rx) = mpsc::channel(2);
        let (waiting_tx, mut waiting_rx) = mpsc::channel(2);
        app.sessions[0].pty_writer = Some(ready_tx);
        app.sessions[1].pty_writer = Some(waiting_tx);
        app.sessions[1].state = SessionState::Running;

        app.handle_pipe_relay(ready_id, "now".into());
        app.handle_pipe_relay(waiting_id, "later".into());

        assert_eq!(ready_rx.try_recv().unwrap(), b"now".to_vec());
        assert!(waiting_rx.try_recv().is_err());
        assert_eq!(app.pending_relays[&waiting_id], vec!["later".to_string()]);

        app.sessions[1].state = SessionState::Ready;
        app.flush_pending_relays(waiting_id);

        assert_eq!(waiting_rx.try_recv().unwrap(), b"later".to_vec());
        assert!(!app.pending_relays.contains_key(&waiting_id));
    }

    #[test]
    fn broadcast_and_direct_send_write_json_lines_to_connected_agents() {
        let mut app = make_app();
        let a = app
            .spawn_headless_session("a".into(), Some("agents".into()))
            .unwrap();
        let b = app
            .spawn_headless_session("b".into(), Some("other".into()))
            .unwrap();
        let (a_tx, mut a_rx) = mpsc::channel(2);
        let (b_tx, mut b_rx) = mpsc::channel(2);
        app.handle_ipc_agent_connected(a, a_tx);
        app.handle_ipc_agent_connected(b, b_tx);

        app.handle_broadcast("agents", serde_json::json!({"type": "ping"}));
        app.handle_ipc_send(b, serde_json::json!({"type": "direct"}));

        assert_eq!(a_rx.try_recv().unwrap(), "{\"type\":\"ping\"}\n");
        assert_eq!(b_rx.try_recv().unwrap(), "{\"type\":\"direct\"}\n");
        assert!(a_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_direct_message_writes_envelope_to_connected_headless_dest() {
        let mut app = make_app();
        let sender = app.spawn_headless_session("sender".into(), None).unwrap();
        let dest = app.spawn_headless_session("dest".into(), None).unwrap();
        let (dest_tx, mut dest_rx) = mpsc::channel(1);
        app.handle_ipc_agent_connected(dest, dest_tx);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        app.handle_agent_direct_message(Some(sender), "dest", "hello", Some(reply_tx));

        assert_eq!(reply_rx.await.unwrap(), serde_json::json!({"ok": true}));
        let line = dest_rx.try_recv().unwrap();
        let envelope: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(envelope["type"], "agent_message");
        assert_eq!(envelope["from"], "sender");
        assert_eq!(envelope["message"], "hello");
    }

    #[tokio::test]
    async fn agent_direct_message_reports_unknown_and_disconnected_dests() {
        let mut app = make_app();
        let _ = app.spawn_headless_session("dest".into(), None).unwrap();

        let (unknown_tx, unknown_rx) = tokio::sync::oneshot::channel();
        app.handle_agent_direct_message(None, "missing", "hello", Some(unknown_tx));
        assert_eq!(
            unknown_rx.await.unwrap(),
            serde_json::json!({"error": "unknown agent: missing"})
        );

        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
        app.handle_agent_direct_message(None, "dest", "hello", Some(closed_tx));
        assert_eq!(
            closed_rx.await.unwrap(),
            serde_json::json!({"error": "agent dest is not connected"})
        );
    }

    #[tokio::test]
    async fn agent_direct_message_writes_raw_text_to_pty_dest() {
        let mut app = make_app();
        let dest = app.spawn_headless_session("terminal".into(), None).unwrap();
        app.sessions[0].headless = false;
        let (pty_tx, mut pty_rx) = mpsc::channel(1);
        app.sessions[0].pty_writer = Some(pty_tx);
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        app.handle_agent_direct_message(None, "terminal", "run this", Some(reply_tx));

        assert_eq!(dest, 0);
        assert_eq!(reply_rx.await.unwrap(), serde_json::json!({"ok": true}));
        assert_eq!(pty_rx.try_recv().unwrap(), b"run this\n".to_vec());
    }

    #[tokio::test]
    async fn ipc_query_returns_session_and_pipe_snapshots() {
        let mut app = make_app();
        let source = app
            .spawn_headless_session("source".into(), Some("agents".into()))
            .unwrap();
        let dest = app.spawn_headless_session("dest".into(), None).unwrap();
        app.handle_ipc_pipe_add(source, dest, "manual", "diff", Some("prefix".into()));

        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        app.handle_ipc_query(
            crate::events::IpcQueryPayload::Query {
                what: "sessions".into(),
            },
            resp_tx,
        );
        let sessions = resp_rx.await.unwrap();
        assert_eq!(sessions.as_array().unwrap().len(), 2);
        assert_eq!(sessions[0]["name"], "source");
        assert_eq!(sessions[0]["state"], "READY");
        assert_eq!(sessions[0]["group"], "agents");

        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        app.handle_ipc_query(
            crate::events::IpcQueryPayload::Query {
                what: "pipes".into(),
            },
            resp_tx,
        );
        let pipes = resp_rx.await.unwrap();
        assert_eq!(pipes[0]["source"], source);
        assert_eq!(pipes[0]["dest"], dest);
        assert_eq!(pipes[0]["trigger"], "manual");
        assert_eq!(pipes[0]["extract"], "diff");
    }

    #[test]
    fn tick_turns_ready_to_running_on_large_byte_burst_and_expires_ipc_state() {
        let mut app = make_app();
        let id = app.spawn_headless_session("shell".into(), None).unwrap();
        app.sessions[0].state = SessionState::Ready;
        app.sessions[0].bytes_since_last_tick = 81;

        app.handle_tick();

        assert_eq!(app.sessions[0].state, SessionState::Running);
        assert_eq!(app.sessions[0].bytes_since_last_tick, 0);

        app.config = Arc::new(Config {
            general: config::GeneralConfig {
                ipc_state_override_timeout_secs: 0,
                ..Default::default()
            },
            ..Default::default()
        });
        app.handle_ipc_state(id, SessionState::Waiting);
        app.sessions[0].ipc_state_set_at =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(2));

        app.handle_tick();

        assert_eq!(app.sessions[0].state, SessionState::Ready);
        assert!(!app.sessions[0].ipc_state);
    }

    #[test]
    fn session_died_replies_to_pending_wait_clears_relays_and_removes_session() {
        let mut app = make_app();
        let id = app.spawn_headless_session("shell".into(), None).unwrap();
        app.pending_relays.insert(id, vec!["pending".into()]);
        let (resp_tx, mut resp_rx) = tokio::sync::oneshot::channel();
        app.pending_ipc_replies.insert(id, (resp_tx, 0));

        app.handle_session_died(id);

        assert!(app.sessions.is_empty());
        assert!(!app.pending_relays.contains_key(&id));
        let resp = resp_rx.try_recv().unwrap();
        assert_eq!(resp["error"], "session died before reaching READY");
        assert_eq!(resp["session_id"], id);
    }

    #[test]
    fn split_pty_lines_handles_lf_crlf_bare_cr_and_deferred_cr() {
        let (lines, pending) = split_pty_lines("one\ntwo\r\nspinner 1\rspinner 2\r");
        assert_eq!(lines, vec!["one".to_string(), "two".to_string()]);
        assert_eq!(pending, "spinner 2\r");

        let (lines, pending) = split_pty_lines(&(pending + "\n"));
        assert_eq!(lines, vec!["spinner 2".to_string()]);
        assert_eq!(pending, "");
    }

    #[test]
    fn geometry_helpers_detect_bordered_rect_hits_and_content_coords() {
        let rect = Rect {
            x: 10,
            y: 5,
            width: 20,
            height: 10,
        };

        assert!(rect_hit(rect, 10, 5));
        assert!(!rect_hit(rect, 30, 5));
        assert!(!rect_inner_hit(rect, 10, 5));
        assert!(rect_inner_hit(rect, 11, 6));
        assert_eq!(to_content_coords(rect, 12, 8), (1, 2));
        assert_eq!(to_content_coords(rect, 99, 99), (18, 8));
    }

    #[test]
    fn strip_ansi_removes_csi_sequences_and_keeps_plain_text() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m plain"), "red plain");
    }
}
