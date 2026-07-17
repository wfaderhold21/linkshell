use ratatui::layout::Rect;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::auth::CapSet;
use crate::config::{self, Config};
use crate::council::CouncilRouter;
use crate::events::AppEvent;
use crate::keybindings::{self, Keymap};
use crate::patterns::PatternMatcher;
use crate::pipe::{self, ExtractMode, Pipe, PipeTrigger};
use crate::session::{
    extract_waiting_prompt, Session, SessionKind, SessionState, MAX_SESSIONS, PTY_COLS, PTY_ROWS,
};

fn expand_home(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}{}", home, &path[1..]);
        }
    }
    path.to_string()
}

fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let mut score = 0;
    let mut position = 0;
    let bytes = candidate.as_bytes();
    for needle in query.bytes() {
        let relative = bytes[position..].iter().position(|byte| *byte == needle)?;
        position += relative;
        score += 100 - relative as i32;
        if position == 0 || candidate.as_bytes().get(position.wrapping_sub(1)) == Some(&b' ') {
            score += 25;
        }
        position += 1;
    }
    Some(score)
}

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
    /// Whether the Kind dropdown list is expanded.
    pub kind_dropdown_open: bool,
    pub name: String,
    pub cwd: String,
    pub custom_cmd: String,
    pub active_field: NewSessionField,
    pub name_cursor: usize,
    pub cwd_cursor: usize,
    pub custom_cmd_cursor: usize,
}

impl NewSessionState {
    /// True when the selected kind is Custom (always the last dropdown entry).
    pub fn is_custom(&self) -> bool {
        self.selected_kind == crate::session::SessionKind::COUNT - 1
    }

    #[allow(dead_code)] // used only in #[cfg(test)] module
    /// Returns the cursor position for the currently active text field.
    /// Returns 0 for the Kind field (which has no text cursor).
    /// Kept as public API surface; exercised by the dialog cursor tests.
    #[cfg_attr(not(test), allow(dead_code))]
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
            kind_dropdown_open: false,
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

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    Chat,
    NewSession,
    FileBrowser,
    CommandBar,
    CommandResult,
    Help,
    PipeList,
    Menu {
        selected_top: usize,
        selected_sub: Option<usize>,
    },
    Search {
        query: String,
        cursor: usize,
        matches: Vec<usize>,
        selected: usize,
    },
    Settings,
}

#[derive(Debug, Clone)]
pub struct SettingsField {
    pub label: &'static str,
    pub value: String,
    #[allow(dead_code)]
    pub description: &'static str,
}

#[derive(Debug, Clone)]
pub struct SettingsState {
    pub fields: Vec<SettingsField>,
    pub selected: usize,
    pub editing: bool,
    pub edit_buf: String,
    pub edit_cursor: usize,
}

impl SettingsState {
    pub fn new_empty() -> Self {
        Self {
            fields: Vec::new(),
            selected: 0,
            editing: false,
            edit_buf: String::new(),
            edit_cursor: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipeGlyph {
    pub outgoing: bool,
    pub peer: String,
    pub recent: bool,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct ChatMsg {
    pub from: String,
    pub text: String,
}

/// A chat message injected into a session's PTY, awaiting the session's
/// return to READY so its answer can be pulled back into the chat.
#[derive(Debug, Clone)]
pub struct PendingChat {
    pub session_id: usize,
    pub name: String,
}

/// A gated orchestrator tool call awaiting /approve or /deny (propose
/// mode). The orchestrator's task blocks on response_tx; dropping it
/// unanswered resolves as a denial on the orchestrator side.
#[derive(Debug)]
pub struct PendingProposal {
    pub tool: String,
    pub detail: String,
    pub response_tx: Option<tokio::sync::oneshot::Sender<crate::events::ProposalVerdict>>,
}

/// A kill request from the orchestrator awaiting human confirmation.
#[derive(Debug, Clone)]
pub struct PendingKill {
    pub session_id: usize,
    pub session_name: String,
    pub reason: String,
    pub requested_at: std::time::Instant,
}

#[derive(Debug, Clone, Default)]
pub struct ChatState {
    pub input: String,
    pub cursor: usize,
    pub messages: Vec<ChatMsg>,
    /// Lines scrolled up from the tail of the transcript.
    pub scroll: usize,
    /// Last explicitly addressed target; bare messages go here.
    pub target: Option<String>,
    /// Per-local-agent conversation history (role, content), oldest first.
    pub histories: std::collections::HashMap<String, Vec<(String, String)>>,
    pub pending: Vec<PendingChat>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    pub template: String,
    pub summary: String,
    pub insert: String,
}

#[derive(Debug, Clone, Default)]
pub struct PaletteState {
    pub matches: Vec<PaletteEntry>,
    pub selected: usize,
}

const COMMAND_PALETTE: &[(&str, &str, &str)] = &[
    ("new claude <name>", "Start a Claude session", "new claude "),
    ("new codex <name>", "Start a Codex session", "new codex "),
    ("new shell <name>", "Start a shell session", "new shell "),
    (
        "new custom <command>",
        "Start a custom command",
        "new custom ",
    ),
    ("kill <session>", "Stop a session", "kill "),
    (
        "yes [session]",
        "Approve a pending permission prompt",
        "yes ",
    ),
    ("no [session]", "Deny a pending permission prompt", "no "),
    (
        "pipe <src> <dst> [options]",
        "Wire session output to another session",
        "pipe ",
    ),
    ("pipe fire <src> [dst]", "Fire a manual pipe", "pipe fire "),
    ("unpipe <src> [dst]", "Remove matching pipes", "unpipe "),
    ("pipes", "Inspect and manage pipes", "pipes"),
    ("council <command>", "Control the agent council", "council "),
    (
        "profile save <name>",
        "Save the current layout as a profile",
        "profile save ",
    ),
    (
        "config <command>",
        "Inspect or reload configuration",
        "config ",
    ),
    (
        "grant <session> <tier>",
        "Change session capabilities",
        "grant ",
    ),
    ("restart <session>", "Restart a session", "restart "),
    ("move <from> <to>", "Swap session positions", "move "),
    ("rename <session> <name>", "Rename a session", "rename "),
    ("log <session> <path>", "Log session output to file", "log "),
    (
        "log stop <session>",
        "Stop logging session output",
        "log stop ",
    ),
    ("broadcast toggle", "Toggle broadcast mode", "broadcast"),
    ("search <query>", "Search session output", "search "),
    ("settings", "Open settings menu", "settings"),
    (
        "detach",
        "Detach from terminal (keep sessions running)",
        "detach",
    ),
    ("quit", "Exit linkshell", "quit"),
];

#[derive(Debug, Clone)]
pub struct FileBrowserState {
    pub current_dir: PathBuf,
    pub entries: Vec<PathBuf>,
    pub selected: usize,
    pub scroll_offset: usize,
}

impl FileBrowserState {
    pub fn new(start: &str) -> Self {
        let current_dir = PathBuf::from(start)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("."));
        let mut state = Self {
            current_dir,
            entries: Vec::new(),
            selected: 0,
            scroll_offset: 0,
        };
        state.refresh();
        state
    }

    pub fn refresh(&mut self) {
        self.entries.clear();
        if let Some(parent) = self.current_dir.parent() {
            self.entries.push(parent.to_path_buf());
        }
        if let Ok(rd) = std::fs::read_dir(&self.current_dir) {
            let mut dirs: Vec<PathBuf> = rd
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.path())
                .collect();
            dirs.sort();
            self.entries.extend(dirs);
        }
    }

    pub fn entry_label(&self, idx: usize) -> String {
        let path = &self.entries[idx];
        if Some(path.as_path()) == self.current_dir.parent() {
            format!(".. ({})", path.display())
        } else {
            path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    Single,
    /// Two panes side by side (vertical divider).
    SplitV,
    /// Two panes stacked (horizontal divider).
    SplitH,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub layout: LayoutMode,
    /// Split direction restored by toggle_split after a collapse to Single.
    pub last_split: LayoutMode,
    pub panes: [Option<usize>; 2],
    pub focused_pane: usize,
    pub mode: AppMode,
    pub new_session_state: NewSessionState,
    pub command_input: String,
    pub command_cursor: usize,
    pub command_result: String,
    pub palette: PaletteState,
    pub pipe_list_selected: usize,
    pub should_quit: bool,
    pub needs_redraw: bool,
    pub event_tx: mpsc::Sender<AppEvent>,
    pub config: Arc<Config>,
    pub pipes: Vec<Pipe>,
    // Current PTY size derived from the output pane (rows, cols)
    pub pane_sizes: [(u16, u16); 2],
    // Layout cache (updated after each draw, used for mouse hit-testing)
    pub output_areas: Vec<Rect>,
    pub session_bar_area: Rect,
    pub session_slot_areas: Vec<Rect>,
    pub status_row_areas: Vec<Rect>,
    pub new_session_area: Rect,
    pub browse_button_area: Rect,
    pub file_browser_area: Rect,
    pub file_browser_state: FileBrowserState,
    pub command_bar_area: Rect,
    pub help_area: Rect,
    pub chat_area: Rect,
    /// Transcript sub-rect of the chat pane (for mouse hit-testing).
    pub chat_transcript_area: Rect,
    /// Highest valid chat.scroll for the last-drawn transcript.
    pub chat_scroll_max: usize,
    /// Plain text of the transcript rows visible in the last draw, for copy.
    pub chat_visible_lines: Vec<String>,
    /// Mouse selection inside the chat transcript (content coordinates).
    pub chat_selection: Option<Selection>,
    pub menu_bar_area: Rect,
    pub menu_item_areas: Vec<Rect>,
    pub menu_submenu_area: Rect,
    pub menu_submenu_item_areas: Vec<Rect>,
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
    /// token -> session_id mapping for capability lookup
    pub tokens: HashMap<String, usize>,
    /// session_id -> granted capabilities
    pub caps: HashMap<usize, CapSet>,
    /// Optional council router for multi-agent orchestration
    pub council: Option<CouncilRouter>,
    /// Handle to the in-process orchestrator agent task (Class A providers)
    pub orchestrator: Option<crate::orchestrator::OrchestratorHandle>,
    /// Session id of the CLI-class orchestrator (Class B providers)
    pub orchestrator_session_id: Option<usize>,
    /// Kill request awaiting human /confirm-kill
    pub pending_kill: Option<PendingKill>,
    /// Propose mode: the orchestrator tool call currently awaiting a verdict.
    pub pending_proposal: Option<PendingProposal>,
    /// Token usage of the orchestrator's own API calls
    pub orchestrator_stats: crate::session::TokenStats,
    /// Live progress of the orchestrator's current turn, plus when it was
    /// set (drives the chat-pane spinner). None while idle.
    pub orchestrator_status: Option<(String, std::time::Instant)>,
    /// Per-(session_id, state label) cooldown for proactive orchestrator events
    orch_event_cooldowns: HashMap<(usize, &'static str), std::time::Instant>,
    /// Session behind the most recent permission request surfaced in chat;
    /// `/yes` and `/no` without a target answer this one.
    pub last_permission_request: Option<usize>,
    pub chat: ChatState,
    /// When true, key input is forwarded to all non-dead sessions
    pub broadcast_mode: bool,
    pub settings_state: SettingsState,
    /// Kept alive for the app's lifetime: on X11/Wayland the clipboard
    /// contents are owned by this process and are lost if the handle drops.
    clipboard: Option<arboard::Clipboard>,
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
        let mut keymap = keybindings::build_keymap(&config.keybindings);
        if let Some(chord) = keybindings::parse_chord(&config.general.menu_key) {
            keymap.insert(chord, keybindings::Action::OpenMenu);
        }
        Self {
            sessions: Vec::new(),
            layout: LayoutMode::Single,
            last_split: LayoutMode::SplitV,
            panes: [None, None],
            focused_pane: 0,
            mode: AppMode::Normal,
            new_session_state: NewSessionState::default(),
            command_input: String::new(),
            command_cursor: 0,
            command_result: String::new(),
            palette: PaletteState::default(),
            pipe_list_selected: 0,
            should_quit: false,
            needs_redraw: true,
            event_tx,
            config,
            pipes: Vec::new(),
            pane_sizes: [(PTY_ROWS, PTY_COLS); 2],
            output_areas: Vec::new(),
            session_bar_area: Rect::default(),
            session_slot_areas: Vec::new(),
            status_row_areas: Vec::new(),
            new_session_area: Rect::default(),
            browse_button_area: Rect::default(),
            file_browser_area: Rect::default(),
            file_browser_state: FileBrowserState::new("."),
            command_bar_area: Rect::default(),
            help_area: Rect::default(),
            chat_area: Rect::default(),
            chat_transcript_area: Rect::default(),
            chat_scroll_max: 0,
            chat_visible_lines: Vec::new(),
            chat_selection: None,
            menu_bar_area: Rect::default(),
            menu_item_areas: Vec::new(),
            menu_submenu_area: Rect::default(),
            menu_submenu_item_areas: Vec::new(),
            selection: None,
            matcher: PatternMatcher::new(),
            next_id: 0,
            pending_ipc_replies: HashMap::new(),
            agent_writers: HashMap::new(),
            pending_relays: HashMap::new(),
            pipe_tasks: HashMap::new(),
            keymap,
            tokens: HashMap::new(),
            caps: HashMap::new(),
            council: None,
            orchestrator: None,
            orchestrator_session_id: None,
            pending_kill: None,
            pending_proposal: None,
            orchestrator_stats: crate::session::TokenStats::default(),
            orchestrator_status: None,
            orch_event_cooldowns: HashMap::new(),
            last_permission_request: None,
            chat: ChatState::default(),
            broadcast_mode: false,
            settings_state: SettingsState::new_empty(),
            clipboard: None,
        }
    }

    pub fn active_session(&self) -> Option<&Session> {
        self.active_idx().and_then(|i| self.sessions.get(i))
    }

    pub fn active_session_mut(&mut self) -> Option<&mut Session> {
        self.active_idx().and_then(|i| self.sessions.get_mut(i))
    }

    pub fn active_idx(&self) -> Option<usize> {
        self.panes[self.focused_pane]
    }

    /// Indices into `sessions` of the sessions the user can see and switch
    /// to. A hidden orchestrator session is excluded everywhere the user
    /// addresses sessions by position (session bar, Alt+N, `kill <n>`).
    pub fn visible_indices(&self) -> Vec<usize> {
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.hidden)
            .map(|(i, _)| i)
            .collect()
    }

    /// Map a 0-based visible position (what the user sees in the bar) to a
    /// real index into `sessions`.
    pub fn visible_to_idx(&self, pos: usize) -> Option<usize> {
        self.visible_indices().get(pos).copied()
    }

    fn visible_count(&self) -> usize {
        self.sessions.iter().filter(|s| !s.hidden).count()
    }

    pub fn apply_profile(&mut self, profile: &config::Profile) -> anyhow::Result<()> {
        let mut ids = HashMap::new();
        for profile_session in &profile.sessions {
            let kind = match profile_session.kind.as_str() {
                "custom" => SessionKind::Custom(profile_session.command.clone()),
                other => SessionKind::from_name(other).ok_or_else(|| {
                    anyhow::anyhow!("profile '{}': unknown kind '{}'", profile.name, other)
                })?,
            };
            let cwd = expand_home(&profile_session.cwd);
            self.spawn_session(kind, profile_session.name.clone(), cwd)?;
            let session = self
                .sessions
                .last_mut()
                .expect("spawned session is present");
            session.group = profile_session.group.clone();
            ids.insert(session.name.clone(), session.id);
        }
        for profile_pipe in &profile.pipes {
            let source = ids.get(&profile_pipe.source).copied().ok_or_else(|| {
                anyhow::anyhow!(
                    "profile '{}': unknown source '{}'",
                    profile.name,
                    profile_pipe.source
                )
            })?;
            let dest = ids.get(&profile_pipe.dest).copied().ok_or_else(|| {
                anyhow::anyhow!(
                    "profile '{}': unknown destination '{}'",
                    profile.name,
                    profile_pipe.dest
                )
            })?;
            self.pipes.push(Pipe {
                source,
                dest,
                trigger: PipeTrigger::parse(&profile_pipe.trigger)?,
                extract: ExtractMode::parse(&profile_pipe.extract)?,
                prefix: profile_pipe.prefix.clone(),
                active: true,
                last_fired: None,
                condition: None,
            });
        }
        Ok(())
    }

    // ── Session management ─────────────────────────────────────────────────

    /// The command line a session kind launches with, per config overrides.
    pub fn resolved_command(&self, kind: &SessionKind) -> String {
        match kind {
            SessionKind::Claude => self.config.sessions.commands.claude.clone(),
            SessionKind::Codex => self.config.sessions.commands.codex.clone(),
            SessionKind::OpenCode => self.config.sessions.commands.opencode.clone(),
            SessionKind::OhMyPi => self.config.sessions.commands.ohmypi.clone(),
            SessionKind::Aider => self.config.sessions.commands.aider.clone(),
            SessionKind::Shell => {
                let c = &self.config.sessions.commands.shell;
                if c.is_empty() {
                    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
                } else {
                    c.clone()
                }
            }
            SessionKind::Custom(cmd) => cmd.clone(),
        }
    }

    pub fn spawn_session(
        &mut self,
        kind: SessionKind,
        name: String,
        cwd: String,
    ) -> anyhow::Result<()> {
        if self.visible_count() >= MAX_SESSIONS {
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
        let cmd_str = self.resolved_command(&kind);

        // Safety: refuse any command containing forbidden flags.
        config::validate_command(&cmd_str).map_err(|e| anyhow::anyhow!("{}", e))?;

        let (pty_rows, pty_cols) = self.pane_sizes[self.focused_pane];
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

        if self.panes[0].is_none() {
            self.panes[0] = Some(idx);
        }

        // Mint a capability token for this session. Interactive shells belong
        // to the human and keep full operator rights (so orchestrator scripts
        // run inside a linkshell shell can manage pipes / create sessions);
        // AI agent sessions are confined to worker capabilities.
        let token = crate::auth::mint_token();
        self.tokens.insert(token.clone(), id);
        let caps = if matches!(kind, SessionKind::Shell) {
            crate::auth::operator_caps()
        } else {
            crate::auth::worker_caps()
        };
        self.caps.insert(id, caps);

        let tx = self.event_tx.clone();
        let cfg = Arc::clone(&self.config);
        let socket = crate::ipc::socket_path(&self.config);

        // ── Resolve the session's real CLI identity ─────────────────────────
        // A "claude" session may be spelled many ways: the plain binary, an
        // env-prefixed command (CLAUDE_CONFIG_DIR=~/w claude), or an aliased
        // wrapper mapped in [sessions.aliases]. Resolve base kind + config
        // home here so the JSONL watcher tracks the right log directory.
        let alias = crate::session::command_base_name(&cmd_str)
            .and_then(|b| self.config.sessions.aliases.get(b))
            .cloned();
        let base = if kind.is_claude_based()
            || crate::session::command_base_name(&cmd_str)
                .map(crate::session::is_claude_basename)
                .unwrap_or(false)
            || alias.as_ref().map(|a| a.kind == "claude").unwrap_or(false)
        {
            crate::session::BaseKind::Claude
        } else if kind.is_codex_based()
            || crate::session::command_base_name(&cmd_str)
                .map(crate::session::is_codex_basename)
                .unwrap_or(false)
            || alias.as_ref().map(|a| a.kind == "codex").unwrap_or(false)
        {
            crate::session::BaseKind::Codex
        } else if crate::session::command_base_name(&cmd_str)
            .map(crate::session::is_local_agent_basename)
            .unwrap_or(false)
            || alias.as_ref().map(|a| a.kind == "local").unwrap_or(false)
        {
            crate::session::BaseKind::LocalAgent
        } else {
            crate::session::BaseKind::Other
        };
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
            s.base = base;
        }

        // Config home precedence: inline env prefix on the command wins, then
        // the alias table. When it comes from the alias table the CLI itself
        // hasn't been told, so also inject it into the PTY environment.
        let home_var = match base {
            crate::session::BaseKind::Claude => Some("CLAUDE_CONFIG_DIR"),
            crate::session::BaseKind::Codex => Some("CODEX_HOME"),
            crate::session::BaseKind::LocalAgent | crate::session::BaseKind::Other => None,
        };
        let mut inject_env: Vec<(String, String)> = Vec::new();
        let config_home = home_var.and_then(|var| {
            if let Some(inline) = crate::session::command_env_assignment(&cmd_str, var) {
                Some(expand_tilde(&inline))
            } else if let Some(dir) = alias.as_ref().and_then(|a| a.config_dir.clone()) {
                let dir = expand_tilde(&dir);
                inject_env.push((var.to_string(), dir.clone()));
                Some(dir)
            } else {
                None
            }
        });

        match base {
            crate::session::BaseKind::Claude => {
                crate::claude_log::spawn_watcher(
                    id,
                    cwd.clone(),
                    tx.clone(),
                    Arc::clone(&cfg),
                    config_home,
                );
            }
            crate::session::BaseKind::Codex => {
                crate::codex_log::spawn_watcher(
                    id,
                    cwd.clone(),
                    tx.clone(),
                    Arc::clone(&cfg),
                    config_home,
                );
            }
            crate::session::BaseKind::LocalAgent | crate::session::BaseKind::Other => {
                // OpenCode persists token/cost stats to its SQLite db; watch
                // it whether the session was created as the OpenCode kind or
                // as a custom command invoking the opencode binary.
                if matches!(kind, SessionKind::OpenCode)
                    || crate::session::command_base_name(&cmd_str) == Some("opencode")
                {
                    crate::opencode_log::spawn_watcher(id, cwd.clone(), tx.clone());
                    if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
                        s.stats_from_watcher = true;
                    }
                }
                // A custom command may still be a claude session behind a
                // wrapper name the classifier can't see through. Watch the
                // claude projects dir speculatively: if a new JSONL appears
                // for this cwd, the watcher upgrades the session's base to
                // Claude and tails it like a native claude session.
                if matches!(kind, SessionKind::Custom(_)) {
                    crate::claude_log::spawn_detecting_watcher(
                        id,
                        cwd.clone(),
                        tx.clone(),
                        Arc::clone(&cfg),
                        None,
                    );
                }
            }
        }

        let wrap_in_shell = !matches!(kind, SessionKind::Shell);
        tokio::spawn(async move {
            if let Err(e) = run_pty(
                id,
                cmd_str,
                cwd,
                pty_rows,
                pty_cols,
                tx.clone(),
                socket,
                token,
                wrap_in_shell,
                inject_env,
            )
            .await
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
        if self.visible_count() >= MAX_SESSIONS {
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
        let (pty_rows, pty_cols) = self.pane_sizes[self.focused_pane];
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
        let idx = self.sessions.len();
        self.sessions.push(session);
        if self.panes[0].is_none() {
            self.panes[0] = Some(idx);
        }
        Ok(id)
    }

    pub fn kill_active_session(&mut self) {
        if let Some(idx) = self.active_idx() {
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
        for pane in &mut self.panes {
            *pane = match *pane {
                Some(i) if i == idx => None,
                Some(i) if i > idx => Some(i - 1),
                other => other,
            };
        }
        if self.layout == LayoutMode::Single && self.panes[0].is_none() && !self.sessions.is_empty()
        {
            // Fall back to the nearest visible session, if any remain.
            let target = idx.min(self.sessions.len() - 1);
            self.panes[0] = self
                .visible_indices()
                .into_iter()
                .min_by_key(|i| i.abs_diff(target));
        }
    }

    pub fn switch_to(&mut self, idx: usize) {
        let other = self.focused_pane ^ 1;
        if idx < self.sessions.len()
            && !self.sessions[idx].hidden
            && (!self.is_split() || self.panes[other] != Some(idx))
        {
            self.panes[self.focused_pane] = Some(idx);
            let (rows, cols) = self.pane_sizes[self.focused_pane];
            let session = &mut self.sessions[idx];
            session.resize_screen(rows, cols);
            if let Some(tx) = &session.pty_resizer {
                let _ = tx.try_send((rows, cols));
            }
        }
    }

    pub fn next_session(&mut self) {
        let n = self.sessions.len();
        if n == 0 {
            return;
        }
        let start = self.active_idx().map_or(0, |i| (i + 1) % n);
        for offset in 0..n {
            let idx = (start + offset) % n;
            let before = self.active_idx();
            self.switch_to(idx);
            if self.active_idx() != before || before == Some(idx) {
                break;
            }
        }
    }

    pub fn prev_session(&mut self) {
        let n = self.sessions.len();
        if n == 0 {
            return;
        }
        let start = self
            .active_idx()
            .map_or(0, |i| if i == 0 { n - 1 } else { i - 1 });
        for offset in 0..n {
            let idx = (start + n - offset) % n;
            let before = self.active_idx();
            self.switch_to(idx);
            if self.active_idx() != before || before == Some(idx) {
                break;
            }
        }
    }

    pub fn is_split(&self) -> bool {
        self.layout != LayoutMode::Single
    }

    pub fn toggle_split(&mut self) {
        match self.layout {
            LayoutMode::Single => {
                // Reopen in whichever direction was used last (default SplitV).
                self.layout = self.last_split;
                if self.panes[1].is_none() {
                    self.panes[1] = self
                        .visible_indices()
                        .into_iter()
                        .find(|idx| Some(*idx) != self.panes[0]);
                }
            }
            LayoutMode::SplitV | LayoutMode::SplitH => {
                self.last_split = self.layout;
                self.panes[0] = self.active_idx();
                self.panes[1] = None;
                self.focused_pane = 0;
                self.layout = LayoutMode::Single;
            }
        }
        self.needs_redraw = true;
    }

    /// Flip an active split between side-by-side (SplitV) and stacked
    /// (SplitH). No-op in Single layout — the pane pair and focus are
    /// preserved; only the divider direction changes. The post-draw
    /// handle_pane_resize pass propagates the new geometry to session PTYs.
    pub fn rotate_split(&mut self) {
        match self.layout {
            LayoutMode::SplitV => self.layout = LayoutMode::SplitH,
            LayoutMode::SplitH => self.layout = LayoutMode::SplitV,
            LayoutMode::Single => return,
        }
        self.last_split = self.layout;
        self.needs_redraw = true;
    }

    pub fn focus_next_pane(&mut self) {
        if self.is_split() {
            self.focused_pane ^= 1;
            self.needs_redraw = true;
        }
    }

    /// Unified scrollback. Normal-screen apps (shells) use vt100's native
    /// scrollback; full-screen TUIs (claude, codex, opencode, ...) occupy the
    /// alternate screen where vt100 keeps none, so we scroll through our own
    /// captured `output_lines` history instead. Same keys, every session type.
    pub fn scroll_up(&mut self, lines: usize) {
        if let Some(idx) = self.active_idx() {
            if let Some(session) = self.sessions.get_mut(idx) {
                if session.screen.screen().alternate_screen() {
                    let max = session.output_lines.len();
                    session.history_scroll = (session.history_scroll + lines).min(max);
                } else {
                    let current = session.screen.screen().scrollback();
                    session.screen.set_scrollback(current + lines);
                }
            }
        }
    }

    pub fn scroll_down(&mut self, lines: usize) {
        if let Some(idx) = self.active_idx() {
            if let Some(session) = self.sessions.get_mut(idx) {
                if session.history_scroll > 0 {
                    session.history_scroll = session.history_scroll.saturating_sub(lines);
                } else {
                    let current = session.screen.screen().scrollback();
                    session.screen.set_scrollback(current.saturating_sub(lines));
                }
            }
        }
    }

    pub fn clear_scroll(&mut self) {
        if let Some(session) = self.active_session_mut() {
            session.screen.set_scrollback(0);
            session.history_scroll = 0;
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn scroll_offset(&self) -> usize {
        self.active_idx()
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.screen.screen().scrollback().max(s.history_scroll))
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
    pub fn handle_pane_resize(&mut self, sizes: [(u16, u16); 2]) {
        let mut changed = false;
        let mut visible: [Option<usize>; 2] = [None, None];
        for (pane_idx, size) in sizes.iter().copied().enumerate() {
            if self.layout == LayoutMode::Single && pane_idx == 1 {
                continue;
            }
            visible[pane_idx] = self.panes[pane_idx];
            if self.pane_sizes[pane_idx] == sizes[pane_idx] {
                continue;
            }
            self.pane_sizes[pane_idx] = size;
            changed = true;
            if let Some(session_idx) = self.panes[pane_idx] {
                if let Some(session) = self.sessions.get_mut(session_idx) {
                    let (rows, cols) = size;
                    session.resize_screen(rows, cols);
                    if let Some(tx) = &session.pty_resizer {
                        let _ = tx.try_send((rows, cols));
                    }
                }
            }
        }
        if !changed {
            return;
        }
        // Hidden sessions must track window resizes too, not just get one
        // deferred resize on switch_to: full-screen TUIs (claude, codex)
        // repaint on that late SIGWINCH so the deferral is invisible, but
        // line-oriented custom commands don't, and would surface cropped or
        // stale content laid out for the old size. Size them for the pane
        // they'd appear in when switched to — the focused one.
        let (rows, cols) = self.pane_sizes[self.focused_pane];
        for (idx, session) in self.sessions.iter_mut().enumerate() {
            if visible.contains(&Some(idx)) {
                continue;
            }
            session.resize_screen(rows, cols);
            if let Some(tx) = &session.pty_resizer {
                let _ = tx.try_send((rows, cols));
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn handle_resize(&mut self, rows: u16, cols: u16) {
        self.handle_pane_resize([(rows, cols), self.pane_sizes[1]]);
    }

    pub fn handle_session_bytes(&mut self, session_id: usize, data: Vec<u8>) {
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.process_bytes(&data);
        }
        // While the user is scrolled up, hold their position (tmux-style)
        // instead of yanking to the tail on every output burst — full-screen
        // TUIs redraw constantly, which previously made scrollback unusable
        // for them. Typing returns to the live view (see clear_scroll).
    }

    pub fn handle_session_output(&mut self, session_id: usize, line: String) {
        let mut state_before = None;
        let mut state_after = None;

        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            state_before = Some(session.state.clone());
            let stripped = strip_ansi(&line);
            // Log to file if configured
            if let Some(log_path) = &session.log_path {
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(log_path)
                {
                    use std::io::Write;
                    let _ = writeln!(file, "{}", stripped);
                }
            }
            session.push_output_line(stripped.clone());
            if let Some(new_state) = self.matcher.infer_state(&stripped, session.base) {
                // When JSONL is active it owns Thinking/Running/Ready transitions, but it
                // cannot see permission prompts or question text, so Waiting and Error must
                // still come from terminal pattern matching.
                if !session.ipc_state
                    || matches!(new_state, SessionState::Waiting | SessionState::Error)
                {
                    session.state = new_state;
                }
            }
            if state_before.as_ref() != Some(&session.state) {
                session.waiting_prompt = if session.state == SessionState::Waiting {
                    extract_waiting_prompt(&session.output_lines)
                } else {
                    None
                };
            }
            // Claude and Codex stats come from their JSONL watchers; skip terminal scraping.
            // Uses the resolved base identity so aliased/env-prefixed CLIs are
            // treated the same as plain `claude` / `codex`.
            // Only recognized local agents get line-scraped stats: the token
            // regexes are loose enough that arbitrary shell/command output
            // ("3 input files", "500 tokens") would fabricate usage for
            // sessions that never call an API. Other sessions can still
            // report real usage via the IPC tokens message. Local agents with
            // a dedicated watcher (opencode → sqlite db) skip scraping too.
            if session.base == crate::session::BaseKind::LocalAgent && !session.stats_from_watcher {
                if let Some(stats) = self.matcher.parse_tokens(&stripped) {
                    session.accumulate_stats(stats);
                }
            }
            state_after = Some(session.state.clone());
        }

        if let (Some(before), Some(after)) = (state_before, state_after) {
            if before != after {
                self.on_state_transition(session_id, &before, &after);
            }
        }
    }

    /// Everything that reacts to a session state transition. Every path that
    /// changes session state (complete lines, partial lines, IPC overrides)
    /// must funnel through here — a consumer wired into only some paths gets
    /// subtle misses, e.g. wait-ready replies never resolving because shell
    /// prompts arrive as partial lines.
    fn on_state_transition(
        &mut self,
        session_id: usize,
        before: &SessionState,
        after: &SessionState,
    ) {
        self.maybe_notify(session_id, before, after);
        self.notify_orchestrator(session_id, after);
        self.surface_permission_request(session_id, after);
        self.check_pipes(session_id, after);
        self.check_chat_pending(session_id, after);
        let council_relays = if let Some(router) = &mut self.council {
            router.on_state(&self.sessions, session_id, after)
        } else {
            vec![]
        };
        for (dest, payload) in council_relays {
            self.handle_pipe_relay(dest, payload);
        }
        self.check_ipc_replies(session_id, after);
        if *after == SessionState::Ready {
            self.flush_pending_relays(session_id);
        }
    }

    /// Wake the orchestrator agent on a notable session state change
    /// (per [orchestrator].events, default waiting/error/dead).
    fn notify_orchestrator(&mut self, session_id: usize, new_state: &SessionState) {
        if self.orchestrator.is_none() && self.orchestrator_session_id.is_none() {
            return;
        }
        if self.orchestrator_session_id == Some(session_id) {
            self.surface_orchestrator_prompt(session_id, new_state);
            return;
        }
        let state_key: &'static str = match new_state {
            SessionState::Starting => "starting",
            SessionState::Ready => "ready",
            SessionState::Thinking => "thinking",
            SessionState::Running => "running",
            SessionState::Waiting => "waiting",
            SessionState::Error => "error",
            SessionState::Dead => "dead",
        };
        if !self
            .config
            .orchestrator
            .events
            .iter()
            .any(|e| e == state_key)
        {
            return;
        }
        let cooldown = std::time::Duration::from_secs(self.config.orchestrator.event_cooldown_secs);
        if let Some(last) = self.orch_event_cooldowns.get(&(session_id, state_key)) {
            if last.elapsed() < cooldown {
                return;
            }
        }
        self.orch_event_cooldowns
            .insert((session_id, state_key), std::time::Instant::now());

        let Some(s) = self.sessions.iter().find(|s| s.id == session_id) else {
            return;
        };
        if s.is_orchestrator {
            return;
        }
        let name = s.name.clone();
        let kind = s.kind.label().to_string();
        let waiting_prompt = s.waiting_prompt.clone();
        let tail = pipe::extract_from_session(&self.sessions, session_id, &ExtractMode::LastN(15))
            .unwrap_or_default();

        if let Some(h) = &self.orchestrator {
            // try_send: a wedged orchestrator must never block the main loop.
            let dead = matches!(
                h.tx.try_send(crate::orchestrator::OrchestratorMsg::SessionEvent {
                    session_id,
                    name,
                    kind,
                    state: new_state.label().to_string(),
                    waiting_prompt,
                    tail,
                }),
                Err(mpsc::error::TrySendError::Closed(_))
            );
            if dead {
                self.orchestrator_gone();
            }
        } else if let Some(orch_id) = self.orchestrator_session_id {
            let prompt = waiting_prompt
                .map(|p| format!(" prompt: {}", p))
                .unwrap_or_default();
            let msg = format!(
                "[linkshell event] session {} \"{}\" ({}) is now {}.{} Investigate with linkshell-ctl and report to the user via `linkshell-ctl chat`.",
                session_id,
                name,
                kind,
                new_state.label(),
                prompt
            );
            self.handle_pipe_relay(orch_id, format!("\x1b[200~{}\x1b[201~\r", msg));
        }
    }

    /// The orchestrator session itself blocked on a prompt (permission
    /// dialog, y/n question) or errored. Nobody watches the watcher, and
    /// when hidden it has no session-bar slot to flag WAITING — surface it
    /// in the chat pane instead. A chat reply addressed to it ("@name 1",
    /// "@name y") types straight into its terminal, so prompts can be
    /// answered without ever showing the session.
    fn surface_orchestrator_prompt(&mut self, session_id: usize, new_state: &SessionState) {
        let state_key: &'static str = match new_state {
            SessionState::Waiting => "orch-self-waiting",
            SessionState::Error => "orch-self-error",
            _ => return,
        };
        let Some(s) = self.sessions.iter().find(|s| s.id == session_id) else {
            return;
        };
        if !s.hidden {
            return; // visible sessions already flag WAITING in the bar
        }
        let cooldown = std::time::Duration::from_secs(self.config.orchestrator.event_cooldown_secs);
        if let Some(last) = self.orch_event_cooldowns.get(&(session_id, state_key)) {
            if last.elapsed() < cooldown {
                return;
            }
        }
        self.orch_event_cooldowns
            .insert((session_id, state_key), std::time::Instant::now());
        let name = s.name.clone();
        let detail = s
            .waiting_prompt
            .clone()
            .unwrap_or_else(|| s.read_tail(3).join(" "));
        let msg = if *new_state == SessionState::Waiting {
            self.last_permission_request = Some(session_id);
            format!(
                "@{} needs input: {} — /yes or /no here answers it, \"@{} <answer>\" types anything else, /orchestrator show inspects",
                name, detail, name
            )
        } else {
            format!(
                "@{} hit an error: {} — /orchestrator show to inspect, /orchestrator restart to recover",
                name, detail
            )
        };
        self.chat_system(msg);
    }

    /// An AI session stopped on what looks like a permission / y-n prompt.
    /// Mirror it into the chat pane so it can be answered there (/yes, /no,
    /// or "@name <text>") without switching to the session.
    fn surface_permission_request(&mut self, session_id: usize, new_state: &SessionState) {
        if *new_state != SessionState::Waiting {
            return;
        }
        let Some(s) = self.sessions.iter().find(|s| s.id == session_id) else {
            return;
        };
        // The orchestrator's own prompts go through surface_orchestrator_prompt;
        // non-AI sessions (shells) wait at their prompt all the time.
        if s.is_orchestrator || s.base == crate::session::BaseKind::Other {
            return;
        }
        let Some(prompt) = s.waiting_prompt.clone() else {
            return;
        };
        let cooldown = std::time::Duration::from_secs(self.config.orchestrator.event_cooldown_secs);
        if let Some(last) = self.orch_event_cooldowns.get(&(session_id, "perm-request")) {
            if last.elapsed() < cooldown {
                return;
            }
        }
        self.orch_event_cooldowns
            .insert((session_id, "perm-request"), std::time::Instant::now());
        let name = s.name.clone();
        self.last_permission_request = Some(session_id);
        self.chat_system(format!(
            "@{} is asking: {} — /yes {} or /no {} answers it (bare /yes answers the latest request)",
            name, prompt, name, name
        ));
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
                if let Some(cond) = &p.condition {
                    if !content.to_lowercase().contains(&cond.to_lowercase()) {
                        continue;
                    }
                }
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
                if let Some(cond) = &p.condition {
                    if !content.to_lowercase().contains(&cond.to_lowercase()) {
                        continue;
                    }
                }
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
            if let Some(new_state) = self.matcher.infer_state(&stripped, session.base) {
                // Partial lines can detect Thinking/Waiting/Ready but must not
                // flip to Running — that requires a complete line.
                // Even with JSONL active, Waiting and Error must pass through because
                // JSONL has no record for permission prompts or question text.
                if new_state != SessionState::Running
                    && (!session.ipc_state
                        || matches!(new_state, SessionState::Waiting | SessionState::Error))
                {
                    session.state = new_state;
                }
            }
            if state_before.as_ref() != Some(&session.state) {
                session.waiting_prompt = if session.state == SessionState::Waiting {
                    let current = std::collections::VecDeque::from([stripped]);
                    extract_waiting_prompt(&current)
                        .or_else(|| extract_waiting_prompt(&session.output_lines))
                } else {
                    None
                };
            }
            state_after = Some(session.state.clone());
        }

        if let (Some(before), Some(after)) = (state_before, state_after) {
            if before != after {
                self.on_state_transition(session_id, &before, &after);
            }
        }
    }

    pub fn handle_session_stats(&mut self, session_id: usize, stats: crate::session::TokenStats) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            // JSONL watchers report cumulative totals. Only accept monotonically
            // increasing cost so a watcher re-reading a fresh log file can't
            // wipe accumulated spend back to zero.
            s.apply_reported_total(stats);
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
            session.waiting_prompt = if state == SessionState::Waiting {
                extract_waiting_prompt(&session.output_lines)
            } else {
                None
            };
            session.ipc_state = true;
            session.ipc_state_set_at = Some(std::time::Instant::now());
        }
        if old.as_ref() != Some(&state) {
            match old {
                Some(old) => self.on_state_transition(session_id, &old, &state),
                // No previous state (unknown session): run the hooks anyway,
                // passing the new state as "before" so notify sees no change.
                None => self.on_state_transition(session_id, &state.clone(), &state),
            }
        }
    }

    fn maybe_notify(&mut self, session_id: usize, old: &SessionState, new: &SessionState) {
        let config = &self.config.notifications;
        if !config.enabled || old == new {
            return;
        }
        let state_name = match new {
            SessionState::Waiting => "waiting",
            SessionState::Error => "error",
            SessionState::Starting => "starting",
            SessionState::Ready => "ready",
            SessionState::Thinking => "thinking",
            SessionState::Running => "running",
            SessionState::Dead => "dead",
        };
        if !config
            .on_states
            .iter()
            .any(|configured| configured.eq_ignore_ascii_case(state_name))
        {
            return;
        }
        let Some(session) = self
            .sessions
            .iter_mut()
            .find(|session| session.id == session_id)
        else {
            return;
        };
        if session.started_at.elapsed().as_secs() < config.min_session_age_secs {
            return;
        }
        if session
            .last_notified
            .is_some_and(|last| last.elapsed().as_secs() < config.debounce_secs)
        {
            return;
        }
        session.last_notified = Some(std::time::Instant::now());
        let title = format!("{} {}", session.name, new.label());
        let body = session
            .waiting_prompt
            .as_deref()
            .unwrap_or("needs attention")
            .to_string();
        crate::notify::notify(config.method, &title, &body);
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
            condition: None,
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

    /// Push a raw JSON line to a connected headless agent.
    pub fn handle_ipc_send(&self, session_id: usize, message: serde_json::Value) {
        if let Some(tx) = self.agent_writers.get(&session_id) {
            let line = serde_json::to_string(&message).unwrap_or_default() + "\n";
            let _ = tx.try_send(line);
        }
    }

    pub fn handle_broadcast(&self, group: &str, msg: serde_json::Value) {
        let members: Vec<usize> = self
            .sessions
            .iter()
            .filter(|s| s.group.as_deref() == Some(group))
            .map(|s| s.id)
            .collect();
        for id in members {
            self.handle_ipc_send(id, msg.clone());
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
        // Snapshot-based event must fire before the session is removed below.
        self.notify_orchestrator(session_id, &SessionState::Dead);
        if self.orchestrator_session_id == Some(session_id) {
            self.orchestrator_session_id = None;
            self.chat_system("orchestrator session died — /orchestrator restart to reconnect");
        }
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
            // Line-oriented sessions answer with the output since the input
            // was sent; alt-screen TUIs repaint in place (the line delta is
            // repaint noise), so return the rendered screen instead.
            let lines: Vec<String> = self
                .sessions
                .iter()
                .find(|s| s.id == session_id)
                .map(|s| {
                    if s.screen.screen().alternate_screen() {
                        s.read_tail(50)
                    } else {
                        s.output_lines.iter().skip(line_offset).cloned().collect()
                    }
                })
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
        /// "output:<id>" or "output:<id>:<n>" → (id, n); n defaults to 50.
        fn parse_output_query(what: &str) -> Option<(usize, usize)> {
            let rest = what.strip_prefix("output:")?;
            let mut parts = rest.splitn(2, ':');
            let sid = parts.next()?.parse().ok()?;
            let n = match parts.next() {
                Some(n) => n.parse().ok()?,
                None => 50,
            };
            Some((sid, n))
        }
        match payload {
            IpcQueryPayload::SessionCreate {
                kind_str,
                name,
                cwd,
            } => {
                let kind = match crate::session::SessionKind::from_name(kind_str.as_str()) {
                    Some(kind) => kind,
                    None => {
                        let _ = response_tx.send(serde_json::json!({
                            "error": format!("unknown session kind: {}", kind_str)
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
            IpcQueryPayload::Query { what } => {
                let resp = match what.as_str() {
                    "sessions" => self.sessions_json(),
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
                    what if what.starts_with("output:") => match parse_output_query(what) {
                        Some((sid, n)) => match self.sessions.iter().find(|s| s.id == sid) {
                            Some(s) => {
                                serde_json::json!({"session_id": sid, "lines": s.read_tail(n)})
                            }
                            None => serde_json::json!({"error": "session not found"}),
                        },
                        None => serde_json::json!({"error": "bad output query"}),
                    },
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

    /// Start the resident orchestrator agent per [orchestrator] config.
    /// API-class providers run as an in-process tool-loop task; CLI-class
    /// providers run the CLI in a session with operator-tier IPC capabilities.
    pub fn start_orchestrator(&mut self) -> anyhow::Result<()> {
        if self.orchestrator.is_some() || self.orchestrator_session_id.is_some() {
            anyhow::bail!("orchestrator already running");
        }
        let cfg = self.config.orchestrator.clone();
        match cfg.class()? {
            crate::config::OrchestratorClass::Api(_) => {
                if self.config.agents.contains_key(&cfg.name) {
                    self.chat_system(format!(
                        "note: [agents.{}] is shadowed by the orchestrator of the same name",
                        cfg.name
                    ));
                }
                self.orchestrator = Some(crate::orchestrator::spawn(cfg, self.event_tx.clone()));
            }
            crate::config::OrchestratorClass::Cli(kind_str) => {
                let kind = crate::session::SessionKind::from_name(kind_str)
                    .ok_or_else(|| anyhow::anyhow!("unknown session kind: {}", kind_str))?;
                // permission_mode: launch with the CLI's own safe auto-approval
                // flags so the orchestrator isn't stopped by routine prompts.
                let kind = match cfg.cli_permission_args(kind_str)? {
                    Some(args) => crate::session::SessionKind::Custom(format!(
                        "{} {}",
                        self.resolved_command(&kind),
                        args
                    )),
                    None => kind,
                };
                let id = self.next_id;
                self.spawn_session(kind, cfg.name.clone(), cfg.cwd.clone())?;
                // Upgrade from the default worker capset; the LINKSHELL_TOKEN
                // already in its environment now grants these caps.
                self.caps.insert(id, crate::auth::orchestrator_caps());
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
                    s.is_orchestrator = true;
                    s.hidden = cfg.hidden;
                }
                if cfg.hidden {
                    // spawn_session may have focused it (first session);
                    // hidden sessions must never occupy a pane.
                    let idx = self.sessions.iter().position(|s| s.id == id);
                    for pane in &mut self.panes {
                        if *pane == idx {
                            *pane = None;
                        }
                    }
                    if self.panes[0].is_none() {
                        self.panes[0] = self.visible_indices().first().copied();
                    }
                }
                self.orchestrator_session_id = Some(id);
                // Briefing lands once the CLI reaches READY. Bracketed paste
                // keeps the multi-line prompt from submitting line by line.
                let briefing = crate::orchestrator::cli_briefing(&cfg);
                self.handle_pipe_relay(id, format!("\x1b[200~{}\x1b[201~\r", briefing));
            }
        }
        Ok(())
    }

    /// Snapshot of all sessions as JSON — shared by the IPC `query sessions`
    /// path and the orchestrator's `list_sessions` tool.
    pub fn sessions_json(&self) -> serde_json::Value {
        // `display` is the 1-based number the user sees in the session bar:
        // position among visible sessions. Hidden sessions have none.
        let mut display = 0usize;
        let arr: Vec<_> = self
            .sessions
            .iter()
            .map(|s| {
                let display = if s.hidden {
                    serde_json::Value::Null
                } else {
                    display += 1;
                    serde_json::json!(display)
                };
                serde_json::json!({
                    "id":             s.id,
                    "display":        display,
                    "name":           s.name,
                    "kind":           s.kind.label(),
                    "state":          s.state.label(),
                    "waiting_prompt": s.waiting_prompt,
                    "group":          s.group,
                    "hidden":         s.hidden,
                    "cwd":            s.cwd,
                    "input_tokens":   s.stats.input_tokens,
                    "output_tokens":  s.stats.output_tokens,
                    "cost_usd":       s.stats.total_cost_usd,
                })
            })
            .collect();
        serde_json::Value::Array(arr)
    }

    /// Execute one orchestrator tool request synchronously. `RequestKill`
    /// never kills — it files a request the user must /confirm-kill.
    pub fn handle_orchestrator_request(
        &mut self,
        req: crate::events::OrchestratorReq,
        response_tx: tokio::sync::oneshot::Sender<serde_json::Value>,
    ) {
        use crate::events::OrchestratorReq;
        let resp = match req {
            OrchestratorReq::ListSessions => self.sessions_json(),
            OrchestratorReq::ReadOutput { session_id, lines } => {
                match self.sessions.iter().find(|s| s.id == session_id) {
                    Some(s) => {
                        serde_json::json!({"session_id": session_id, "lines": s.read_tail(lines)})
                    }
                    None => serde_json::json!({"error": "session not found"}),
                }
            }
            OrchestratorReq::StartSession {
                kind,
                name,
                cwd,
                initial_prompt,
            } => match crate::session::SessionKind::from_name(&kind) {
                Some(k) => {
                    let new_id = self.next_id;
                    match self.spawn_session(k, name, cwd) {
                        Ok(()) => {
                            if let Some(prompt) = initial_prompt {
                                // Shaped here (bracketed paste for multi-line)
                                // because pipe relay forwards it verbatim.
                                let msg = String::from_utf8(shape_injected_input(&prompt))
                                    .unwrap_or(prompt);
                                self.handle_pipe_relay(new_id, msg);
                            }
                            serde_json::json!({"session_id": new_id})
                        }
                        Err(e) => serde_json::json!({"error": e.to_string()}),
                    }
                }
                None => serde_json::json!({"error": format!("unknown session kind: {}", kind)}),
            },
            OrchestratorReq::SendInput {
                session_id,
                text,
                wait_ready,
            } => {
                let Some(s) = self.sessions.iter().find(|s| s.id == session_id) else {
                    let _ = response_tx.send(serde_json::json!({"error": "session not found"}));
                    return;
                };
                if wait_ready {
                    if self.pending_ipc_replies.contains_key(&session_id) {
                        let _ = response_tx.send(serde_json::json!({
                            "error": "session already has a pending input waiter"
                        }));
                        return;
                    }
                    let line_offset = s.output_lines.len();
                    s.write_bytes(shape_injected_input(&text));
                    self.pending_ipc_replies
                        .insert(session_id, (response_tx, line_offset));
                    return; // reply arrives via check_ipc_replies on READY
                }
                s.write_bytes(shape_injected_input(&text));
                serde_json::json!({"ok": true})
            }
            OrchestratorReq::PipeAdd {
                source,
                dest,
                trigger,
                extract,
                prefix,
            } => {
                self.handle_ipc_pipe_add(source, dest, &trigger, &extract, prefix);
                serde_json::json!({"ok": true})
            }
            OrchestratorReq::PipeRemove { source, dest } => {
                self.handle_ipc_pipe_remove(source, dest);
                serde_json::json!({"ok": true})
            }
            OrchestratorReq::RequestKill { session_id, reason } => {
                self.file_kill_request(session_id, reason)
            }
        };
        let _ = response_tx.send(resp);
    }

    /// File a kill request for human confirmation and announce it in chat.
    fn file_kill_request(&mut self, session_id: usize, reason: String) -> serde_json::Value {
        if self.orchestrator_session_id == Some(session_id) {
            return serde_json::json!({"error": "the orchestrator cannot request its own kill"});
        }
        let Some((idx, s)) = self
            .sessions
            .iter()
            .enumerate()
            .find(|(_, s)| s.id == session_id)
        else {
            return serde_json::json!({"error": "session not found"});
        };
        let name = s.name.clone();
        // The number the user sees in the session bar, falling back to the
        // raw id for sessions with no bar slot.
        let display = self
            .visible_indices()
            .iter()
            .position(|&i| i == idx)
            .map_or(session_id + 1, |p| p + 1);
        self.pending_kill = Some(PendingKill {
            session_id,
            session_name: name.clone(),
            reason: reason.clone(),
            requested_at: std::time::Instant::now(),
        });
        let why = if reason.is_empty() {
            String::new()
        } else {
            format!(" — reason: {}", reason)
        };
        self.chat_system(format!(
            "agent requests killing session {} \"{}\"{}. /confirm-kill to approve, /deny-kill to refuse.",
            display,
            name,
            why
        ));
        serde_json::json!({"status": "pending_user_confirmation", "session_id": session_id})
    }

    /// A gated tool call arrived from the orchestrator (propose mode).
    pub fn handle_orchestrator_proposal(
        &mut self,
        tool: String,
        detail: String,
        response_tx: tokio::sync::oneshot::Sender<crate::events::ProposalVerdict>,
    ) {
        // The orchestrator blocks per proposal, so two pending at once means
        // the first was orphaned (e.g. restart); dropping its sender resolves
        // it as denied on whatever still awaits it.
        self.pending_proposal = Some(PendingProposal {
            tool: tool.clone(),
            detail: detail.clone(),
            response_tx: Some(response_tx),
        });
        self.chat_system(format!(
            "agent proposes {}: {} — /approve to run, /deny [reason] to refuse",
            tool, detail
        ));
        if self.mode != AppMode::Chat {
            self.command_result = format!(
                "orchestrator proposes {} — open chat (Alt+T) to review",
                tool
            );
            self.mode = AppMode::CommandResult;
        }
        self.needs_redraw = true;
    }

    /// Resolve the pending proposal. The verdict unblocks the orchestrator's
    /// tool call; a deny reason is returned to the model as the tool result.
    pub fn resolve_pending_proposal(&mut self, approve: bool, reason: String) {
        let Some(mut p) = self.pending_proposal.take() else {
            self.command_result = "no pending proposal".to_string();
            return;
        };
        let verdict = if approve {
            crate::events::ProposalVerdict::Approved
        } else {
            crate::events::ProposalVerdict::Denied(reason.clone())
        };
        let delivered = p
            .response_tx
            .take()
            .map(|tx| tx.send(verdict).is_ok())
            .unwrap_or(false);
        if !delivered {
            self.chat_system(format!(
                "proposal for {} had already expired on the agent side",
                p.tool
            ));
            return;
        }
        let note = if approve {
            format!("approved {}: {}", p.tool, p.detail)
        } else if reason.trim().is_empty() {
            format!("denied {}", p.tool)
        } else {
            format!("denied {} — {}", p.tool, reason)
        };
        self.command_result = note.clone();
        self.chat_system(note);
        self.needs_redraw = true;
    }

    pub fn handle_orchestrator_usage(&mut self, input: u64, output: u64) {
        self.orchestrator_stats.input_tokens += input;
        self.orchestrator_stats.output_tokens += output;
    }

    /// A line posted into the chat pane via IPC `chat_post`. If it came from
    /// the CLI orchestrator session, drop any pending PTY-scrape reply for it
    /// so the answer isn't double-posted.
    pub fn handle_ipc_chat_post(&mut self, from_session_id: Option<usize>, text: String) {
        let from = from_session_id
            .and_then(|sid| self.sessions.iter().find(|s| s.id == sid))
            .map(|s| s.name.clone())
            .unwrap_or_else(|| "ctl".to_string());
        if let Some(sid) = from_session_id {
            self.chat.pending.retain(|p| p.session_id != sid);
        }
        self.chat.messages.push(ChatMsg { from, text });
        self.needs_redraw = true;
    }

    /// Resolve an IPC connection's identity: returns (session_id, caps) or None for rejection.
    pub fn handle_authenticate(
        &mut self,
        token: Option<String>,
        transport: crate::ipc::Transport,
        name: Option<String>,
        group: Option<String>,
        response_tx: tokio::sync::oneshot::Sender<Option<(Option<usize>, CapSet)>>,
    ) {
        use crate::ipc::Transport;

        let result: Option<(Option<usize>, CapSet)> = if let Some(tok) = token {
            if let Some(&sid) = self.tokens.get(&tok) {
                let caps = self
                    .caps
                    .get(&sid)
                    .cloned()
                    .unwrap_or_else(crate::auth::worker_caps);
                Some((Some(sid), caps))
            } else {
                None
            }
        } else if transport == Transport::Unix {
            if name.as_ref().map(|n| !n.is_empty()).unwrap_or(false) {
                match self.spawn_headless_session(name.unwrap_or_default(), group) {
                    Ok(id) => {
                        let caps = crate::auth::operator_caps();
                        self.caps.insert(id, caps.clone());
                        Some((Some(id), caps))
                    }
                    Err(_) => Some((None, crate::auth::operator_caps())),
                }
            } else {
                Some((None, crate::auth::operator_caps()))
            }
        } else {
            // TCP with no token → reject
            None
        };

        let _ = response_tx.send(result);
    }

    pub fn handle_tick(&mut self) {
        self.check_orchestrator_alive();
        if let Some(p) = &self.pending_proposal {
            let expired = p
                .response_tx
                .as_ref()
                .map(|tx| tx.is_closed())
                .unwrap_or(true);
            if expired {
                let tool = p.tool.clone();
                self.pending_proposal = None;
                self.chat_system(format!("proposal for {} timed out and was denied", tool));
                self.needs_redraw = true;
            }
        }
        if self.orchestrator_status.is_some() {
            if self.orchestrator.is_none() {
                self.orchestrator_status = None;
            }
            // Keep the chat-pane spinner animating while a turn is running.
            if self.mode == AppMode::Chat {
                self.needs_redraw = true;
            }
        }
        // (session id, state before the tick flipped it to Ready). Routed
        // through on_state_transition below — the single funnel for state
        // changes — so tick-detected completions reach every consumer
        // (orchestrator events, notifications, pipes, council, ...) instead
        // of a hand-copied subset that silently drifts.
        let mut tick_ready: Vec<(usize, SessionState)> = Vec::new();

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
                    let before = session.state.clone();
                    session.state = SessionState::Ready;
                    tick_ready.push((session.id, before));
                }
            }
            if !session.ipc_state {
                if let Some(last) = session.last_output_at {
                    let elapsed = last.elapsed();
                    if (elapsed > Duration::from_secs(2)
                        && matches!(
                            session.state,
                            SessionState::Running | SessionState::Thinking
                        ))
                        || (elapsed > Duration::from_secs(30)
                            && session.state == SessionState::Waiting)
                    {
                        let before = session.state.clone();
                        session.state = SessionState::Ready;
                        tick_ready.push((session.id, before));
                    }
                }
            }
        }

        for (id, before) in tick_ready {
            self.on_state_transition(id, &before, &SessionState::Ready);
        }
    }

    // ── Mouse handling ─────────────────────────────────────────────────────

    pub fn handle_mouse(&mut self, ev: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};

        let col = ev.column;
        let row = ev.row;

        if matches!(self.mode, AppMode::Menu { .. }) {
            self.handle_menu_mouse(ev.kind, col, row);
            return;
        }

        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                match self.mode.clone() {
                    AppMode::Chat => {
                        if rect_hit(self.chat_transcript_area, col, row) {
                            // Begin transcript selection (borderless rect)
                            let c = col - self.chat_transcript_area.x;
                            let r = row - self.chat_transcript_area.y;
                            self.chat_selection = Some(Selection {
                                start_col: c,
                                start_row: r,
                                end_col: c,
                                end_row: r,
                            });
                        } else if !rect_hit(self.chat_area, col, row) {
                            self.chat_selection = None;
                            self.mode = AppMode::Normal;
                        } else {
                            self.chat_selection = None;
                        }
                        return;
                    }
                    AppMode::Help => {
                        if !rect_hit(self.help_area, col, row) {
                            self.mode = AppMode::Normal;
                        }
                        return;
                    }
                    AppMode::NewSession => {
                        self.handle_new_session_mouse(col, row);
                        return;
                    }
                    AppMode::FileBrowser => {
                        self.handle_file_browser_mouse(
                            col,
                            row,
                            crate::ui::FILE_BROWSER_VISIBLE_ROWS,
                        );
                        return;
                    }
                    AppMode::CommandBar => {
                        if rect_hit(self.command_bar_area, col, row) {
                            self.set_command_cursor_from_col(col);
                        } else {
                            self.mode = AppMode::Normal;
                        }
                        return;
                    }
                    AppMode::CommandResult => {
                        self.mode = AppMode::Normal;
                        return;
                    }
                    AppMode::PipeList => {
                        self.mode = AppMode::Normal;
                        return;
                    }
                    AppMode::Search { .. } => {
                        self.mode = AppMode::Normal;
                        return;
                    }
                    AppMode::Settings => {
                        self.mode = AppMode::Normal;
                        return;
                    }
                    AppMode::Normal | AppMode::Menu { .. } => {}
                }

                // Session bar click → switch session (slots cover visible
                // sessions only, so map the slot position to a real index)
                for (i, slot) in self.session_slot_areas.iter().enumerate() {
                    if rect_hit(*slot, col, row) {
                        if let Some(idx) = self.visible_to_idx(i) {
                            self.switch_to(idx);
                        }
                        self.selection = None;
                        return;
                    }
                }
                // Output area click → begin selection
                if let Some((pane, area)) = self.output_area_at(col, row) {
                    self.focused_pane = pane;
                    let (c, r) = to_content_coords(area, col, row);
                    self.selection = Some(Selection {
                        start_col: c,
                        start_row: r,
                        end_col: c,
                        end_row: r,
                    });
                }
                for (i, row_area) in self.status_row_areas.iter().enumerate() {
                    if rect_hit(*row_area, col, row) {
                        if let Some(idx) = self.visible_to_idx(i) {
                            self.switch_to(idx);
                        }
                        self.selection = None;
                        return;
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if matches!(self.mode, AppMode::Chat) => {
                let area = self.chat_transcript_area;
                if let Some(sel) = &mut self.chat_selection {
                    sel.end_col = col.saturating_sub(area.x).min(area.width.saturating_sub(1));
                    sel.end_row = row
                        .saturating_sub(area.y)
                        .min(area.height.saturating_sub(1));
                }
            }
            MouseEventKind::Up(MouseButton::Left) if matches!(self.mode, AppMode::Chat) => {
                // Finalize transcript selection; auto-copy like the panes
                if let Some(sel) = &self.chat_selection {
                    let ((mr, mc), (er, ec)) = sel.normalized();
                    if mr == er && mc == ec {
                        self.chat_selection = None;
                    } else {
                        self.copy_chat_selection();
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right)
                if matches!(self.mode, AppMode::Chat) && self.chat_selection.is_some() =>
            {
                self.copy_chat_selection();
            }
            MouseEventKind::Drag(MouseButton::Left)
                if self
                    .output_areas
                    .iter()
                    .any(|area| rect_inner_hit(*area, col, row)) =>
            {
                let area = self.output_areas[self.focused_pane];
                let (c, r) = to_content_coords(area, col, row);
                if let Some(sel) = &mut self.selection {
                    sel.end_col = c;
                    sel.end_row = r;
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
            MouseEventKind::Down(MouseButton::Right) if self.selection.is_some() => {
                self.copy_selection();
            }
            MouseEventKind::ScrollUp => {
                if matches!(self.mode, AppMode::Chat) {
                    if rect_hit(self.chat_area, col, row) {
                        self.chat_scroll_up(3);
                    }
                } else if matches!(self.mode, AppMode::NewSession) {
                    self.new_session_select_kind(-1);
                } else if self.focus_pane_at(col, row) {
                    self.scroll_up(3);
                } else if rect_hit(self.session_bar_area, col, row) {
                    self.prev_session();
                }
            }
            MouseEventKind::ScrollDown => {
                if matches!(self.mode, AppMode::Chat) {
                    if rect_hit(self.chat_area, col, row) {
                        self.chat_scroll_down(3);
                    }
                } else if matches!(self.mode, AppMode::NewSession) {
                    self.new_session_select_kind(1);
                } else if self.focus_pane_at(col, row) {
                    self.scroll_down(3);
                } else if rect_hit(self.session_bar_area, col, row) {
                    self.next_session();
                }
            }
            _ => {}
        }
    }

    fn output_area_at(&self, col: u16, row: u16) -> Option<(usize, Rect)> {
        self.output_areas
            .iter()
            .copied()
            .enumerate()
            .find(|(_, area)| rect_hit(*area, col, row))
    }

    fn focus_pane_at(&mut self, col: u16, row: u16) -> bool {
        if let Some((pane, _)) = self.output_area_at(col, row) {
            self.focused_pane = pane;
            true
        } else {
            false
        }
    }

    fn handle_new_session_mouse(&mut self, col: u16, row: u16) {
        let inner = inset_rect(self.new_session_area, 1);

        // The expanded dropdown list overlays the fields below the Kind row
        // (and can extend past the dialog border), so hit-test it first.
        if self.new_session_state.kind_dropdown_open {
            let list = crate::ui::kind_dropdown_list_rect(self.new_session_area);
            if rect_hit(list, col, row) {
                let idx = row.saturating_sub(list.y + 1) as usize; // skip top border
                if row > list.y && idx < crate::session::SessionKind::COUNT {
                    self.new_session_state.selected_kind = idx;
                }
            }
            self.new_session_state.kind_dropdown_open = false;
            return;
        }

        if !rect_hit(self.new_session_area, col, row) {
            self.mode = AppMode::Normal;
            return;
        }

        // Kind dropdown field (full-width, top row of the dialog)
        let kind_field = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 3,
        };
        if rect_hit(kind_field, col, row) {
            self.new_session_state.active_field = NewSessionField::Kind;
            self.new_session_state.kind_dropdown_open = true;
            return;
        }

        let fields_y = inner.y + 3;
        let name = Rect {
            x: inner.x,
            y: fields_y,
            width: inner.width,
            height: 3,
        };
        let cwd = Rect {
            x: inner.x,
            y: fields_y + 3,
            width: inner.width,
            height: 3,
        };
        let custom = Rect {
            x: inner.x,
            y: fields_y + 6,
            width: inner.width,
            height: 3,
        };

        if rect_hit(name, col, row) {
            self.new_session_state.active_field = NewSessionField::Name;
            self.new_session_state.name_cursor =
                byte_index_for_col(&self.new_session_state.name, input_col(name, col));
        } else if rect_hit(self.browse_button_area, col, row) {
            self.open_file_browser();
        } else if rect_hit(cwd, col, row) {
            self.new_session_state.active_field = NewSessionField::Cwd;
            self.new_session_state.cwd_cursor =
                byte_index_for_col(&self.new_session_state.cwd, input_col(cwd, col));
        } else if self.new_session_state.is_custom() && rect_hit(custom, col, row) {
            self.new_session_state.active_field = NewSessionField::CustomCmd;
            self.new_session_state.custom_cmd_cursor =
                byte_index_for_col(&self.new_session_state.custom_cmd, input_col(custom, col));
        }
    }

    fn handle_menu_mouse(&mut self, kind: crossterm::event::MouseEventKind, col: u16, row: u16) {
        use crossterm::event::{MouseButton, MouseEventKind};

        match kind {
            MouseEventKind::Down(MouseButton::Left) => {
                for (idx, area) in self.menu_item_areas.iter().enumerate() {
                    if rect_hit(*area, col, row) {
                        self.mode = AppMode::Menu {
                            selected_top: idx,
                            selected_sub: Some(0),
                        };
                        return;
                    }
                }
                if let AppMode::Menu {
                    selected_top,
                    selected_sub: Some(_),
                } = self.mode
                {
                    for (idx, area) in self.menu_submenu_item_areas.iter().enumerate() {
                        if rect_hit(*area, col, row) {
                            self.execute_menu_action(selected_top, idx);
                            return;
                        }
                    }
                }
                if !rect_hit(self.menu_bar_area, col, row)
                    && !rect_hit(self.menu_submenu_area, col, row)
                {
                    self.mode = AppMode::Normal;
                }
            }
            MouseEventKind::ScrollUp => self.menu_move_sub(-1),
            MouseEventKind::ScrollDown => self.menu_move_sub(1),
            _ => {}
        }
    }

    fn selected_text(&self) -> Option<String> {
        let sel = self.selection.as_ref()?;
        let session = self.active_session()?;
        let screen = session.screen.screen();
        let (screen_rows, screen_cols) = screen.size();
        let display_rows = self
            .output_areas
            .get(self.focused_pane)
            .copied()
            .unwrap_or_default()
            .height
            .saturating_sub(2);
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

    /// Text covered by the chat transcript selection, from the plain-text
    /// snapshot of the rows visible in the last draw.
    fn chat_selected_text(&self) -> Option<String> {
        let sel = self.chat_selection.as_ref()?;
        let ((min_row, min_col), (max_row, max_col)) = sel.normalized();
        let mut out: Vec<String> = Vec::new();
        for row in min_row..=max_row {
            let Some(line) = self.chat_visible_lines.get(row as usize) else {
                continue;
            };
            let chars: Vec<char> = line.chars().collect();
            let start = if row == min_row { min_col as usize } else { 0 };
            let end = if row == max_row {
                (max_col as usize + 1).min(chars.len())
            } else {
                chars.len()
            };
            let slice: String = chars
                .get(start.min(chars.len())..end)
                .unwrap_or(&[])
                .iter()
                .collect();
            out.push(slice.trim_end().to_string());
        }
        Some(out.join("\n"))
    }

    fn copy_chat_selection(&mut self) {
        if let Some(text) = self.chat_selected_text().filter(|t| !t.trim().is_empty()) {
            self.copy_text(text);
        }
    }

    fn copy_selection(&mut self) {
        let Some(text) = self.selected_text().filter(|t| !t.is_empty()) else {
            return;
        };
        self.copy_text(text);
    }

    fn copy_text(&mut self, text: String) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        if let Some(cb) = self.clipboard.as_mut() {
            // PRIMARY selection first, so middle-click paste works on Linux.
            #[cfg(target_os = "linux")]
            {
                use arboard::{LinuxClipboardKind, SetExtLinux};
                let _ = cb
                    .set()
                    .clipboard(LinuxClipboardKind::Primary)
                    .text(text.clone());
            }
            let _ = cb.set_text(text);
        }
    }

    // ── New session dialog ─────────────────────────────────────────────────

    pub fn open_new_session(&mut self) {
        self.new_session_state = NewSessionState::default();
        self.mode = AppMode::NewSession;
    }

    pub fn new_session_tab(&mut self) {
        use NewSessionField::*;
        let is_custom = self.new_session_state.is_custom();
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
        let count = crate::session::SessionKind::COUNT as i32;
        self.new_session_state.selected_kind = ((cur + delta).rem_euclid(count)) as usize;
        self.new_session_state.active_field = NewSessionField::Kind;
    }

    pub fn confirm_new_session(&mut self) -> anyhow::Result<()> {
        let ns = self.new_session_state.clone();
        let kind = SessionKind::from_index(ns.selected_kind, &ns.custom_cmd);
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

    // ── File browser ───────────────────────────────────────────────────────

    pub fn open_file_browser(&mut self) {
        let start = if self.new_session_state.cwd.is_empty() {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string())
        } else {
            self.new_session_state.cwd.clone()
        };
        self.file_browser_state = FileBrowserState::new(&start);
        self.mode = AppMode::FileBrowser;
    }

    pub fn file_browser_up(&mut self) {
        if self.file_browser_state.selected > 0 {
            self.file_browser_state.selected -= 1;
            if self.file_browser_state.selected < self.file_browser_state.scroll_offset {
                self.file_browser_state.scroll_offset = self.file_browser_state.selected;
            }
        }
    }

    pub fn file_browser_down(&mut self, visible_rows: usize) {
        let max = self.file_browser_state.entries.len().saturating_sub(1);
        if self.file_browser_state.selected < max {
            self.file_browser_state.selected += 1;
            let bottom = self.file_browser_state.scroll_offset + visible_rows;
            if self.file_browser_state.selected >= bottom {
                self.file_browser_state.scroll_offset =
                    self.file_browser_state.selected + 1 - visible_rows;
            }
        }
    }

    pub fn file_browser_enter(&mut self) {
        let sel = self.file_browser_state.selected;
        if sel >= self.file_browser_state.entries.len() {
            return;
        }
        let path = self.file_browser_state.entries[sel].clone();
        if path.is_dir() {
            self.file_browser_state.current_dir = path;
            self.file_browser_state.selected = 0;
            self.file_browser_state.scroll_offset = 0;
            self.file_browser_state.refresh();
        }
    }

    pub fn file_browser_select(&mut self) {
        let s = self
            .file_browser_state
            .current_dir
            .to_string_lossy()
            .to_string();
        self.new_session_state.cwd = s.clone();
        self.new_session_state.cwd_cursor = s.len();
        self.new_session_state.active_field = NewSessionField::Cwd;
        self.mode = AppMode::NewSession;
    }

    pub fn file_browser_cancel(&mut self) {
        self.mode = AppMode::NewSession;
    }

    pub fn handle_file_browser_mouse(&mut self, col: u16, row: u16, list_visible: usize) {
        if !rect_hit(self.file_browser_area, col, row) {
            self.file_browser_cancel();
            return;
        }
        // inner area starts 2 rows below top border (border + current-dir line)
        let inner_y = self.file_browser_area.y + 3;
        if row < inner_y {
            return;
        }
        let idx = (row - inner_y) as usize + self.file_browser_state.scroll_offset;
        if idx < self.file_browser_state.entries.len() {
            self.file_browser_state.selected = idx;
        }
        let _ = list_visible;
    }

    // ── Command bar ────────────────────────────────────────────────────────

    pub fn open_command_bar(&mut self) {
        self.command_input.clear();
        self.command_cursor = 0;
        self.mode = AppMode::CommandBar;
        self.refresh_command_palette();
    }

    pub fn command_input_char(&mut self, c: char) {
        self.command_input.insert(self.command_cursor, c);
        self.command_cursor += c.len_utf8();
        self.refresh_command_palette();
    }

    pub fn command_backspace(&mut self) {
        if self.command_cursor > 0 {
            let prev = self.command_input[..self.command_cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.command_input.remove(prev);
            self.command_cursor = prev;
            self.refresh_command_palette();
        }
    }

    pub fn command_palette_move(&mut self, delta: isize) {
        if self.palette.matches.is_empty() {
            self.palette.selected = 0;
            return;
        }
        self.palette.selected = (self.palette.selected as isize + delta)
            .clamp(0, self.palette.matches.len() as isize - 1)
            as usize;
    }

    pub fn command_palette_insert_selected(&mut self) {
        if let Some(entry) = self.palette.matches.get(self.palette.selected) {
            self.command_input = entry.insert.clone();
            self.command_cursor = self.command_input.len();
            self.refresh_command_palette();
        }
    }

    fn refresh_command_palette(&mut self) {
        let query = self.command_input.trim().to_lowercase();
        let mut matches: Vec<(i32, PaletteEntry)> = COMMAND_PALETTE
            .iter()
            .filter_map(|(template, summary, insert)| {
                fuzzy_score(&query, &template.to_lowercase()).map(|score| {
                    (
                        score,
                        PaletteEntry {
                            template: (*template).into(),
                            summary: (*summary).into(),
                            insert: (*insert).into(),
                        },
                    )
                })
            })
            .collect();

        if self.command_input.starts_with("pipe ") && self.sessions.len() >= 2 {
            for source in &self.sessions {
                for dest in &self.sessions {
                    if source.id != dest.id {
                        let insert = format!("pipe {} {} ", source.name, dest.name);
                        matches.push((
                            10_000,
                            PaletteEntry {
                                template: insert.trim_end().into(),
                                summary: "Wire these sessions".into(),
                                insert,
                            },
                        ));
                    }
                }
            }
        }
        matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.template.cmp(&b.1.template)));
        self.palette.matches = matches.into_iter().map(|(_, entry)| entry).collect();
        self.palette.selected = self
            .palette
            .selected
            .min(self.palette.matches.len().saturating_sub(1));
    }

    pub fn command_cursor_left(&mut self) {
        if self.command_cursor > 0 {
            self.command_cursor = self.command_input[..self.command_cursor]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    pub fn command_cursor_right(&mut self) {
        if self.command_cursor < self.command_input.len() {
            let ch = self.command_input[self.command_cursor..]
                .chars()
                .next()
                .unwrap();
            self.command_cursor += ch.len_utf8();
        }
    }

    pub fn command_cursor_home(&mut self) {
        self.command_cursor = 0;
    }

    pub fn command_cursor_end(&mut self) {
        self.command_cursor = self.command_input.len();
    }

    fn set_command_cursor_from_col(&mut self, col: u16) {
        let content_col = col.saturating_sub(self.command_bar_area.x + 2) as usize;
        self.command_cursor = byte_index_for_col(&self.command_input, content_col);
    }

    pub fn execute_command(&mut self) {
        let cmd = self.command_input.trim().to_string();
        self.mode = AppMode::Normal;
        self.command_input.clear();
        self.command_cursor = 0;

        // Pipe commands need the full token list; handle before the splitn block.
        let all_parts: Vec<&str> = cmd.split_whitespace().collect();
        match all_parts.first().copied() {
            Some("pipes") => {
                self.open_pipe_list();
                return;
            }
            Some("pipe") => {
                self.execute_pipe_command(&all_parts[1..]);
                return;
            }
            Some("unpipe") => {
                self.execute_unpipe_command(&all_parts[1..]);
                return;
            }
            Some("council") => {
                self.execute_council_command(&all_parts[1..]);
                return;
            }
            Some("profile") => {
                self.execute_profile_command(&all_parts[1..]);
                return;
            }
            Some("config") => {
                self.execute_config_command(&all_parts[1..]);
                return;
            }
            Some("grant") => {
                self.execute_grant_command(&all_parts[1..]);
                return;
            }
            Some("restart") => {
                self.execute_restart_command(&all_parts[1..]);
                return;
            }
            Some("move") => {
                self.execute_move_command(&all_parts[1..]);
                return;
            }
            Some("rename") => {
                self.execute_rename_command(&all_parts[1..]);
                return;
            }
            Some("log") => {
                self.execute_log_command(&all_parts[1..]);
                return;
            }
            Some("broadcast") => {
                self.broadcast_mode = !self.broadcast_mode;
                self.command_result = if self.broadcast_mode {
                    "Broadcast mode ON".to_string()
                } else {
                    "Broadcast mode OFF".to_string()
                };
                self.mode = AppMode::CommandResult;
                return;
            }
            Some("search") => {
                let query = all_parts[1..].join(" ");
                self.mode = AppMode::Search {
                    query: query.clone(),
                    cursor: query.len(),
                    matches: vec![],
                    selected: 0,
                };
                self.search_update_matches();
                return;
            }
            Some("settings") => {
                self.open_settings();
                return;
            }
            Some("detach") => {
                let _ = self.event_tx.try_send(AppEvent::Detach);
                return;
            }
            _ => {}
        }

        let parts: Vec<&str> = cmd.splitn(3, ' ').collect();
        match parts.as_slice() {
            // `new custom <full command line>` — the remainder is the command,
            // spaces and env prefixes included (name is auto-assigned).
            ["new", "custom", rest @ ..] if !rest.is_empty() => {
                let command = rest.first().unwrap_or(&"").to_string();
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| "~".to_string());
                let _ = self.spawn_session(SessionKind::Custom(command), String::new(), cwd);
            }
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
                    if let Some(idx) = num.checked_sub(1).and_then(|p| self.visible_to_idx(p)) {
                        self.remove_session(idx);
                    }
                }
            }
            ["quit"] | ["q"] => self.should_quit = true,
            ["orchestrator", "start"] => {
                self.command_result = match self.start_orchestrator() {
                    Ok(()) => "orchestrator started".to_string(),
                    Err(e) => format!("orchestrator: {}", e),
                };
            }
            ["orchestrator", "restart"] => {
                // Tear down whichever flavor is running, then start fresh.
                self.orchestrator = None;
                if let Some(orch_id) = self.orchestrator_session_id.take() {
                    if let Some(idx) = self.sessions.iter().position(|s| s.id == orch_id) {
                        self.remove_session(idx);
                    }
                }
                self.command_result = match self.start_orchestrator() {
                    Ok(()) => "orchestrator restarted".to_string(),
                    Err(e) => format!("orchestrator: {}", e),
                };
            }
            ["orchestrator", "stop"] => {
                self.command_result = if self.orchestrator.take().is_some() {
                    // Dropping the handle closes the channel; the task exits.
                    "orchestrator stopped".to_string()
                } else if let Some(orch_id) = self.orchestrator_session_id.take() {
                    if let Some(idx) = self.sessions.iter().position(|s| s.id == orch_id) {
                        self.remove_session(idx);
                    }
                    "orchestrator session killed".to_string()
                } else {
                    "orchestrator is not running".to_string()
                };
            }
            ["orchestrator", vis @ ("show" | "hide")] => {
                let show = *vis == "show";
                self.command_result = match self.orchestrator_session_id {
                    Some(orch_id) => {
                        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == orch_id) {
                            s.hidden = !show;
                        }
                        if !show {
                            // Move any pane showing it to another session.
                            let idx = self.sessions.iter().position(|s| s.id == orch_id);
                            for pane in &mut self.panes {
                                if *pane == idx {
                                    *pane = None;
                                }
                            }
                            if self.panes[0].is_none() {
                                self.panes[0] = self.visible_indices().first().copied();
                            }
                        }
                        format!(
                            "orchestrator session {}",
                            if show { "shown" } else { "hidden" }
                        )
                    }
                    None => "no CLI orchestrator session running".to_string(),
                };
            }
            ["orchestrator"] | ["orchestrator", "status"] => {
                self.command_result = if let Some(h) = &self.orchestrator {
                    format!(
                        "orchestrator @{} ({}) — {} in / {} out tokens",
                        h.name,
                        self.config.orchestrator.provider,
                        self.orchestrator_stats.input_tokens,
                        self.orchestrator_stats.output_tokens
                    )
                } else if let Some(orch_id) = self.orchestrator_session_id {
                    format!(
                        "orchestrator session {} ({})",
                        orch_id + 1,
                        self.config.orchestrator.provider
                    )
                } else {
                    "orchestrator is not running (enable [orchestrator] in linkshell.toml)"
                        .to_string()
                };
            }
            ["confirm-kill"] => self.resolve_pending_kill(true),
            ["deny-kill"] => self.resolve_pending_kill(false),
            ["interrupt"] | ["stop"] => self.interrupt_orchestrator(),
            ["approve"] => self.resolve_pending_proposal(true, String::new()),
            ["deny", rest @ ..] => self.resolve_pending_proposal(false, rest.join(" ")),
            ["yes"] => self.answer_permission(None, true),
            ["yes", t] => self.answer_permission(Some(t), true),
            ["no"] => self.answer_permission(None, false),
            ["no", t] => self.answer_permission(Some(t), false),
            _ => {}
        }
    }

    /// Break the orchestrator out of its current turn (/interrupt, /stop).
    /// The turn stops at the next safe point: immediately if it's blocked in
    /// a tool call, otherwise before the next tool iteration.
    fn interrupt_orchestrator(&mut self) {
        let Some(h) = &self.orchestrator else {
            self.command_result = "no API orchestrator running".to_string();
            return;
        };
        h.interrupt();
        // A pending proposal is a blocked tool call; clear it so the chat
        // pane doesn't keep asking for a verdict on a dead question.
        self.pending_proposal = None;
        let name = h.name.clone();
        self.chat_system(format!("interrupt sent to @{}", name));
        self.command_result = format!("interrupted @{}", name);
    }

    /// Approve or refuse the orchestrator's pending kill request.
    fn resolve_pending_kill(&mut self, approve: bool) {
        let Some(pk) = self.pending_kill.take() else {
            self.command_result = "no pending kill request".to_string();
            return;
        };
        if pk.requested_at.elapsed() > std::time::Duration::from_secs(600) {
            self.command_result = "kill request expired; ask the agent again".to_string();
            return;
        }
        if approve {
            match self.sessions.iter().position(|s| s.id == pk.session_id) {
                Some(idx) => {
                    self.remove_session(idx);
                    self.command_result = format!(
                        "killed session {} \"{}\"",
                        pk.session_id + 1,
                        pk.session_name
                    );
                }
                None => {
                    self.command_result = "session already gone".to_string();
                }
            }
        } else {
            self.command_result = format!(
                "refused kill of session {} \"{}\"",
                pk.session_id + 1,
                pk.session_name
            );
        }
        let why = if pk.reason.is_empty() {
            String::new()
        } else {
            format!(" (requested for: {})", pk.reason)
        };
        let note = format!(
            "user {} kill of session {} \"{}\"{}",
            if approve { "approved" } else { "refused" },
            pk.session_id + 1,
            pk.session_name,
            why
        );
        self.notify_orchestrator_note(note);
    }

    /// Deliver a system note to whichever orchestrator flavor is running.
    fn notify_orchestrator_note(&mut self, note: String) {
        if let Some(handle) = &self.orchestrator {
            let dead = matches!(
                handle
                    .tx
                    .try_send(crate::orchestrator::OrchestratorMsg::SystemNote(note)),
                Err(mpsc::error::TrySendError::Closed(_))
            );
            if dead {
                self.orchestrator_gone();
            }
        } else if let Some(orch_id) = self.orchestrator_session_id {
            self.handle_pipe_relay(orch_id, format!("[linkshell] {}\r", note));
        }
    }

    /// The API-class orchestrator task is gone (its channel closed): clear
    /// the stale handle and tell the user how to get it back.
    fn orchestrator_gone(&mut self) {
        if self.orchestrator.take().is_some() {
            self.chat_system(
                "orchestrator agent is no longer running — /orchestrator restart to reconnect",
            );
            self.needs_redraw = true;
        }
    }

    /// Detect a silently-died orchestrator task even when nobody is talking
    /// to it. Called every tick; cheap (one atomic load).
    fn check_orchestrator_alive(&mut self) {
        if self.orchestrator.as_ref().is_some_and(|h| h.tx.is_closed()) {
            self.orchestrator_gone();
        }
    }

    fn execute_pipe_command(&mut self, args: &[&str]) {
        if args.first() == Some(&"fire") {
            let src_id = args
                .get(1)
                .and_then(|reference| self.resolve_session_ref(reference));
            let dst_id = args
                .get(2)
                .and_then(|reference| self.resolve_session_ref(reference));
            if let Some(source) = src_id {
                self.fire_manual_pipes(source, dst_id);
            }
            return;
        }

        if args.len() < 2 {
            return;
        }
        let src_id = self.resolve_session_ref(args[0]);
        let dst_id = self.resolve_session_ref(args[1]);

        let (source, dest) = match (src_id, dst_id) {
            (Some(s), Some(d)) => (s, d),
            _ => return,
        };

        let mut extract = ExtractMode::LastBlock;
        let mut trigger = PipeTrigger::OnReady;
        let mut prefix: Option<String> = None;
        let mut condition: Option<String> = None;

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
            } else if let Some(val) = flag.strip_prefix("--if-matches=") {
                condition = Some(val.trim_matches('"').to_string());
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
            condition,
        });
    }

    fn resolve_session_ref(&self, reference: &str) -> Option<usize> {
        reference
            .parse::<usize>()
            .ok()
            .and_then(|number| self.sessions.get(number.wrapping_sub(1)))
            .or_else(|| {
                self.sessions
                    .iter()
                    .find(|session| session.name == reference)
            })
            .map(|session| session.id)
    }

    pub fn pipe_summary_for(&self, session_id: usize, now: std::time::Instant) -> Vec<PipeGlyph> {
        self.pipes
            .iter()
            .filter(|pipe| pipe.source == session_id || pipe.dest == session_id)
            .map(|pipe| {
                let outgoing = pipe.source == session_id;
                let peer_id = if outgoing { pipe.dest } else { pipe.source };
                PipeGlyph {
                    outgoing,
                    peer: self
                        .sessions
                        .iter()
                        .find(|session| session.id == peer_id)
                        .map(|session| session.name.clone())
                        .unwrap_or_else(|| format!("#{peer_id}")),
                    recent: pipe
                        .last_fired
                        .and_then(|fired| now.checked_duration_since(fired))
                        .is_some_and(|elapsed| elapsed < Duration::from_secs(5)),
                    active: pipe.active,
                }
            })
            .collect()
    }

    pub fn open_pipe_list(&mut self) {
        self.pipe_list_selected = self
            .pipe_list_selected
            .min(self.pipes.len().saturating_sub(1));
        self.mode = AppMode::PipeList;
    }

    pub fn pipe_list_move(&mut self, delta: isize) {
        if self.pipes.is_empty() {
            self.pipe_list_selected = 0;
        } else {
            self.pipe_list_selected = (self.pipe_list_selected as isize + delta)
                .clamp(0, self.pipes.len() as isize - 1)
                as usize;
        }
    }

    pub fn pipe_list_toggle(&mut self) {
        if let Some(pipe) = self.pipes.get_mut(self.pipe_list_selected) {
            pipe.active = !pipe.active;
        }
    }

    pub fn pipe_list_delete(&mut self) {
        if self.pipe_list_selected < self.pipes.len() {
            let removed = self.pipes.remove(self.pipe_list_selected);
            let key = PipeKey::from_pipe(&removed);
            self.abort_pipe_tasks(|candidate| candidate == key);
            self.pipe_list_selected = self
                .pipe_list_selected
                .min(self.pipes.len().saturating_sub(1));
        }
    }

    pub fn pipe_list_fire(&mut self) {
        if let Some(pipe) = self.pipes.get(self.pipe_list_selected) {
            self.fire_manual_pipes(pipe.source, Some(pipe.dest));
        }
    }

    fn execute_profile_command(&mut self, args: &[&str]) {
        let ["save", name] = args else {
            self.command_result = "usage: profile save <name>".into();
            self.mode = AppMode::CommandResult;
            return;
        };
        let names: HashMap<usize, String> = self
            .sessions
            .iter()
            .map(|session| (session.id, session.name.clone()))
            .collect();
        let profile = config::Profile {
            name: (*name).to_string(),
            sessions: self
                .sessions
                .iter()
                .map(|session| {
                    let command = match &session.kind {
                        SessionKind::Custom(command) => command.clone(),
                        _ => String::new(),
                    };
                    config::ProfileSession {
                        kind: session.kind.label().into(),
                        command,
                        name: session.name.clone(),
                        cwd: session.cwd.clone(),
                        group: session.group.clone(),
                    }
                })
                .collect(),
            pipes: self
                .pipes
                .iter()
                .filter_map(|pipe| {
                    Some(config::ProfilePipe {
                        source: names.get(&pipe.source)?.clone(),
                        dest: names.get(&pipe.dest)?.clone(),
                        trigger: match pipe.trigger {
                            PipeTrigger::OnReady => "on_ready",
                            PipeTrigger::OnWaiting => "on_waiting",
                            PipeTrigger::Manual => "manual",
                        }
                        .into(),
                        extract: match pipe.extract {
                            ExtractMode::LastBlock => "last_block".into(),
                            ExtractMode::LastN(n) => format!("last:{n}"),
                            ExtractMode::Diff => "diff".into(),
                            ExtractMode::Summarize(n) => format!("summarize:{n}"),
                        },
                        prefix: pipe.prefix.clone(),
                    })
                })
                .collect(),
        };
        self.command_result = match config::save_profile(&profile) {
            Ok(path) => format!("saved profile '{}' to {}", name, path.display()),
            Err(error) => format!("profile save: {error}"),
        };
        self.mode = AppMode::CommandResult;
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

    pub fn write_to_active(&mut self, data: &[u8]) {
        // Typing returns the view to the live tail.
        self.clear_scroll();
        if self.broadcast_mode {
            let ids: Vec<usize> = self
                .sessions
                .iter()
                .filter(|s| s.state != SessionState::Dead)
                .map(|s| s.id)
                .collect();
            for id in ids {
                if let Some(s) = self.sessions.iter().find(|s| s.id == id) {
                    s.write_bytes(data.to_vec());
                }
            }
        } else if let Some(session) = self.active_session() {
            session.write_bytes(data.to_vec());
        }
    }

    pub fn open_menu(&mut self) {
        self.mode = AppMode::Menu {
            selected_top: 0,
            selected_sub: None,
        };
    }

    pub fn menu_move_top(&mut self, delta: i32) {
        if let AppMode::Menu {
            selected_top,
            selected_sub,
        } = self.mode
        {
            let next = ((selected_top as i32 + delta).rem_euclid(MENU.len() as i32)) as usize;
            self.mode = AppMode::Menu {
                selected_top: next,
                selected_sub,
            };
        }
    }

    pub fn menu_move_sub(&mut self, delta: i32) {
        if let AppMode::Menu {
            selected_top,
            selected_sub,
        } = self.mode
        {
            let count = MENU[selected_top].1.len() as i32;
            let cur = selected_sub.unwrap_or(0) as i32;
            self.mode = AppMode::Menu {
                selected_top,
                selected_sub: Some(((cur + delta).rem_euclid(count)) as usize),
            };
        }
    }

    pub fn menu_open_submenu(&mut self) {
        if let AppMode::Menu { selected_top, .. } = self.mode {
            self.mode = AppMode::Menu {
                selected_top,
                selected_sub: Some(0),
            };
        }
    }

    pub fn menu_close_submenu(&mut self) {
        if let AppMode::Menu { selected_top, .. } = self.mode {
            self.mode = AppMode::Menu {
                selected_top,
                selected_sub: None,
            };
        }
    }

    pub fn execute_selected_menu_action(&mut self) {
        if let AppMode::Menu {
            selected_top,
            selected_sub,
        } = self.mode
        {
            self.execute_menu_action(selected_top, selected_sub.unwrap_or(0));
        }
    }

    fn execute_menu_action(&mut self, top: usize, sub: usize) {
        self.mode = AppMode::Normal;
        match (top, sub) {
            (0, 0) => self.open_new_session(),
            (0, 1) => self.kill_active_session(),
            (0, 2) => self.next_session(),
            (0, 3) => self.prev_session(),
            (1, 0) => self.scroll_up(20),
            (1, 1) => self.scroll_down(20),
            (1, 2) => self.clear_scroll(),
            (2, 0) => {
                self.command_result = if self.pipes.is_empty() {
                    "No active pipes".into()
                } else {
                    self.pipes
                        .iter()
                        .map(|p| {
                            format!(
                                "{} → {}  trigger={:?}  extract={:?}{}",
                                p.source + 1,
                                p.dest + 1,
                                p.trigger,
                                p.extract,
                                p.prefix
                                    .as_deref()
                                    .map(|s| format!("  prefix={:?}", s))
                                    .unwrap_or_default(),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" | ")
                };
                self.mode = AppMode::CommandResult;
            }
            (2, 1) => {
                self.command_input = "pipe ".into();
                self.command_cursor = self.command_input.len();
                self.mode = AppMode::CommandBar;
            }
            (2, 2) => {
                self.command_input = "unpipe ".into();
                self.command_cursor = self.command_input.len();
                self.mode = AppMode::CommandBar;
            }
            (3, 0) | (3, 1) => self.mode = AppMode::Help,
            _ => {}
        }
    }
}

pub const MENU: &[(&str, &[&str])] = &[
    ("Sessions", &["New Session", "Kill Session", "Next", "Prev"]),
    ("View", &["Scroll Up", "Scroll Down", "Clear Scroll"]),
    ("Pipes", &["List Pipes", "Add Pipe", "Remove Pipe"]),
    ("Help", &["Keybindings", "About"]),
];

// ── Chat ──────────────────────────────────────────────────────────────────────

impl App {
    pub fn toggle_chat(&mut self) {
        self.chat_selection = None;
        self.mode = if matches!(self.mode, AppMode::Chat) {
            AppMode::Normal
        } else {
            AppMode::Chat
        };
    }

    pub fn chat_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        match key.code {
            KeyCode::Esc => {
                self.chat_selection = None;
                self.mode = AppMode::Normal;
            }
            KeyCode::Enter => self.chat_send(),
            KeyCode::Backspace if self.chat.cursor > 0 => {
                let mut i = self.chat.cursor - 1;
                while i > 0 && !self.chat.input.is_char_boundary(i) {
                    i -= 1;
                }
                self.chat.input.replace_range(i..self.chat.cursor, "");
                self.chat.cursor = i;
            }
            KeyCode::Left if self.chat.cursor > 0 => {
                let mut i = self.chat.cursor - 1;
                while i > 0 && !self.chat.input.is_char_boundary(i) {
                    i -= 1;
                }
                self.chat.cursor = i;
            }
            KeyCode::Right if self.chat.cursor < self.chat.input.len() => {
                let mut i = self.chat.cursor + 1;
                while i < self.chat.input.len() && !self.chat.input.is_char_boundary(i) {
                    i += 1;
                }
                self.chat.cursor = i;
            }
            KeyCode::PageUp => self.chat_scroll_up(10),
            KeyCode::PageDown => self.chat_scroll_down(10),
            KeyCode::Char(c) => {
                self.chat.input.insert(self.chat.cursor, c);
                self.chat.cursor += c.len_utf8();
            }
            _ => {}
        }
    }

    pub fn chat_scroll_up(&mut self, lines: usize) {
        self.chat.scroll = (self.chat.scroll + lines).min(self.chat_scroll_max);
    }

    pub fn chat_scroll_down(&mut self, lines: usize) {
        self.chat.scroll = self.chat.scroll.saturating_sub(lines);
    }

    /// Insert pasted text into the chat input at the cursor. Newlines are
    /// kept (rendered as ⏎; sent via bracketed paste); other control
    /// characters are dropped.
    pub fn chat_paste(&mut self, text: &str) {
        let cleaned: String = text
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|c| !c.is_control() || *c == '\n')
            .collect();
        self.chat.input.insert_str(self.chat.cursor, &cleaned);
        self.chat.cursor += cleaned.len();
    }

    fn chat_system(&mut self, text: impl Into<String>) {
        self.chat.messages.push(ChatMsg {
            from: "linkshell".to_string(),
            text: text.into(),
        });
    }

    /// Parse and dispatch one chat input line:
    ///   /agents           list addressable targets
    ///   /approve, /deny [reason]   answer a pending orchestrator proposal
    ///   /<command>        run any command-bar command without leaving chat
    ///   @<target> <msg>   address a session (by name or number), a configured
    ///                     local LLM agent, or `all`
    ///   <msg>             goes to the last addressed target
    pub fn chat_send(&mut self) {
        let raw = std::mem::take(&mut self.chat.input);
        self.chat.cursor = 0;
        self.chat.scroll = 0;
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            return;
        }

        if raw == "/agents" {
            let mut targets: Vec<String> = Vec::new();
            if let Some(h) = &self.orchestrator {
                targets.push(format!("@{} (orchestrator)", h.name));
            }
            if let Some(s) = self
                .orchestrator_session_id
                .and_then(|id| self.sessions.iter().find(|s| s.id == id))
            {
                targets.push(format!("@{} (orchestrator)", s.name));
            }
            targets.extend(
                self.config
                    .agents
                    .keys()
                    .map(|k| format!("@{} (local llm)", k)),
            );
            targets.extend(
                self.sessions
                    .iter()
                    .filter(|s| !s.hidden)
                    .map(|s| format!("@{} / @{} ({})", s.name, s.id + 1, s.kind.label())),
            );
            targets.push("@all (every AI session)".to_string());
            let list = if targets.is_empty() {
                "no targets — spawn sessions or configure [agents.*]".to_string()
            } else {
                targets.join("   ")
            };
            self.chat_system(list);
            return;
        }

        if let Some(cmd) = raw.strip_prefix('/') {
            self.command_input = cmd.to_string();
            self.execute_command();
            if !self.command_result.is_empty() {
                let result = self.command_result.clone();
                self.chat_system(result);
                self.command_result.clear();
            }
            self.mode = AppMode::Chat; // stay in the pane
            return;
        }

        // Resolve the target
        let (target, msg) = if let Some(rest) = raw.strip_prefix('@') {
            match rest.split_once(char::is_whitespace) {
                Some((t, m)) => (t.to_string(), m.trim().to_string()),
                None => {
                    self.chat.target = Some(rest.to_string());
                    self.chat_system(format!("now talking to @{}", rest));
                    return;
                }
            }
        } else {
            match &self.chat.target {
                Some(t) => (t.clone(), raw),
                // Unaddressed messages default to the orchestrator when
                // present — the API-class agent or the CLI-class session.
                None => {
                    let orch_name =
                        self.orchestrator
                            .as_ref()
                            .map(|h| h.name.clone())
                            .or_else(|| {
                                self.orchestrator_session_id
                                    .and_then(|id| self.sessions.iter().find(|s| s.id == id))
                                    .map(|s| s.name.clone())
                            });
                    match orch_name {
                        Some(name) => (name, raw),
                        None => {
                            self.chat_system("address someone first: @name message (see /agents)");
                            return;
                        }
                    }
                }
            }
        };
        self.chat.target = Some(target.clone());

        if target == "all" {
            let ids: Vec<(usize, String)> = self
                .sessions
                .iter()
                .filter(|s| s.base != crate::session::BaseKind::Other && !s.is_orchestrator)
                .map(|s| (s.id, s.name.clone()))
                .collect();
            if ids.is_empty() {
                self.chat_system("no AI sessions to broadcast to");
                return;
            }
            for (id, name) in &ids {
                self.send_chat_to_session(*id, name.clone(), &msg);
            }
            self.chat.messages.push(ChatMsg {
                from: "you → all".to_string(),
                text: msg,
            });
            return;
        }

        // The resident orchestrator agent (API class; the CLI class is a
        // session and resolves through the session-name path below).
        if let Some(handle) = self.orchestrator.as_ref().filter(|h| h.name == target) {
            let send = handle
                .tx
                .try_send(crate::orchestrator::OrchestratorMsg::UserChat(msg.clone()));
            self.chat.messages.push(ChatMsg {
                from: format!("you → {}", target),
                text: msg,
            });
            match send {
                Err(mpsc::error::TrySendError::Closed(_)) => self.orchestrator_gone(),
                Err(mpsc::error::TrySendError::Full(_)) => {
                    self.chat_system(
                        "orchestrator is backed up and dropped that message — try again, or /orchestrator restart if it stays stuck",
                    );
                }
                Ok(()) => {}
            }
            return;
        }

        // Local LLM agent from [agents.*]
        if let Some(agent) = self.config.agents.get(&target).cloned() {
            let history = self.chat.histories.entry(target.clone()).or_default();
            history.push(("user".to_string(), msg.clone()));
            // Bound context growth
            let excess = history.len().saturating_sub(40);
            if excess > 0 {
                history.drain(..excess);
            }
            crate::agent_llm::spawn_chat_request(
                target.clone(),
                agent,
                history.clone(),
                self.event_tx.clone(),
            );
            self.chat.messages.push(ChatMsg {
                from: format!("you → {}", target),
                text: msg,
            });
            return;
        }

        // Session by name or by the number shown in the session bar
        let by_number = target
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|p| self.visible_to_idx(p));
        let found = self
            .sessions
            .iter()
            .enumerate()
            .find(|(idx, s)| s.name == target || by_number == Some(*idx))
            .map(|(_, s)| (s.id, s.name.clone()));
        match found {
            Some((id, name)) => {
                self.send_chat_to_session(id, name.clone(), &msg);
                self.chat.messages.push(ChatMsg {
                    from: format!("you → {}", name),
                    text: msg,
                });
            }
            None => {
                self.chat_system(format!(
                    "no target '{}' — /agents lists who you can talk to",
                    target
                ));
            }
        }
    }

    /// Answer a pending permission / y-n prompt in a session with the CLI's
    /// own keys (claude: `1` / Esc, codex: `y` / `n`, others: `y⏎` / `n⏎`).
    /// Without a target, answers the request most recently surfaced in chat.
    fn answer_permission(&mut self, target: Option<&str>, approve: bool) {
        let id = match target {
            Some(t) => {
                let by_number = t
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|p| self.visible_to_idx(p));
                let found = self
                    .sessions
                    .iter()
                    .enumerate()
                    .find(|(idx, s)| s.name == t || by_number == Some(*idx))
                    .map(|(_, s)| s.id);
                match found {
                    Some(id) => id,
                    None => {
                        self.command_result = format!("no session '{}'", t);
                        return;
                    }
                }
            }
            None => match self
                .last_permission_request
                .filter(|id| self.sessions.iter().any(|s| s.id == *id))
            {
                Some(id) => id,
                None => {
                    self.command_result =
                        "no pending permission request — use yes/no <session>".to_string();
                    return;
                }
            },
        };
        let Some(s) = self.sessions.iter().find(|s| s.id == id) else {
            return;
        };
        if s.state != SessionState::Waiting {
            self.command_result = format!(
                "@{} isn't waiting on a prompt (state: {})",
                s.name,
                s.state.label()
            );
            return;
        }
        let bytes: &[u8] = match (&s.base, approve) {
            (crate::session::BaseKind::Claude, true) => b"1",
            (crate::session::BaseKind::Claude, false) => b"\x1b",
            (crate::session::BaseKind::Codex, true) => b"y",
            (crate::session::BaseKind::Codex, false) => b"n",
            (_, true) => b"y\r",
            (_, false) => b"n\r",
        };
        s.write_bytes(bytes.to_vec());
        let name = s.name.clone();
        if self.last_permission_request == Some(id) {
            self.last_permission_request = None;
        }
        self.command_result = format!("sent {} to @{}", if approve { "yes" } else { "no" }, name);
    }

    /// Inject a chat message into a session's PTY and await its READY reply.
    fn send_chat_to_session(&mut self, id: usize, name: String, msg: &str) {
        if let Some(s) = self.sessions.iter().find(|s| s.id == id) {
            s.write_bytes(shape_injected_input(msg));
        }
        // One outstanding reply per session; a new message supersedes it.
        self.chat.pending.retain(|p| p.session_id != id);
        self.chat.pending.push(PendingChat {
            session_id: id,
            name,
        });
    }

    /// Called on session state transitions: when a session we're awaiting hits
    /// READY, pull its answer into the chat transcript.
    pub fn check_chat_pending(&mut self, session_id: usize, state: &SessionState) {
        if !matches!(state, SessionState::Ready) {
            return;
        }
        let Some(pos) = self
            .chat
            .pending
            .iter()
            .position(|p| p.session_id == session_id)
        else {
            return;
        };
        let pending = self.chat.pending.remove(pos);
        let reply = crate::pipe::extract_from_session(
            &self.sessions,
            session_id,
            &crate::pipe::ExtractMode::LastBlock,
        )
        .or_else(|| {
            // A raw screen tail shows everything still on screen (the echoed
            // question, earlier turns); cut it down to the latest reply.
            crate::pipe::extract_from_session(
                &self.sessions,
                session_id,
                &crate::pipe::ExtractMode::LastN(40),
            )
            .map(|t| last_reply_block(&t))
        });
        if let Some(text) = reply {
            self.chat.messages.push(ChatMsg {
                from: pending.name,
                text,
            });
        }
    }

    pub fn handle_chat_reply(&mut self, from: String, text: String) {
        self.chat
            .histories
            .entry(from.clone())
            .or_default()
            .push(("assistant".to_string(), text.clone()));
        self.chat.messages.push(ChatMsg { from, text });
        self.chat.scroll = 0;
    }
}

// ── Council ───────────────────────────────────────────────────────────────────

impl App {
    /// `council <file>` / `council load <file>` / `council status` / `council stop`
    fn execute_council_command(&mut self, args: &[&str]) {
        let result = match args {
            [] | ["status"] => {
                self.command_result = match &self.council {
                    Some(r) => format!(
                        "council '{}': round {}/{}{}",
                        r.group,
                        r.round,
                        r.max_rounds,
                        if r.complete { "  [complete]" } else { "" },
                    ),
                    None => "no council running — usage: council <file.toml>".to_string(),
                };
                self.mode = AppMode::CommandResult;
                return;
            }
            ["stop"] => {
                self.command_result = match self.council.take() {
                    Some(r) => format!("council '{}' stopped (sessions left running)", r.group),
                    None => "no council running".to_string(),
                };
                self.mode = AppMode::CommandResult;
                return;
            }
            ["load", path] | [path] => self.load_council_file(path),
            _ => Err(anyhow::anyhow!(
                "usage: council <file.toml> | council status | council stop"
            )),
        };
        if let Err(e) = result {
            self.command_result = format!("council: {}", e);
            self.mode = AppMode::CommandResult;
        }
    }

    /// `config path` / `config edit` / `config reload`
    fn execute_config_command(&mut self, args: &[&str]) {
        self.command_result = match args {
            ["path"] | [] => match crate::config::config_path() {
                Some(p) => p.to_string_lossy().to_string(),
                None => "cannot resolve $HOME".to_string(),
            },
            ["edit"] => match crate::config::config_path() {
                Some(p) => {
                    // Make sure the file and its directory exist so the editor
                    // has something to open on first run.
                    if let Some(dir) = p.parent() {
                        let _ = std::fs::create_dir_all(dir);
                    }
                    if !p.exists() {
                        let _ = std::fs::write(&p, "# linkshell configuration\n");
                    }
                    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
                    let cmd = format!("{} {}", editor, p.to_string_lossy());
                    let cwd = p
                        .parent()
                        .map(|d| d.to_string_lossy().to_string())
                        .unwrap_or_else(|| "~".to_string());
                    match self.spawn_session(SessionKind::Custom(cmd), "config".to_string(), cwd) {
                        Ok(_) => "editing config — run `config reload` when done".to_string(),
                        Err(e) => format!("config edit: {}", e),
                    }
                }
                None => "cannot resolve $HOME".to_string(),
            },
            ["reload"] => {
                self.config = std::sync::Arc::new(crate::config::load());
                self.keymap = keybindings::build_keymap(&self.config.keybindings);
                "config reloaded — commands, aliases, agents, pricing, and keybindings apply now; \
                 socket settings and running watchers keep their old values"
                    .to_string()
            }
            _ => "usage: config path | config edit | config reload".to_string(),
        };
        self.mode = AppMode::CommandResult;
    }

    /// `grant <n> <operator|worker|council>` — change a session's IPC capabilities.
    fn execute_grant_command(&mut self, args: &[&str]) {
        self.command_result = match args {
            [n, tier] => match n.parse::<usize>().ok().and_then(|n| {
                self.sessions
                    .iter()
                    .find(|s| s.id + 1 == n || s.id == n)
                    .map(|s| s.id)
            }) {
                Some(id) => {
                    let caps = match *tier {
                        "operator" => Some(crate::auth::operator_caps()),
                        "worker" => Some(crate::auth::worker_caps()),
                        "council" => Some(crate::auth::council_caps()),
                        _ => None,
                    };
                    match caps {
                        Some(caps) => {
                            self.caps.insert(id, caps);
                            format!("session {} granted {} capabilities", n, tier)
                        }
                        None => "usage: grant <n> <operator|worker|council>".to_string(),
                    }
                }
                None => format!("no session {}", n),
            },
            _ => "usage: grant <n> <operator|worker|council>".to_string(),
        };
        self.mode = AppMode::CommandResult;
    }

    /// `restart [n]` — respawn a session with the same command, name, and cwd.
    fn execute_restart_command(&mut self, args: &[&str]) {
        let idx = match args {
            [] => self.active_idx(),
            [n] => n.parse::<usize>().ok().and_then(|n| {
                self.sessions
                    .iter()
                    .position(|s| s.id + 1 == n || s.id == n)
            }),
            _ => None,
        };
        let Some(idx) = idx else {
            self.command_result = "usage: restart [n]".to_string();
            self.mode = AppMode::CommandResult;
            return;
        };
        let (kind, name, cwd, id) = {
            let s = &self.sessions[idx];
            (s.kind.clone(), s.name.clone(), s.cwd.clone(), s.id)
        };
        let _ = id;
        self.remove_session(idx);
        self.command_result = match self.spawn_session(kind, name.clone(), cwd) {
            Ok(_) => format!("restarted '{}'", name),
            Err(e) => format!("restart: {}", e),
        };
        self.mode = AppMode::CommandResult;
    }

    fn execute_move_command(&mut self, args: &[&str]) {
        if args.len() < 2 {
            self.command_result = "usage: move <from> <to>".to_string();
            self.mode = AppMode::CommandResult;
            return;
        }
        let from_1: usize = match args[0].parse() {
            Ok(n) => n,
            Err(_) => {
                self.command_result = "move: invalid session number".to_string();
                self.mode = AppMode::CommandResult;
                return;
            }
        };
        let to_1: usize = match args[1].parse() {
            Ok(n) => n,
            Err(_) => {
                self.command_result = "move: invalid session number".to_string();
                self.mode = AppMode::CommandResult;
                return;
            }
        };
        let n = self.sessions.len();
        if from_1 < 1 || from_1 > n || to_1 < 1 || to_1 > n {
            self.command_result = format!("move: session index out of range (1–{})", n);
            self.mode = AppMode::CommandResult;
            return;
        }
        let from_idx = from_1 - 1;
        let to_idx = to_1 - 1;
        self.sessions.swap(from_idx, to_idx);
        // Update pane indices to follow the swapped sessions
        for idx in self.panes.iter_mut().flatten() {
            if *idx == from_idx {
                *idx = to_idx;
            } else if *idx == to_idx {
                *idx = from_idx;
            }
        }
        self.command_result = format!("moved session {} ↔ {}", from_1, to_1);
        self.mode = AppMode::CommandResult;
    }

    fn execute_rename_command(&mut self, args: &[&str]) {
        if args.len() < 2 {
            self.command_result = "usage: rename <session> <new_name>".to_string();
            self.mode = AppMode::CommandResult;
            return;
        }
        let id = self.resolve_session_ref(args[0]);
        let new_name = args[1..].join(" ");
        match id {
            None => {
                self.command_result = format!("rename: session '{}' not found", args[0]);
                self.mode = AppMode::CommandResult;
            }
            Some(session_id) => {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                    s.name = new_name.clone();
                    self.command_result = format!("renamed to '{}'", new_name);
                    self.mode = AppMode::CommandResult;
                }
            }
        }
    }

    fn execute_log_command(&mut self, args: &[&str]) {
        if args.first() == Some(&"stop") {
            let id = args
                .get(1)
                .and_then(|r| self.resolve_session_ref(r))
                .or_else(|| self.active_session().map(|s| s.id));
            match id {
                None => {
                    self.command_result = "log stop: no session found".to_string();
                    self.mode = AppMode::CommandResult;
                }
                Some(session_id) => {
                    if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                        s.log_path = None;
                        self.command_result = "logging stopped".to_string();
                        self.mode = AppMode::CommandResult;
                    }
                }
            }
            return;
        }
        if args.len() < 2 {
            self.command_result = "usage: log <session> <path>  or  log stop <session>".to_string();
            self.mode = AppMode::CommandResult;
            return;
        }
        let id = self.resolve_session_ref(args[0]);
        let path = std::path::PathBuf::from(args[1]);
        match id {
            None => {
                self.command_result = format!("log: session '{}' not found", args[0]);
                self.mode = AppMode::CommandResult;
            }
            Some(session_id) => {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
                    let display = path.display().to_string();
                    s.log_path = Some(path);
                    self.command_result = format!("logging to {}", display);
                    self.mode = AppMode::CommandResult;
                }
            }
        }
    }

    pub fn search_compute_matches(&self, query: &str) -> Vec<usize> {
        if query.is_empty() {
            return vec![];
        }
        let query_lower = query.to_lowercase();
        if let Some(session) = self.active_session() {
            session
                .output_lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.to_lowercase().contains(&query_lower))
                .map(|(i, _)| i)
                .collect()
        } else {
            vec![]
        }
    }

    pub fn search_update_matches(&mut self) {
        let query = match &self.mode {
            AppMode::Search { query, .. } => query.clone(),
            _ => return,
        };
        let matches = self.search_compute_matches(&query);
        if let AppMode::Search {
            query: q,
            cursor,
            selected,
            ..
        } = &self.mode
        {
            let q = q.clone();
            let c = *cursor;
            let s = (*selected).min(matches.len().saturating_sub(1));
            self.mode = AppMode::Search {
                query: q,
                cursor: c,
                matches,
                selected: s,
            };
        }
    }

    pub fn open_settings(&mut self) {
        let fields = vec![
            SettingsField {
                label: "Tick interval (ms)",
                value: self.config.general.tick_interval_ms.to_string(),
                description: "How often the event loop ticks",
            },
            SettingsField {
                label: "Scroll buffer (lines)",
                value: self.config.general.scroll_buffer_lines.to_string(),
                description: "Number of output lines retained per session",
            },
            SettingsField {
                label: "Notifications enabled",
                value: self.config.notifications.enabled.to_string(),
                description: "Enable desktop/terminal notifications",
            },
            SettingsField {
                label: "Notification method",
                value: format!("{:?}", self.config.notifications.method),
                description: "auto, osc9, notify-send, bell, none",
            },
            SettingsField {
                label: "Notification debounce (s)",
                value: self.config.notifications.debounce_secs.to_string(),
                description: "Minimum seconds between notifications per session",
            },
            SettingsField {
                label: "Default CWD",
                value: self.config.sessions.default_cwd.clone(),
                description: "Default working directory for new sessions",
            },
            SettingsField {
                label: "Claude command",
                value: self.config.sessions.commands.claude.clone(),
                description: "Command used to start Claude sessions",
            },
            SettingsField {
                label: "Codex command",
                value: self.config.sessions.commands.codex.clone(),
                description: "Command used to start Codex sessions",
            },
            SettingsField {
                label: "Shell command",
                value: self.config.sessions.commands.shell.clone(),
                description: "Shell command (empty = $SHELL)",
            },
        ];
        self.settings_state = SettingsState {
            fields,
            selected: 0,
            editing: false,
            edit_buf: String::new(),
            edit_cursor: 0,
        };
        self.mode = AppMode::Settings;
    }

    pub fn apply_settings(&mut self) -> anyhow::Result<()> {
        let mut cfg = (*self.config).clone();
        for field in &self.settings_state.fields {
            match field.label {
                "Tick interval (ms)" => {
                    cfg.general.tick_interval_ms =
                        field.value.parse().unwrap_or(cfg.general.tick_interval_ms);
                }
                "Scroll buffer (lines)" => {
                    cfg.general.scroll_buffer_lines = field
                        .value
                        .parse()
                        .unwrap_or(cfg.general.scroll_buffer_lines);
                }
                "Notifications enabled" => {
                    cfg.notifications.enabled =
                        field.value.parse().unwrap_or(cfg.notifications.enabled);
                }
                "Notification debounce (s)" => {
                    cfg.notifications.debounce_secs = field
                        .value
                        .parse()
                        .unwrap_or(cfg.notifications.debounce_secs);
                }
                "Default CWD" => {
                    cfg.sessions.default_cwd = field.value.clone();
                }
                "Claude command" => {
                    cfg.sessions.commands.claude = field.value.clone();
                }
                "Codex command" => {
                    cfg.sessions.commands.codex = field.value.clone();
                }
                "Shell command" => {
                    cfg.sessions.commands.shell = field.value.clone();
                }
                _ => {}
            }
        }
        config::save(&cfg)?;
        Ok(())
    }

    /// Read and parse a council.toml, then launch it.
    pub fn load_council_file(&mut self, path: &str) -> anyhow::Result<()> {
        let cfg = crate::council::load_config_file(path)?;
        self.launch_council(cfg)
    }

    pub fn launch_council(&mut self, cfg: crate::council::CouncilConfig) -> anyhow::Result<()> {
        if self.council.is_some() {
            return Err(anyhow::anyhow!(
                "a council is already running — `council stop` first"
            ));
        }
        if self.visible_count() + cfg.agent.len() > MAX_SESSIONS {
            return Err(anyhow::anyhow!(
                "council needs {} sessions but only {} slots are free",
                cfg.agent.len(),
                MAX_SESSIONS - self.visible_count()
            ));
        }
        // Validate route names before spawning anything.
        for r in &cfg.route {
            for n in r.from.iter().chain(r.to.iter()) {
                if !cfg.agent.iter().any(|a| &a.name == n) {
                    return Err(anyhow::anyhow!("route references unknown agent '{}'", n));
                }
            }
        }

        let group = cfg.council.name.clone();
        let mut name_to_id: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();

        for a in &cfg.agent {
            let kind = parse_kind(&a.kind);
            let id_before = self.next_id;
            self.spawn_session(kind, a.name.clone(), a.cwd.clone().unwrap_or_default())?;
            name_to_id.insert(a.name.clone(), id_before);
            // Tighten capabilities: council members only report state.
            self.caps.insert(id_before, crate::auth::council_caps());
            if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id_before) {
                s.group = Some(group.clone());
            }
            // Seed system prompt as the first relay once the agent reaches READY.
            if let Some(sys) = &a.system {
                self.pending_relays
                    .entry(id_before)
                    .or_default()
                    .push(sys.clone());
            }
        }

        let mut router = crate::council::CouncilRouter::new(&cfg.council, &group);
        for r in &cfg.route {
            let from: Vec<usize> = r
                .from
                .iter()
                .filter_map(|n| name_to_id.get(n).copied())
                .collect();
            let to: Vec<usize> =
                r.to.iter()
                    .filter_map(|n| name_to_id.get(n).copied())
                    .collect();
            router.add_route(from, to, r);
        }

        // Seed the task into the entry agent (first 'from' of the first route).
        if let Some(&entry) = cfg
            .route
            .first()
            .and_then(|r| r.from.first())
            .and_then(|n| name_to_id.get(n))
        {
            self.pending_relays
                .entry(entry)
                .or_default()
                .push(cfg.council.task.clone());
        }

        self.council = Some(router);
        Ok(())
    }
}

/// Expand a leading `~` or `~/` to $HOME. Anything else passes through.
fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}{}", home, &path[1..]);
        }
    }
    path.to_string()
}

fn parse_kind(s: &str) -> crate::session::SessionKind {
    match s {
        "claude" => crate::session::SessionKind::Claude,
        "codex" => crate::session::SessionKind::Codex,
        "shell" => crate::session::SessionKind::Shell,
        other => crate::session::SessionKind::Custom(other.to_string()),
    }
}

// ── PTY runner task ────────────────────────────────────────────────────────

// All 9 parameters are required session context (id, cmd, cwd, dims, channels, env vars);
// collapsing them into a struct would add boilerplate for a single private async function.
#[allow(clippy::too_many_arguments)]
async fn run_pty(
    session_id: usize,
    cmd: String,
    cwd: String,
    pty_rows: u16,
    pty_cols: u16,
    tx: mpsc::Sender<AppEvent>,
    linkshell_sock: String,
    linkshell_token: String,
    wrap_in_shell: bool,
    extra_env: Vec<(String, String)>,
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
        let mut command = if wrap_in_shell {
            // Run through the user's interactive shell so aliases in .bashrc/.zshrc
            // are honoured. We avoid `exec` because exec bypasses alias expansion —
            // only the first word of a plain simple command gets alias-expanded.
            // We also re-cd to the configured cwd so that any `cd` in .bashrc does
            // not shift claude to a different directory and break the JSONL watcher.
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
            let escaped_cwd = cwd.replace('\'', "'\\''");
            let shell_cmd = format!("cd '{}' && {}", escaped_cwd, cmd);
            let mut c = pty_process::Command::new(&shell);
            c.args(["-i", "-c", &shell_cmd]);
            c
        } else {
            let args: Vec<&str> = cmd.split_whitespace().collect();
            let (bin, cmd_args) = match args.as_slice() {
                [first, rest @ ..] => (*first, rest),
                [] => return Err(anyhow::anyhow!("empty command")),
            };
            let mut c = pty_process::Command::new(bin);
            c.args(cmd_args);
            c
        };
        command.current_dir(&cwd);
        command.env("LINKSHELL_SESSION_ID", session_id.to_string());
        command.env("LINKSHELL_SOCK", &linkshell_sock);
        command.env("LINKSHELL_TOKEN", &linkshell_token);
        // Identity env for aliased claude/codex sessions (CLAUDE_CONFIG_DIR /
        // CODEX_HOME from [sessions.aliases]); set on the shell so the CLI and
        // any children inherit it.
        for (k, v) in &extra_env {
            command.env(k, v);
        }
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
                    // Injected input (pipes, chat, orchestrator send_input)
                    // arrives as one chunk ending in '\r'/'\n'. Full-screen
                    // TUIs (claude, codex) treat such a chunk as a paste and
                    // turn the trailing newline into a line break in their
                    // input box instead of submitting. Write the text, give
                    // the TUI a beat to process it, then press Enter as its
                    // own keystroke. A lone '\r' (real Enter keypress) and
                    // user pastes (bracketed, ending in '~') pass through.
                    if chunk_carries_enter(&bytes) {
                        if write_half.write_all(&bytes[..bytes.len() - 1]).await.is_err() { break; }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        if write_half.write_all(b"\r").await.is_err() { break; }
                    } else if write_half.write_all(&bytes).await.is_err() { break; }
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

fn inset_rect(r: Rect, amount: u16) -> Rect {
    Rect {
        x: r.x.saturating_add(amount),
        y: r.y.saturating_add(amount),
        width: r.width.saturating_sub(amount.saturating_mul(2)),
        height: r.height.saturating_sub(amount.saturating_mul(2)),
    }
}

fn input_col(area: Rect, col: u16) -> usize {
    col.saturating_sub(area.x + 1) as usize
}

fn byte_index_for_col(text: &str, col: usize) -> usize {
    text.char_indices()
        .nth(col)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len())
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

/// True when a PTY write chunk is injected input carrying its own Enter:
/// longer than one byte and newline-terminated. A lone '\r' is a real Enter
/// keypress; bracketed user pastes end in '~'; escape responses end in a
/// letter — none of those match. The writer task sends the trailing Enter
/// separately after a pause so TUIs register a submit, not a pasted newline.
fn chunk_carries_enter(bytes: &[u8]) -> bool {
    bytes.len() > 1 && matches!(bytes.last(), Some(b'\r') | Some(b'\n'))
}

/// Shape injected input (chat, orchestrator send_input, initial prompts) for
/// a session PTY: multi-line messages go through bracketed paste so TUIs
/// (claude, codex, opencode) don't submit at each embedded newline, and the
/// trailing '\r' lets the writer task press Enter as its own keystroke.
fn shape_injected_input(msg: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(msg.len() + 13);
    if msg.contains('\n') {
        bytes.extend_from_slice(b"\x1b[200~");
        bytes.extend_from_slice(msg.as_bytes());
        bytes.extend_from_slice(b"\x1b[201~");
    } else {
        bytes.extend_from_slice(msg.as_bytes());
    }
    bytes.push(b'\r');
    bytes
}

/// Best-effort cut of a screen scrape down to the agent's latest reply:
/// content from the last "⏺" response marker (used by claude code and codex
/// for assistant turns) onward. Falls back to the full text when no marker
/// is present (shells, other TUIs).
fn last_reply_block(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    match lines.iter().rposition(|l| l.trim_start().starts_with('⏺')) {
        Some(i) => lines[i..].join("\n"),
        None => text.to_string(),
    }
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
    fn chat_paste_inserts_at_cursor_and_normalizes_newlines() {
        let mut app = make_app();
        app.chat.input = "ab".into();
        app.chat.cursor = 1;
        app.chat_paste("x\r\ny\tz");
        assert_eq!(app.chat.input, "ax\nyzb");
        assert_eq!(app.chat.cursor, 5);
    }

    #[test]
    fn chat_scroll_clamps_to_last_drawn_max() {
        let mut app = make_app();
        app.chat_scroll_max = 7;
        app.chat_scroll_up(10);
        assert_eq!(app.chat.scroll, 7);
        app.chat_scroll_down(3);
        assert_eq!(app.chat.scroll, 4);
    }

    #[test]
    fn chat_selection_extracts_visible_text_by_char_columns() {
        let mut app = make_app();
        app.chat_visible_lines = vec![
            "you: hello there".into(),
            "agent: hi".into(),
            "linkshell: note".into(),
        ];
        app.chat_selection = Some(Selection {
            start_col: 5,
            start_row: 0,
            end_col: 7,
            end_row: 1,
        });
        assert_eq!(app.chat_selected_text().unwrap(), "hello there\nagent: h");
    }

    #[test]
    fn chat_multiline_message_is_sent_via_bracketed_paste() {
        let mut app = make_app();
        let id = app.spawn_headless_session("worker".into(), None).unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        app.sessions[0].pty_writer = Some(tx);
        assert_eq!(app.sessions[0].id, id);

        app.chat.input = "@worker line1\nline2".into();
        app.chat_send();

        let sent = rx.try_recv().unwrap();
        assert_eq!(sent, b"\x1b[200~line1\nline2\x1b[201~\r".to_vec());
    }

    #[test]
    fn yes_no_commands_answer_with_cli_specific_keys() {
        let mut app = make_app();
        app.spawn_headless_session("coder".into(), None).unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        app.sessions[0].pty_writer = Some(tx);
        app.sessions[0].base = crate::session::BaseKind::Claude;
        app.sessions[0].state = SessionState::Waiting;
        let id = app.sessions[0].id;

        // Bare /yes answers the most recently surfaced request
        app.last_permission_request = Some(id);
        app.command_input = "yes".into();
        app.execute_command();
        assert_eq!(rx.try_recv().unwrap(), b"1".to_vec());
        assert_eq!(app.last_permission_request, None);

        // Targeted /no on a codex-based session sends 'n'
        app.sessions[0].base = crate::session::BaseKind::Codex;
        app.command_input = "no coder".into();
        app.execute_command();
        assert_eq!(rx.try_recv().unwrap(), b"n".to_vec());
    }

    #[test]
    fn yes_command_refuses_sessions_that_are_not_waiting() {
        let mut app = make_app();
        app.spawn_headless_session("coder".into(), None).unwrap();
        let (tx, mut rx) = mpsc::channel(4);
        app.sessions[0].pty_writer = Some(tx);
        app.sessions[0].state = SessionState::Ready;

        app.command_input = "yes coder".into();
        app.execute_command();
        assert!(rx.try_recv().is_err());
        assert!(app.command_result.contains("isn't waiting"));
    }

    #[test]
    fn waiting_ai_session_surfaces_permission_request_in_chat() {
        let mut app = make_app();
        app.spawn_headless_session("coder".into(), None).unwrap();
        app.sessions[0].base = crate::session::BaseKind::Claude;
        app.sessions[0].waiting_prompt = Some("Allow Bash(cargo test)?".into());
        let id = app.sessions[0].id;

        app.surface_permission_request(id, &SessionState::Waiting);

        assert_eq!(app.last_permission_request, Some(id));
        let last = app.chat.messages.last().unwrap();
        assert!(last.text.contains("Allow Bash(cargo test)?"));
        assert!(last.text.contains("/yes coder"));
    }

    #[test]
    fn command_palette_lists_and_fuzzy_filters_registered_commands() {
        let mut app = make_app();
        app.open_command_bar();
        assert!(app
            .palette
            .matches
            .iter()
            .any(|entry| entry.template.starts_with("pipe ")));

        for ch in "pfsv".chars() {
            app.command_input_char(ch);
        }

        assert_eq!(
            app.palette
                .matches
                .first()
                .map(|entry| entry.template.as_str()),
            Some("profile save <name>")
        );
    }

    #[test]
    fn command_palette_completes_session_arguments_and_preserves_exact_commands() {
        let mut app = make_app();
        app.spawn_headless_session("reviewer".into(), None).unwrap();
        app.spawn_headless_session("local-llm".into(), None)
            .unwrap();
        app.open_command_bar();
        for ch in "pipe ".chars() {
            app.command_input_char(ch);
        }

        assert!(app
            .palette
            .matches
            .iter()
            .any(|entry| entry.insert == "pipe reviewer local-llm "));
        app.palette.selected = app
            .palette
            .matches
            .iter()
            .position(|entry| entry.insert == "pipe reviewer local-llm ")
            .unwrap();
        app.command_palette_insert_selected();
        assert_eq!(app.command_input, "pipe reviewer local-llm ");

        app.command_input = "q".into();
        app.command_cursor = 1;
        app.execute_command();
        assert!(app.should_quit);
    }

    #[test]
    fn pipe_topology_summaries_use_peer_names_direction_and_activity() {
        let mut app = make_app();
        let source = app.spawn_headless_session("reviewer".into(), None).unwrap();
        let dest = app
            .spawn_headless_session("local-llm".into(), None)
            .unwrap();
        app.pipes.push(Pipe {
            source,
            dest,
            trigger: PipeTrigger::Manual,
            extract: ExtractMode::LastBlock,
            prefix: None,
            active: false,
            last_fired: Some(std::time::Instant::now()),
            condition: None,
        });

        let outgoing = app.pipe_summary_for(source, std::time::Instant::now());
        assert_eq!(outgoing[0].peer, "local-llm");
        assert!(outgoing[0].outgoing);
        assert!(outgoing[0].recent);
        assert!(!outgoing[0].active);
        assert!(!app.pipe_summary_for(dest, std::time::Instant::now())[0].outgoing);
    }

    #[test]
    fn pipe_list_can_select_toggle_and_delete_pipes() {
        let mut app = make_app();
        let source = app.spawn_headless_session("a".into(), None).unwrap();
        let dest = app.spawn_headless_session("b".into(), None).unwrap();
        for _ in 0..2 {
            app.pipes.push(Pipe {
                source,
                dest,
                trigger: PipeTrigger::Manual,
                extract: ExtractMode::LastBlock,
                prefix: None,
                active: true,
                last_fired: None,
                condition: None,
            });
        }
        app.open_pipe_list();
        app.pipe_list_move(1);
        app.pipe_list_toggle();
        assert!(!app.pipes[1].active);
        app.pipe_list_delete();
        assert_eq!(app.pipes.len(), 1);
        assert_eq!(app.pipe_list_selected, 0);
    }

    #[test]
    fn waiting_transition_captures_prompt_and_later_state_clears_it() {
        let mut app = make_app();
        let id = app.spawn_headless_session("agent".into(), None).unwrap();
        app.sessions[0].state = SessionState::Running;
        app.handle_session_output(id, "Should I apply this change? [y/n]".into());
        assert_eq!(app.sessions[0].state, SessionState::Waiting);
        assert_eq!(
            app.sessions[0].waiting_prompt.as_deref(),
            Some("Should I apply this change? [y/n]")
        );

        app.handle_session_output(id, "$ ".into());
        assert_eq!(app.sessions[0].state, SessionState::Ready);
        assert_eq!(app.sessions[0].waiting_prompt, None);
    }

    #[test]
    fn notifications_respect_state_age_and_per_session_debounce() {
        let mut config = Config::default();
        config.notifications.method = crate::notify::Method::None;
        config.notifications.min_session_age_secs = 0;
        config.notifications.debounce_secs = 30;
        let mut app = make_app_with_config(config);
        let id = app.spawn_headless_session("agent".into(), None).unwrap();

        app.handle_ipc_state(id, SessionState::Waiting);
        let first = app.sessions[0].last_notified;
        assert!(first.is_some());
        app.handle_ipc_state(id, SessionState::Running);
        app.handle_ipc_state(id, SessionState::Error);
        assert_eq!(app.sessions[0].last_notified, first);
    }

    #[tokio::test]
    async fn apply_profile_spawns_named_sessions_and_resolves_pipe_ids() {
        let mut app = make_app();
        let profile = crate::config::Profile {
            name: "dev".into(),
            sessions: vec![
                crate::config::ProfileSession {
                    kind: "shell".into(),
                    command: String::new(),
                    name: "source".into(),
                    cwd: "/tmp".into(),
                    group: Some("team".into()),
                },
                crate::config::ProfileSession {
                    kind: "custom".into(),
                    command: "true".into(),
                    name: "dest".into(),
                    cwd: "/tmp".into(),
                    group: None,
                },
            ],
            pipes: vec![crate::config::ProfilePipe {
                source: "source".into(),
                dest: "dest".into(),
                trigger: "manual".into(),
                extract: "last:7".into(),
                prefix: Some("Review:".into()),
            }],
        };

        app.apply_profile(&profile).unwrap();

        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.sessions[0].name, "source");
        assert_eq!(app.sessions[0].group.as_deref(), Some("team"));
        assert_eq!(app.sessions[1].name, "dest");
        assert_eq!(app.pipes.len(), 1);
        assert_eq!(app.pipes[0].source, app.sessions[0].id);
        assert_eq!(app.pipes[0].dest, app.sessions[1].id);
        assert_eq!(app.pipes[0].trigger, PipeTrigger::Manual);
        assert!(matches!(app.pipes[0].extract, ExtractMode::LastN(7)));
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
    fn new_session_kind_selection_wraps_and_flags_custom_only_at_last_index() {
        let mut app = make_app();
        app.open_new_session();

        assert_eq!(app.new_session_state.selected_kind, 0);
        assert!(!app.new_session_state.is_custom());

        // Wrap backwards to the last entry (Custom).
        app.new_session_select_kind(-1);
        assert_eq!(
            app.new_session_state.selected_kind,
            crate::session::SessionKind::COUNT - 1
        );
        assert!(app.new_session_state.is_custom());

        // And forwards back to the first.
        app.new_session_select_kind(1);
        assert_eq!(app.new_session_state.selected_kind, 0);
    }

    #[test]
    fn new_session_dialog_resets_dropdown_state_on_open() {
        let mut app = make_app();
        app.new_session_state.kind_dropdown_open = true;
        app.new_session_state.selected_kind = 3;

        app.open_new_session();

        assert!(!app.new_session_state.kind_dropdown_open);
        assert_eq!(app.new_session_state.selected_kind, 0);
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
        assert_eq!(app.active_idx(), Some(2));
        app.kill_active_session();
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.active_idx(), Some(1));

        app.prev_session();
        assert_eq!(app.active_idx(), Some(0));
        app.next_session();
        assert_eq!(app.active_idx(), Some(1));
    }

    #[test]
    fn chunk_carries_enter_matches_injected_input_only() {
        assert!(chunk_carries_enter(b"ls -la /tmp\r"));
        assert!(chunk_carries_enter(b"fix the bug\n"));
        assert!(chunk_carries_enter(b"\x1b[200~multi\nline~\x1b[201~\r"));
        // Real Enter keypress, user paste, escape response: untouched
        assert!(!chunk_carries_enter(b"\r"));
        assert!(!chunk_carries_enter(b"\x1b[200~pasted text\n\x1b[201~"));
        assert!(!chunk_carries_enter(b"\x1b[?1u"));
        assert!(!chunk_carries_enter(b"a"));
    }

    #[test]
    fn last_reply_block_cuts_at_the_last_response_marker() {
        let screen = "you: fix the bug\n⏺ Reading files\n⏺ Done — the bug was a typo.\n  Fixed in src/foo.rs";
        assert_eq!(
            last_reply_block(screen),
            "⏺ Done — the bug was a typo.\n  Fixed in src/foo.rs"
        );
        // No marker: unchanged
        assert_eq!(last_reply_block("plain output"), "plain output");
    }

    #[test]
    fn hidden_orchestrator_waiting_prompt_is_surfaced_in_chat() {
        let mut app = make_app();
        let id = app.spawn_headless_session("agent".into(), None).unwrap();
        app.orchestrator_session_id = Some(id);
        {
            let s = app.sessions.iter_mut().find(|s| s.id == id).unwrap();
            s.is_orchestrator = true;
            s.hidden = true;
            s.waiting_prompt = Some("Allow Bash(cargo test)?".into());
        }

        app.notify_orchestrator(id, &SessionState::Waiting);

        let last = app.chat.messages.last().expect("chat notice");
        assert!(last.text.contains("needs input"));
        assert!(last.text.contains("Allow Bash(cargo test)?"));

        // Cooldown: an immediate repeat stays quiet
        let count = app.chat.messages.len();
        app.notify_orchestrator(id, &SessionState::Waiting);
        assert_eq!(app.chat.messages.len(), count);

        // A visible orchestrator session flags WAITING in the bar instead
        app.orch_event_cooldowns.clear();
        app.sessions[0].hidden = false;
        app.notify_orchestrator(id, &SessionState::Waiting);
        assert_eq!(app.chat.messages.len(), count);
    }

    #[test]
    fn hidden_sessions_are_skipped_by_switching_and_visible_numbering() {
        let mut app = make_app();
        let _ = app.spawn_headless_session("orch".into(), None).unwrap();
        let _ = app.spawn_headless_session("one".into(), None).unwrap();
        let _ = app.spawn_headless_session("two".into(), None).unwrap();
        app.sessions[0].hidden = true;
        app.panes[0] = Some(1);

        // Visible positions map past the hidden session
        assert_eq!(app.visible_indices(), vec![1, 2]);
        assert_eq!(app.visible_to_idx(0), Some(1));
        assert_eq!(app.visible_to_idx(2), None);

        // Direct switching to the hidden session is refused
        app.switch_to(0);
        assert_eq!(app.active_idx(), Some(1));

        // Cycling wraps across it in both directions
        app.next_session();
        assert_eq!(app.active_idx(), Some(2));
        app.next_session();
        assert_eq!(app.active_idx(), Some(1));
        app.prev_session();
        assert_eq!(app.active_idx(), Some(2));

        // Killing the last visible session must not focus the hidden one
        app.kill_active_session();
        app.kill_active_session();
        assert_eq!(app.active_idx(), None);
        assert_eq!(app.sessions.len(), 1);

        // display in sessions_json is the bar number; hidden gets null
        let _ = app.spawn_headless_session("three".into(), None).unwrap();
        let json = app.sessions_json();
        assert!(json[0]["display"].is_null());
        assert_eq!(json[1]["display"], 1);
    }

    #[test]
    fn session_output_strips_ansi_and_updates_state_without_scraping_shell_stats() {
        let mut app = make_app();
        let id = app.spawn_headless_session("shell".into(), None).unwrap();
        app.sessions[0].kind = SessionKind::Shell;

        // Token-shaped text in ordinary shell output must not fabricate usage.
        app.handle_session_output(id, "\x1b[31m100 input 200 output $0.01\x1b[0m".into());

        assert_eq!(
            app.sessions[0].output_lines.back().unwrap(),
            "100 input 200 output $0.01"
        );
        assert_eq!(app.sessions[0].state, SessionState::Running);
        assert_eq!(app.sessions[0].stats.input_tokens, 0);
        assert_eq!(app.sessions[0].stats.output_tokens, 0);
        assert_eq!(app.sessions[0].stats.total_cost_usd, 0.0);
    }

    #[test]
    fn session_output_scrapes_stats_only_for_local_agent_sessions() {
        let mut app = make_app();
        let id = app.spawn_headless_session("agent".into(), None).unwrap();
        app.sessions[0].base = crate::session::BaseKind::LocalAgent;

        app.handle_session_output(id, "100 input 200 output $0.01".into());

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

        assert_eq!(app.pane_sizes[0], (12, 80));
        assert_eq!(app.sessions[0].screen.screen().size(), (12, 80));
        assert_eq!(rx.try_recv().unwrap(), (12, 80));
    }

    #[test]
    fn split_focus_controls_active_session_and_session_switching() {
        let mut app = make_app();
        app.spawn_headless_session("one".into(), None).unwrap();
        app.spawn_headless_session("two".into(), None).unwrap();
        app.spawn_headless_session("three".into(), None).unwrap();

        app.toggle_split();
        assert_eq!(app.layout, LayoutMode::SplitV);
        assert_eq!(app.panes, [Some(0), Some(1)]);
        assert_eq!(app.active_idx(), Some(0));

        app.focus_next_pane();
        assert_eq!(app.focused_pane, 1);
        assert_eq!(app.active_idx(), Some(1));

        app.switch_to(2);
        assert_eq!(app.panes, [Some(0), Some(2)]);
        app.switch_to(0);
        assert_eq!(
            app.panes,
            [Some(0), Some(2)],
            "a session cannot be displayed in both panes"
        );
    }

    #[test]
    fn rotate_split_flips_direction_and_toggle_remembers_it() {
        let mut app = make_app();
        app.spawn_headless_session("one".into(), None).unwrap();
        app.spawn_headless_session("two".into(), None).unwrap();

        app.rotate_split();
        assert_eq!(
            app.layout,
            LayoutMode::Single,
            "rotate is a no-op in Single"
        );

        app.toggle_split();
        assert_eq!(app.layout, LayoutMode::SplitV);
        let panes = app.panes;
        app.focus_next_pane();

        app.rotate_split();
        assert_eq!(app.layout, LayoutMode::SplitH);
        assert_eq!(app.panes, panes, "rotation preserves the pane pair");
        assert_eq!(app.focused_pane, 1, "rotation preserves focus");

        app.focus_next_pane();
        assert_eq!(app.focused_pane, 0, "pane focus works in SplitH");

        // Collapse and reopen: the split comes back stacked.
        app.toggle_split();
        assert_eq!(app.layout, LayoutMode::Single);
        app.toggle_split();
        assert_eq!(app.layout, LayoutMode::SplitH);
    }

    #[test]
    fn collapsing_split_keeps_the_focused_session() {
        let mut app = make_app();
        app.spawn_headless_session("one".into(), None).unwrap();
        app.spawn_headless_session("two".into(), None).unwrap();
        app.toggle_split();
        app.focus_next_pane();

        app.toggle_split();

        assert_eq!(app.layout, LayoutMode::Single);
        assert_eq!(app.focused_pane, 0);
        assert_eq!(app.panes, [Some(1), None]);
        assert_eq!(app.active_idx(), Some(1));
    }

    #[test]
    fn killing_split_pane_session_clears_it_and_preserves_other_pane() {
        let mut app = make_app();
        app.spawn_headless_session("one".into(), None).unwrap();
        app.spawn_headless_session("two".into(), None).unwrap();
        app.spawn_headless_session("three".into(), None).unwrap();
        app.toggle_split();
        app.focus_next_pane();

        app.kill_active_session();

        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.panes, [Some(0), None]);
        assert_eq!(app.focused_pane, 1);

        app.panes[1] = Some(1);
        app.focus_next_pane();
        app.kill_active_session();
        assert_eq!(app.panes, [None, Some(0)]);
    }

    #[test]
    fn split_resize_updates_visible_sessions_per_pane_and_hidden_to_focused_size() {
        let mut app = make_app();
        let first = app.spawn_headless_session("one".into(), None).unwrap();
        let second = app.spawn_headless_session("two".into(), None).unwrap();
        let hidden = app.spawn_headless_session("hidden".into(), None).unwrap();
        let (first_tx, mut first_rx) = mpsc::channel(1);
        let (second_tx, mut second_rx) = mpsc::channel(1);
        let (hidden_tx, mut hidden_rx) = mpsc::channel(1);
        app.handle_session_resizer(first, first_tx);
        app.handle_session_resizer(second, second_tx);
        app.handle_session_resizer(hidden, hidden_tx);
        app.toggle_split();

        app.handle_pane_resize([(12, 40), (12, 39)]);

        assert_eq!(app.pane_sizes, [(12, 40), (12, 39)]);
        assert_eq!(app.sessions[0].screen.screen().size(), (12, 40));
        assert_eq!(app.sessions[1].screen.screen().size(), (12, 39));
        assert_eq!(first_rx.try_recv().unwrap(), (12, 40));
        assert_eq!(second_rx.try_recv().unwrap(), (12, 39));
        // Hidden sessions track the focused pane's size so their programs
        // see the SIGWINCH immediately instead of on switch_to.
        assert_eq!(app.sessions[2].screen.screen().size(), (12, 40));
        assert_eq!(hidden_rx.try_recv().unwrap(), (12, 40));
    }

    #[test]
    fn window_resize_reaches_hidden_sessions_in_single_layout() {
        let mut app = make_app();
        app.spawn_headless_session("visible".into(), None).unwrap();
        let hidden = app.spawn_headless_session("hidden".into(), None).unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        app.handle_session_resizer(hidden, tx);

        app.handle_resize(20, 100);

        assert_eq!(app.sessions[1].screen.screen().size(), (20, 100));
        assert_eq!(rx.try_recv().unwrap(), (20, 100));
    }

    #[test]
    fn switching_a_pane_resizes_the_newly_visible_session() {
        let mut app = make_app();
        app.spawn_headless_session("one".into(), None).unwrap();
        app.spawn_headless_session("two".into(), None).unwrap();
        let third = app.spawn_headless_session("three".into(), None).unwrap();
        app.toggle_split();
        app.handle_pane_resize([(12, 40), (12, 39)]);
        let (tx, mut rx) = mpsc::channel(1);
        app.handle_session_resizer(third, tx);
        app.focus_next_pane();

        app.switch_to(2);

        assert_eq!(app.sessions[2].screen.screen().size(), (12, 39));
        assert_eq!(rx.try_recv().unwrap(), (12, 39));
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
    fn broadcast_writes_json_lines_only_to_agents_in_the_named_group() {
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

        assert_eq!(a_rx.try_recv().unwrap(), "{\"type\":\"ping\"}\n");
        assert!(a_rx.try_recv().is_err());
        assert!(b_rx.try_recv().is_err());
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
    fn partial_line_prompt_resolves_pending_wait_ready_reply() {
        let mut app = make_app();
        let id = app.spawn_headless_session("shell".into(), None).unwrap();
        app.sessions[0].state = SessionState::Running;
        app.sessions[0].push_output_line("total 4".into());
        let (resp_tx, mut resp_rx) = tokio::sync::oneshot::channel();
        app.pending_ipc_replies.insert(id, (resp_tx, 0));

        // Shell prompts are partial lines (no trailing newline); the READY
        // they trigger must resolve the parked wait-ready reply.
        app.handle_session_current_line(id, "user@host:~$ ".into());

        assert_eq!(app.sessions[0].state, SessionState::Ready);
        let resp = resp_rx.try_recv().expect("reply resolved on prompt");
        assert_eq!(resp["session_id"], id);
        assert_eq!(resp["lines"][0], "total 4");
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
    #[tokio::test]
    async fn command_bar_new_custom_takes_full_command_line() {
        let mut app = make_app();
        app.command_input = "new custom CLAUDE_CONFIG_DIR=/tmp/w claude --continue".into();
        app.execute_command();
        assert_eq!(app.sessions.len(), 1);
        let s = &app.sessions[0];
        assert_eq!(
            s.kind,
            SessionKind::Custom("CLAUDE_CONFIG_DIR=/tmp/w claude --continue".into())
        );
        // env-prefixed claude resolves to the Claude identity
        assert_eq!(s.base, crate::session::BaseKind::Claude);

        // single-token form still works: `new <cmd> [name]`
        let mut app = make_app();
        app.command_input = "new mytool build".into();
        app.execute_command();
        assert_eq!(app.sessions[0].kind, SessionKind::Custom("mytool".into()));
        assert_eq!(app.sessions[0].name, "build");
    }
    #[tokio::test]
    async fn chat_addresses_sessions_and_captures_ready_reply() {
        let mut app = make_app();
        let id = app
            .spawn_headless_session("critic".to_string(), None)
            .unwrap();

        // Bare message with no target yet → guidance, no pending
        app.chat.input = "hello?".into();
        app.chat_send();
        assert!(app.chat.pending.is_empty());
        assert!(app.chat.messages.last().unwrap().text.contains("@name"));

        // Address the session by name
        app.chat.input = "@critic review this".into();
        app.chat_send();
        assert_eq!(app.chat.pending.len(), 1);
        assert_eq!(app.chat.target.as_deref(), Some("critic"));
        assert_eq!(app.chat.messages.last().unwrap().from, "you → critic");

        // Session produces output, then hits READY → reply lands in chat
        if let Some(s) = app.sessions.iter_mut().find(|s| s.id == id) {
            s.push_output_line("the fix looks correct".to_string());
        }
        app.check_chat_pending(id, &SessionState::Ready);
        assert!(app.chat.pending.is_empty());
        let last = app.chat.messages.last().unwrap();
        assert_eq!(last.from, "critic");
        assert!(last.text.contains("looks correct"));

        // Bare follow-up reuses the sticky target
        app.chat.input = "thanks".into();
        app.chat_send();
        assert_eq!(app.chat.pending.len(), 1);
    }

    #[tokio::test]
    async fn chat_slash_runs_command_bar_commands_without_leaving_chat() {
        let mut app = make_app();
        app.mode = AppMode::Chat;
        app.chat.input = "/council status".into();
        app.chat_send();
        assert!(matches!(app.mode, AppMode::Chat));
        assert!(app
            .chat
            .messages
            .last()
            .unwrap()
            .text
            .contains("no council running"));
    }

    #[test]
    fn chat_local_agent_history_is_bounded_and_roled() {
        let mut app = make_app();
        for i in 0..50 {
            app.chat
                .histories
                .entry("qwen".to_string())
                .or_default()
                .push(("user".to_string(), format!("m{}", i)));
        }
        app.handle_chat_reply("qwen".to_string(), "answer".to_string());
        let h = &app.chat.histories["qwen"];
        assert_eq!(
            h.last().unwrap(),
            &("assistant".to_string(), "answer".to_string())
        );
        assert_eq!(app.chat.messages.last().unwrap().from, "qwen");
    }

    #[test]
    fn grant_command_swaps_capability_tiers() {
        let mut app = make_app();
        let id = app.spawn_headless_session("w".to_string(), None).unwrap();
        app.command_input = format!("grant {} council", id + 1);
        app.execute_command();
        assert_eq!(app.caps[&id], crate::auth::council_caps());
        app.command_input = format!("grant {} operator", id + 1);
        app.execute_command();
        assert_eq!(app.caps[&id], crate::auth::operator_caps());
    }
    #[tokio::test]
    async fn scrollback_is_unified_across_screen_modes() {
        let mut app = make_app();
        let id = app.spawn_headless_session("tui".to_string(), None).unwrap();
        app.panes[0] = app.sessions.iter().position(|s| s.id == id);
        {
            let s = app.sessions.iter_mut().find(|s| s.id == id).unwrap();
            for i in 0..100 {
                s.push_output_line(format!("history-{}", i));
            }
            // Enter the alternate screen, like claude/codex/opencode do.
            s.process_bytes(b"\x1b[?1049h");
            assert!(s.screen.screen().alternate_screen());
        }
        // Scrolling a full-screen app walks our captured history…
        app.scroll_up(20);
        assert_eq!(app.sessions[0].history_scroll, 20);
        assert_eq!(app.scroll_offset(), 20);
        // …is clamped to what exists…
        app.scroll_up(500);
        assert_eq!(app.sessions[0].history_scroll, 100);
        // …new output does NOT yank the view back…
        app.handle_session_bytes(id, b"more output\r\n".to_vec());
        assert_eq!(app.sessions[0].history_scroll, 100);
        // …and typing returns to the live tail.
        app.write_to_active(b"x");
        assert_eq!(app.sessions[0].history_scroll, 0);
        assert_eq!(app.scroll_offset(), 0);
    }

    fn orch_request(
        app: &mut App,
        req: crate::events::OrchestratorReq,
    ) -> Option<serde_json::Value> {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.handle_orchestrator_request(req, tx);
        rx.try_recv().ok()
    }

    #[test]
    fn kill_request_needs_confirmation_and_confirm_removes_the_session() {
        let mut app = make_app();
        let id = app.spawn_headless_session("victim".into(), None).unwrap();

        let resp = orch_request(
            &mut app,
            crate::events::OrchestratorReq::RequestKill {
                session_id: id,
                reason: "stuck".into(),
            },
        )
        .unwrap();
        assert_eq!(resp["status"], "pending_user_confirmation");
        assert!(app.pending_kill.is_some());
        // The request is announced in chat, nothing is killed yet.
        assert_eq!(app.sessions.len(), 1);
        assert!(app
            .chat
            .messages
            .iter()
            .any(|m| m.text.contains("/confirm-kill")));

        app.command_input = "confirm-kill".into();
        app.execute_command();
        assert!(app.sessions.is_empty());
        assert!(app.pending_kill.is_none());
    }

    #[test]
    fn deny_kill_keeps_the_session_and_bad_targets_error() {
        let mut app = make_app();
        let id = app.spawn_headless_session("victim".into(), None).unwrap();

        let resp = orch_request(
            &mut app,
            crate::events::OrchestratorReq::RequestKill {
                session_id: id,
                reason: String::new(),
            },
        )
        .unwrap();
        assert_eq!(resp["status"], "pending_user_confirmation");
        app.command_input = "deny-kill".into();
        app.execute_command();
        assert_eq!(app.sessions.len(), 1);
        assert!(app.pending_kill.is_none());

        // Unknown session
        let resp = orch_request(
            &mut app,
            crate::events::OrchestratorReq::RequestKill {
                session_id: 99,
                reason: String::new(),
            },
        )
        .unwrap();
        assert!(resp["error"].is_string());

        // The orchestrator may not request its own death
        app.orchestrator_session_id = Some(id);
        let resp = orch_request(
            &mut app,
            crate::events::OrchestratorReq::RequestKill {
                session_id: id,
                reason: String::new(),
            },
        )
        .unwrap();
        assert!(resp["error"].as_str().unwrap().contains("own kill"));
    }

    #[test]
    fn output_query_returns_session_tail() {
        let mut app = make_app();
        let id = app.spawn_headless_session("noisy".into(), None).unwrap();
        {
            let s = app.sessions.iter_mut().find(|s| s.id == id).unwrap();
            for i in 0..10 {
                s.push_output_line(format!("line-{}", i));
            }
        }
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.handle_ipc_query(
            crate::events::IpcQueryPayload::Query {
                what: format!("output:{}:3", id),
            },
            tx,
        );
        let resp = rx.try_recv().unwrap();
        let lines = resp["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], "line-9");

        // list_sessions tool shape carries both id and 1-based display
        let resp = orch_request(&mut app, crate::events::OrchestratorReq::ListSessions).unwrap();
        assert_eq!(resp[0]["id"], id);
        assert_eq!(resp[0]["display"], id + 1);
    }

    #[test]
    fn dead_orchestrator_task_is_noticed_once_and_cleared() {
        let mut app = make_app();
        let (otx, orx) = mpsc::channel(1);
        app.orchestrator = Some(crate::orchestrator::OrchestratorHandle::detached(
            otx,
            "agent".into(),
        ));
        drop(orx); // the task is gone
        app.handle_tick();
        assert!(app.orchestrator.is_none());
        assert!(app
            .chat
            .messages
            .iter()
            .any(|m| m.text.contains("/orchestrator restart")));
        // No duplicate notice on later ticks
        let notices = app.chat.messages.len();
        app.handle_tick();
        assert_eq!(app.chat.messages.len(), notices);
    }

    #[tokio::test]
    async fn orchestrator_restart_replaces_a_dead_handle() {
        let mut app = make_app();
        let (otx, orx) = mpsc::channel(1);
        app.orchestrator = Some(crate::orchestrator::OrchestratorHandle::detached(
            otx,
            "agent".into(),
        ));
        drop(orx);
        app.command_input = "orchestrator restart".into();
        app.execute_command();
        assert_eq!(app.command_result, "orchestrator restarted");
        assert!(app.orchestrator.as_ref().is_some_and(|h| !h.tx.is_closed()));
    }

    #[test]
    fn approve_and_deny_resolve_the_pending_proposal() {
        let mut app = make_app();
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.handle_orchestrator_proposal("send_input".into(), "session 2 ← \"ls\"".into(), tx);
        assert!(app.pending_proposal.is_some());
        assert!(app
            .chat
            .messages
            .iter()
            .any(|m| m.text.contains("/approve")));

        app.resolve_pending_proposal(true, String::new());
        assert!(app.pending_proposal.is_none());
        assert!(matches!(
            rx.try_recv(),
            Ok(crate::events::ProposalVerdict::Approved)
        ));

        // Deny with a reason on a fresh proposal.
        let (tx2, mut rx2) = tokio::sync::oneshot::channel();
        app.handle_orchestrator_proposal("start_session".into(), "claude \"w\"".into(), tx2);
        app.resolve_pending_proposal(false, "not now".into());
        match rx2.try_recv() {
            Ok(crate::events::ProposalVerdict::Denied(reason)) => assert_eq!(reason, "not now"),
            _ => panic!("expected Denied"),
        }

        // Nothing pending: friendly no-op.
        app.resolve_pending_proposal(true, String::new());
        assert_eq!(app.command_result, "no pending proposal");
    }

    #[test]
    fn orchestrator_events_respect_filter_cooldown_and_self_exclusion() {
        let mut app = make_app();
        let watched = app.spawn_headless_session("worker".into(), None).unwrap();
        let orch = app.spawn_headless_session("agent".into(), None).unwrap();
        app.orchestrator_session_id = None;
        let (otx, mut orx) = mpsc::channel(8);
        app.orchestrator = Some(crate::orchestrator::OrchestratorHandle::detached(
            otx,
            "agent".into(),
        ));

        // READY is in the default event list (completion notifications)
        app.notify_orchestrator(watched, &SessionState::Ready);
        assert!(orx.try_recv().is_ok());
        // …but not twice within the cooldown
        app.notify_orchestrator(watched, &SessionState::Ready);
        assert!(orx.try_recv().is_err());

        // WAITING fires…
        app.notify_orchestrator(watched, &SessionState::Waiting);
        assert!(orx.try_recv().is_ok());
        // …but not twice within the cooldown
        app.notify_orchestrator(watched, &SessionState::Waiting);
        assert!(orx.try_recv().is_err());
        // A different state for the same session still fires
        app.notify_orchestrator(watched, &SessionState::Error);
        assert!(orx.try_recv().is_ok());

        // Events about the orchestrator's own session are suppressed
        app.orchestrator_session_id = Some(orch);
        app.notify_orchestrator(orch, &SessionState::Waiting);
        assert!(orx.try_recv().is_err());
    }

    #[test]
    fn tick_detected_completion_reaches_the_orchestrator() {
        let mut app = make_app();
        let id = app.spawn_headless_session("worker".into(), None).unwrap();
        let (otx, mut orx) = mpsc::channel(8);
        app.orchestrator = Some(crate::orchestrator::OrchestratorHandle::detached(
            otx,
            "agent".into(),
        ));

        // Simulate an agent CLI that streamed output and then went quiet:
        // Running with the last output more than 2s ago.
        {
            let s = app.sessions.iter_mut().find(|s| s.id == id).unwrap();
            s.state = SessionState::Running;
            s.last_output_at = Some(std::time::Instant::now() - std::time::Duration::from_secs(3));
        }

        app.handle_tick();

        let s = app.sessions.iter().find(|s| s.id == id).unwrap();
        assert_eq!(s.state, SessionState::Ready);
        // The tick-detected transition must reach the orchestrator: this
        // previously bypassed on_state_transition and never fired.
        let msg = orx.try_recv();
        assert!(
            matches!(
                msg,
                Ok(crate::orchestrator::OrchestratorMsg::SessionEvent { ref state, .. })
                    if state == "READY"
            ),
            "expected a ready SessionEvent, got {:?}",
            msg.is_ok()
        );
    }
}
