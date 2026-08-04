use std::collections::VecDeque;
use std::time::Instant;
use tokio::sync::mpsc;

pub const MAX_SESSIONS: usize = 8;
pub const PTY_ROWS: u16 = 40;
pub const PTY_COLS: u16 = 200;

#[derive(Debug, Clone, PartialEq)]
pub enum SessionKind {
    Claude,
    Codex,
    OpenCode,
    OhMyPi,
    Aider,
    Shell,
    Custom(String),
}

/// Display order for the new-session Kind dropdown. `Custom` must stay last —
/// the dialog treats the final index as "show the Command field".
pub const KIND_LABELS: &[&str] = &[
    "Claude", "Codex", "OpenCode", "Oh My Pi", "Aider", "Shell", "Custom",
];

impl SessionKind {
    /// Number of selectable kinds in the new-session dialog.
    pub const COUNT: usize = KIND_LABELS.len();

    /// Build a kind from its dropdown index. `Custom` carries the command
    /// entered in the dialog.
    pub fn from_index(index: usize, custom_cmd: &str) -> SessionKind {
        match index {
            0 => SessionKind::Claude,
            1 => SessionKind::Codex,
            2 => SessionKind::OpenCode,
            3 => SessionKind::OhMyPi,
            4 => SessionKind::Aider,
            5 => SessionKind::Shell,
            _ => SessionKind::Custom(custom_cmd.to_string()),
        }
    }

    /// Parse a kind name as used in profiles and `linkshell-ctl` (`custom`
    /// is handled separately by callers because it carries a command).
    pub fn from_name(name: &str) -> Option<SessionKind> {
        match name {
            "claude" => Some(SessionKind::Claude),
            "codex" => Some(SessionKind::Codex),
            "opencode" => Some(SessionKind::OpenCode),
            "oh-my-pi" | "ohmypi" | "omp" => Some(SessionKind::OhMyPi),
            "aider" => Some(SessionKind::Aider),
            "shell" => Some(SessionKind::Shell),
            _ => None,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            SessionKind::Claude => "claude",
            SessionKind::Codex => "codex",
            SessionKind::OpenCode => "opencode",
            SessionKind::OhMyPi => "oh-my-pi",
            SessionKind::Aider => "aider",
            SessionKind::Shell => "shell",
            SessionKind::Custom(_) => "custom",
        }
    }

    /// True for first-class local/open coding agent kinds (state inference
    /// via BaseKind::LocalAgent, no JSONL watcher).
    pub fn is_local_agent_kind(&self) -> bool {
        matches!(
            self,
            SessionKind::OpenCode | SessionKind::OhMyPi | SessionKind::Aider
        )
    }

    pub fn is_claude_based(&self) -> bool {
        matches!(self, SessionKind::Claude)
            || self
                .custom_base_name()
                .map(is_claude_basename)
                .unwrap_or(false)
    }

    pub fn is_codex_based(&self) -> bool {
        matches!(self, SessionKind::Codex)
            || self
                .custom_base_name()
                .map(is_codex_basename)
                .unwrap_or(false)
    }

    /// True for TUI-based agent sessions whose transcript is useful as
    /// scrollback, and which therefore get our own capture rather than
    /// vt100's. Full-repaint dashboards (htop, btop) would spew garbage if
    /// enabled here.
    ///
    /// Two different reasons these sessions have no usable vt100 scrollback:
    /// claude lives on the alternate screen, where there is none by design;
    /// codex stays on the normal screen but scrolls inside a DECSTBM region,
    /// and vt100 (correctly, per the DEC spec) discards lines evicted from a
    /// restricted region instead of pushing them to scrollback.
    pub fn captures_scrollback(&self) -> bool {
        matches!(self, SessionKind::Claude | SessionKind::Codex)
    }

    pub(crate) fn custom_base_name_pub(&self) -> Option<&str> {
        self.custom_base_name()
    }

    fn custom_base_name(&self) -> Option<&str> {
        if let SessionKind::Custom(cmd) = self {
            command_base_name(cmd)
        } else {
            None
        }
    }
}

/// Split a chunk after every DECSTBM (`ESC [ params r`) sequence.
///
/// Each item is a slice to feed vt100 and, when that slice ended with a
/// DECSTBM, its parameters — so the caller can mirror the region change vt100
/// applies but does not expose. The escape stays inside the slice; only the
/// caller's copy of the region is updated afterwards.
fn decstbm_segments(data: &[u8]) -> Vec<(&[u8], Option<Vec<u8>>)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] != 0x1b || data[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let mut j = i + 2;
        while j < data.len() && (data[j].is_ascii_digit() || data[j] == b';') {
            j += 1;
        }
        if j < data.len() && data[j] == b'r' {
            out.push((&data[start..=j], Some(data[i + 2..j].to_vec())));
            start = j + 1;
            i = j + 1;
        } else {
            i = j.max(i + 2);
        }
    }
    if start < data.len() || out.is_empty() {
        out.push((&data[start..], None));
    }
    out
}

/// Upper bound on how finely one segment is split for capture. A burst with
/// more newlines than this has already replaced everything on screen several
/// times over, so the older lines are unrecoverable anyway and there is
/// nothing to buy by rendering the region for each one.
const MAX_SCROLL_PIECES: usize = 512;

/// Split a segment so each piece performs at most one scroll: after every
/// newline, which is what scrolls a region in practice.
fn scroll_pieces(segment: &[u8]) -> Vec<&[u8]> {
    let mut out: Vec<&[u8]> = Vec::new();
    let mut start = 0;
    for (i, b) in segment.iter().enumerate() {
        if *b == b'\n' {
            out.push(&segment[start..=i]);
            start = i + 1;
            if out.len() >= MAX_SCROLL_PIECES {
                break;
            }
        }
    }
    if start < segment.len() {
        out.push(&segment[start..]);
    }
    if out.is_empty() {
        out.push(segment);
    }
    out
}

/// Whether a slice could possibly scroll the screen. Snapshotting the region
/// costs a row render, and the overwhelming majority of a repainting TUI's
/// output is cursor positioning that moves nothing.
fn may_scroll(segment: &[u8]) -> bool {
    segment.contains(&b'\n')
        || segment.contains(&0x0b)
        || segment.contains(&0x0c)
        || segment
            .windows(2)
            .any(|w| w == b"\x1bD" || w == b"\x1bM" || w == b"\x1bE")
        || find_final(segment, b'S')
        || find_final(segment, b'T')
}

/// True when the slice contains a CSI sequence with the given final byte.
fn find_final(data: &[u8], final_byte: u8) -> bool {
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] != 0x1b || data[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let mut j = i + 2;
        while j < data.len() && (data[j].is_ascii_digit() || data[j] == b';') {
            j += 1;
        }
        if j < data.len() && data[j] == final_byte {
            return true;
        }
        i = j.max(i + 2);
    }
    false
}

/// Parse DECSTBM parameters (`top;bottom`, 1-based inclusive) into a 0-based
/// inclusive row range. An empty or degenerate region means "the whole
/// screen", which is what resets it.
fn parse_decstbm(params: &[u8], rows: u16) -> Option<(u16, u16)> {
    let text = std::str::from_utf8(params).ok()?;
    let mut parts = text.split(';');
    let top: u16 = parts.next().unwrap_or("").trim().parse().unwrap_or(1);
    let bottom: u16 = parts
        .next()
        .unwrap_or("")
        .trim()
        .parse()
        .unwrap_or(rows.max(1));
    let top = top.max(1) - 1;
    let bottom = bottom.max(1).min(rows.max(1)) - 1;
    if top >= bottom {
        return None;
    }
    Some((top, bottom))
}

/// How many lines scrolled off the top of the region between two snapshots:
/// where the new top row used to sit in the old ones.
///
/// Anchoring on the top row rather than matching the whole overlap is what
/// makes this work on a live screen. The obvious formulation — the smallest
/// `k` with `prev[k..] == cur[..n-k]` — never matches in practice, because
/// the same chunk that scrolled also wrote new content into the bottom rows,
/// so the two slices always disagree at the tail.
///
/// A blank new top row is treated as "no scroll": it identifies nothing, and
/// blank lines are not worth reconstructing history from. The row below is
/// checked as corroboration so a screen that merely repeats a line does not
/// read as having scrolled to it.
fn scrolled_off(prev: &[String], cur: &[String]) -> usize {
    let n = prev.len();
    if n == 0 || cur.is_empty() {
        return 0;
    }
    let head = cur[0].trim();
    if head.is_empty() {
        return 0;
    }
    for k in 1..n {
        if prev[k].trim() != head {
            continue;
        }
        if k + 1 < n && cur.len() >= 2 && prev[k + 1] != cur[1] {
            continue;
        }
        return k;
    }
    0
}

/// True when a command basename is `claude` or a claude wrapper following the
/// common naming convention: `claude-work`, `claude.sh`, `claude_glm`. Wrappers
/// with unrelated names still need a [sessions.aliases] entry.
pub fn is_claude_basename(name: &str) -> bool {
    name.strip_prefix("claude")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['-', '.', '_']))
}

/// Codex analogue of [`is_claude_basename`].
pub fn is_codex_basename(name: &str) -> bool {
    name.strip_prefix("codex")
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['-', '.', '_']))
}

/// Basename of the actual binary in a command line, skipping leading
/// `VAR=value` environment assignments — so `CLAUDE_CONFIG_DIR=~/w claude -c`
/// resolves to `claude`.
pub fn command_base_name(cmd: &str) -> Option<&str> {
    let first = cmd.split_whitespace().find(|tok| !is_env_assignment(tok))?;
    std::path::Path::new(first).file_name()?.to_str()
}

/// A token is an env assignment if it has `NAME=` where NAME is a valid
/// identifier (letters, digits, underscore; not starting with a digit).
fn is_env_assignment(tok: &str) -> bool {
    match tok.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && !name.starts_with(|c: char| c.is_ascii_digit())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// Extract the value of a leading `VAR=value` assignment from a command line.
/// Only assignments *before* the command word count; strips simple quoting.
pub fn command_env_assignment(cmd: &str, var: &str) -> Option<String> {
    for tok in cmd.split_whitespace() {
        if !is_env_assignment(tok) {
            return None; // reached the command word
        }
        let (name, value) = tok.split_once('=')?;
        if name == var {
            let value = value.trim_matches('"').trim_matches('\'');
            return Some(value.to_string());
        }
    }
    None
}

/// The underlying CLI identity of a session, independent of how the command
/// was spelled (direct, env-prefixed, or aliased via config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseKind {
    Claude,
    Codex,
    /// A recognized local/open coding agent or LLM CLI (opencode, oh-my-pi,
    /// pi, aider, llama.cpp interactive). Gets agent-style state inference
    /// but no JSONL watcher — these tools have no common log format.
    LocalAgent,
    Other,
}

/// Command basenames recognized as local coding agents / LLM CLIs.
pub const LOCAL_AGENT_BASENAMES: &[&str] = &[
    "opencode",
    "omp",
    "pi",
    "aider",
    "llama-cli",
    "llama",
    "ollama",
];

pub fn is_local_agent_basename(name: &str) -> bool {
    LOCAL_AGENT_BASENAMES.contains(&name)
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SessionState {
    Starting,
    Ready,
    Thinking,
    Running,
    Waiting,
    Error,
    Dead,
}

impl SessionState {
    pub fn label(&self) -> &str {
        match self {
            SessionState::Starting => "STARTING",
            SessionState::Ready => "READY",
            SessionState::Thinking => "THINKING",
            SessionState::Running => "RUNNING",
            SessionState::Waiting => "WAITING!",
            SessionState::Error => "ERROR",
            SessionState::Dead => "DEAD",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_cost_usd: f64,
    /// Total input tokens for the most recent API call — reflects current context window size.
    pub context_tokens: u64,
}

pub struct Session {
    pub id: usize,
    pub name: String,
    pub kind: SessionKind,
    pub state: SessionState,
    pub waiting_prompt: Option<String>,
    pub last_notified: Option<Instant>,
    /// No PTY — managed entirely via IPC socket.
    pub headless: bool,
    /// True for the resident CLI-class orchestrator session: excluded from
    /// @all broadcasts and from proactive orchestrator events about itself.
    pub is_orchestrator: bool,
    /// Not shown in the session bar / status panel and unreachable by
    /// session switching. Still fully alive (PTY, state inference, IPC);
    /// used for the CLI-class orchestrator so it lives behind the chat pane.
    pub hidden: bool,
    /// OS process id of the PTY child, once known. Used for pause/resume
    /// (SIGSTOP/SIGCONT on the child's process group).
    pub child_pid: Option<u32>,
    /// Process is stopped with SIGSTOP. The session stays alive (PTY, screen,
    /// stats) but produces no output and consumes no CPU until resumed.
    pub paused: bool,
    /// When true, state was set by IPC and should not be auto-reverted by the tick timeout.
    /// Cleared when pattern matching updates the state from PTY output.
    pub ipc_state: bool,
    /// When ipc_state was last set; used to expire stale overrides in handle_tick.
    pub ipc_state_set_at: Option<Instant>,
    /// The current WAITING state came from terminal pattern matching (a
    /// permission dialog / question on screen), not from a watcher or IPC.
    /// Dialogs are invisible to the JSONL/db watchers, so this state has to
    /// survive their Running/Thinking reports — but it must also be *given
    /// up* once the dialog is gone, or the session parks on WAITING forever.
    pub pattern_waiting: bool,
    /// Last state reported by an authoritative source (JSONL/db watcher or an
    /// IPC `state` message). Restored when a pattern-derived WAITING clears,
    /// so the session doesn't fall back to a stale terminal-scraped guess.
    pub watcher_state: Option<SessionState>,
    /// Agent group for broadcast addressing and group-triggered pipes.
    pub group: Option<String>,
    /// vt100 screen buffer — updated with raw PTY bytes, used for display
    pub screen: vt100::Parser,
    pub stats: TokenStats,
    /// Model ID reported in the CLI's JSONL log, once known.
    pub model: Option<String>,
    /// Maximum context window in tokens, when discoverable (LM Studio /
    /// llama-server API probe, or llama-cli's "n_ctx = N" startup line).
    pub context_max: u64,
    /// A ctx_probe task is already running for this session.
    pub ctx_probe_spawned: bool,
    pub pro_sub: bool,
    pub started_at: Instant,
    #[allow(dead_code)] // only read in tests; kept for debugging and future use
    pub cwd: String,
    /// Send bytes to the PTY writer task
    pub pty_writer: Option<mpsc::Sender<Vec<u8>>>,
    /// Send resize events to the PTY writer task
    pub pty_resizer: Option<mpsc::Sender<(u16, u16)>>,
    /// Stripped output lines as they arrive from the PTY reader, for pattern
    /// matching, pipe extraction and `read`. For a repainting TUI this is
    /// mostly repaint fragments — which is fine for those consumers and
    /// useless as scrollback, hence the separate buffer below.
    pub output_lines: VecDeque<String>,
    /// The transcript recovered from the screen as it scrolls: what the
    /// scrollback view walks. Kept apart from `output_lines` because that one
    /// is a stream of whatever crossed the PTY, and for claude and codex the
    /// two have almost nothing to do with each other.
    pub scrollback_lines: VecDeque<String>,
    pub scroll_buffer_lines: usize,
    /// Raw bytes received since the last tick — used to detect active generation
    /// without relying on newlines (Claude Code streams via cursor movement, not \n).
    pub bytes_since_last_tick: usize,
    /// Scroll offset (in lines) into `output_lines` history. Used for
    /// sessions whose scrollback we capture ourselves because vt100 has none
    /// to offer; unifies scrolling across session types.
    pub history_scroll: usize,
    /// The DECSTBM scrolling region (top, bottom), inclusive and 0-based.
    /// vt100 tracks this internally but does not expose it, and we need it:
    /// only rows inside the region move when the app scrolls, so it is the
    /// only slice of the screen worth diffing for evicted lines.
    scroll_region: (u16, u16),
    /// Resolved CLI identity: which JSONL watcher / stats pipeline applies.
    /// Defaults from the command's base name; spawn_session refines it with
    /// the config alias table.
    pub base: BaseKind,
    /// Last time we received output from this session; used for idle timeout detection.
    pub last_output_at: Option<Instant>,
    /// A dedicated log/db watcher reports authoritative cumulative stats for
    /// this session, so terminal line-scraping must not touch `stats`.
    pub stats_from_watcher: bool,
    /// Optional path to log session output lines to disk.
    pub log_path: Option<std::path::PathBuf>,
    /// Hash of the last rendered screen contents, used by `process_bytes` to
    /// tell whether a byte chunk actually changed the visible frame. Full-screen
    /// TUIs (opencode especially) repaint continuously with byte-identical
    /// frames; suppressing redraws for those keeps the render loop off the CPU.
    last_screen_hash: u64,
}

/// Compact human count for status columns: 999, 42.3k, 999.9k, 1.23M, 99.9M,
/// 999M. Always ≤ 6 chars.
pub fn fmt_count(n: u64) -> String {
    let f = n as f64;
    if n < 1_000 {
        n.to_string()
    } else if f < 999_950.0 {
        format!("{:.1}k", f / 1_000.0)
    } else if f < 9_995_000.0 {
        format!("{:.2}M", f / 1_000_000.0)
    } else if f < 99_950_000.0 {
        format!("{:.1}M", f / 1_000_000.0)
    } else {
        format!("{:.0}M", f / 1_000_000.0)
    }
}

impl Session {
    pub fn new(
        id: usize,
        name: String,
        kind: SessionKind,
        cwd: String,
        rows: u16,
        cols: u16,
        scroll_buffer_lines: usize,
    ) -> Self {
        let base = if kind.is_claude_based() {
            BaseKind::Claude
        } else if kind.is_codex_based() {
            BaseKind::Codex
        } else if kind.is_local_agent_kind()
            || kind
                .custom_base_name_pub()
                .map(is_local_agent_basename)
                .unwrap_or(false)
        {
            BaseKind::LocalAgent
        } else {
            BaseKind::Other
        };
        Self {
            id,
            name,
            kind,
            state: SessionState::Starting,
            waiting_prompt: None,
            last_notified: None,
            headless: false,
            is_orchestrator: false,
            hidden: false,
            child_pid: None,
            paused: false,
            ipc_state: false,
            ipc_state_set_at: None,
            pattern_waiting: false,
            watcher_state: None,
            group: None,
            screen: vt100::Parser::new(rows, cols, 1000),
            stats: TokenStats::default(),
            model: None,
            context_max: 0,
            ctx_probe_spawned: false,
            pro_sub: false,
            started_at: Instant::now(),
            last_output_at: None,
            cwd,
            pty_writer: None,
            pty_resizer: None,
            output_lines: VecDeque::new(),
            scrollback_lines: VecDeque::new(),
            scroll_buffer_lines,
            bytes_since_last_tick: 0,
            history_scroll: 0,
            scroll_region: (0, rows.saturating_sub(1)),
            base,
            log_path: None,
            stats_from_watcher: false,
            last_screen_hash: 0,
        }
    }

    /// Feed raw PTY bytes into the vt100 screen. When `detect_change`, returns
    /// `true` if the visible frame changed as a result — callers use this to
    /// skip redundant redraws for full-screen TUIs that repaint with
    /// byte-identical output. The check hashes the whole formatted screen, so
    /// callers that cannot act on the answer (off-screen sessions) pass `false`
    /// and always get `false` back.
    pub fn process_bytes(&mut self, data: &[u8], detect_change: bool) -> bool {
        use std::hash::{Hash, Hasher};

        self.bytes_since_last_tick += data.len();
        if self.kind.captures_scrollback() {
            self.process_capturing(data);
        } else {
            self.screen.process(data);
        }

        if !detect_change {
            // Leave last_screen_hash stale on purpose: the first chunk after the
            // session becomes visible again then reads as changed and redraws.
            return false;
        }
        // contents_formatted is exactly what the display path renders from, so
        // an unchanged hash means the next frame would be pixel-identical.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.screen.screen().contents_formatted().hash(&mut hasher);
        let hash = hasher.finish();
        let changed = hash != self.last_screen_hash;
        self.last_screen_hash = hash;
        changed
    }

    /// Process a chunk while recovering the lines it scrolls away.
    ///
    /// Two levels of splitting, each for its own reason.
    ///
    /// The chunk is split at every DECSTBM sequence so each piece is handled
    /// under one stable scrolling region. Without that, the region can change
    /// mid-chunk — codex sets `ESC[1;25r`, scrolls, then resets with `ESC[r`,
    /// dozens of times a second — and a diff taken across the change compares
    /// the transcript against the composer codex pins below it, which reads
    /// as a scroll and captures the composer into the history.
    ///
    /// Within a piece that can scroll, it is split again at newlines, because
    /// a single before/after snapshot only shows the net movement. A chunk
    /// carrying six newlines scrolls six times, and the lines evicted by the
    /// first five are gone by the time we look.
    fn process_capturing(&mut self, data: &[u8]) {
        for (segment, region) in decstbm_segments(data) {
            if self.vt100_keeps_no_scrollback() && may_scroll(segment) {
                for piece in scroll_pieces(segment) {
                    let prev = self.region_rows();
                    self.screen.process(piece);
                    self.capture_evicted(&prev);
                }
            } else {
                self.screen.process(segment);
            }

            // vt100 has now applied the sequence; mirror it, since vt100
            // tracks the region internally but does not expose it.
            if let Some(params) = region {
                let rows = self.screen.screen().size().0;
                self.scroll_region =
                    parse_decstbm(&params, rows).unwrap_or((0, rows.saturating_sub(1)));
            }
        }
    }

    /// Append whatever scrolled off the top of the region since `prev`.
    fn capture_evicted(&mut self, prev: &[String]) {
        let cur = self.region_rows();
        let shift = scrolled_off(prev, &cur);
        for line in prev.iter().take(shift) {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            if self
                .scrollback_lines
                .back()
                .is_some_and(|last| last == line)
            {
                continue;
            }
            self.push_scrollback_line(line.to_string());
        }
    }

    /// True when vt100 will not retain what scrolls away, so recovering it is
    /// on us. Two independent reasons, one per agent CLI we support:
    ///
    /// - The alternate screen's grid is built as `Grid::new(size, 0)` — zero
    ///   scrollback, by construction. This is claude.
    /// - A restricted DECSTBM region: `Grid::scroll_up` only pushes to
    ///   scrollback `if !self.scroll_region_active()`, which is correct per
    ///   the DEC spec. This is codex, which stays on the normal screen and so
    ///   looked like an ordinary app to every check we had.
    fn vt100_keeps_no_scrollback(&self) -> bool {
        self.screen.screen().alternate_screen() || self.scroll_region_restricted()
    }

    /// True when the app has reserved part of the screen for itself.
    fn scroll_region_restricted(&self) -> bool {
        let rows = self.screen.screen().size().0;
        let (top, bottom) = self.scroll_region;
        top > 0 || bottom + 1 < rows
    }

    /// The scrolling region's rows as plain text.
    fn region_rows(&self) -> Vec<String> {
        let screen = self.screen.screen();
        let (rows, cols) = screen.size();
        let (top, bottom) = self.scroll_region;
        if top >= rows {
            return Vec::new();
        }
        let count = u32::from(bottom.min(rows - 1) - top) + 1;
        screen
            .rows(0, cols)
            .skip(usize::from(top))
            .take(count as usize)
            .map(|r| r.trim_end().to_string())
            .collect()
    }

    pub fn resize_screen(&mut self, rows: u16, cols: u16) {
        self.screen.set_size(rows, cols);
    }

    /// The lines the scrollback view scrolls through. Sessions we capture for
    /// use the recovered transcript; everything else falls back to the raw
    /// line stream, which is what an alt-screen shell (vim, less) scrolls
    /// back into.
    pub fn history_lines(&self) -> &VecDeque<String> {
        if self.kind.captures_scrollback() {
            &self.scrollback_lines
        } else {
            &self.output_lines
        }
    }

    pub fn push_scrollback_line(&mut self, line: String) {
        self.scrollback_lines.push_back(line);
        if self.scrollback_lines.len() > self.scroll_buffer_lines {
            self.scrollback_lines.pop_front();
        }
    }

    pub fn push_output_line(&mut self, line: String) {
        self.last_output_at = Some(Instant::now());
        self.output_lines.push_back(line);
        if self.output_lines.len() > self.scroll_buffer_lines {
            self.output_lines.pop_front();
        }
    }

    /// Session output as readable lines, for programmatic consumers
    /// (orchestrator tools, `linkshell-ctl read`, pipe extraction).
    ///
    /// Full-screen TUIs (claude, codex, opencode, …) live on the alternate
    /// screen and repaint via cursor movement and bare `\r`, so the
    /// newline-split `output_lines` buffer is sparse repaint noise for them.
    /// For those, render the current vt100 screen — exactly what the user
    /// sees. Line-oriented sessions (shells) keep their full line history.
    pub fn readable_lines(&self) -> Vec<String> {
        if self.screen.screen().alternate_screen() {
            let contents = self.screen.screen().contents();
            clean_tui_lines(contents.lines())
        } else {
            self.output_lines.iter().cloned().collect()
        }
    }

    /// The last `n` readable lines (see `readable_lines`).
    pub fn read_tail(&self, n: usize) -> Vec<String> {
        let mut lines = self.readable_lines();
        let start = lines.len().saturating_sub(n);
        lines.drain(..start);
        lines
    }

    /// Apply a state inferred from terminal output.
    ///
    /// Terminal patterns are the only source that can see permission dialogs
    /// and questions, so WAITING/ERROR from them outrank a watcher's
    /// Running/Thinking. The flip side is that a pattern WAITING must release
    /// as soon as the screen stops showing the dialog — otherwise a session
    /// whose watcher never reports another *change* parks on WAITING forever.
    ///
    /// Callers working from partial lines drop RUNNING before calling: there,
    /// "non-empty text" is too weak a signal to declare a session running.
    pub fn apply_pattern_state(&mut self, new_state: SessionState) {
        match new_state {
            SessionState::Waiting | SessionState::Error => {
                self.pattern_waiting = new_state == SessionState::Waiting;
                self.state = new_state;
            }
            // Ready/Thinking are positive evidence that the dialog is gone.
            // RUNNING is not: a repaint of an unrelated screen region also
            // reads as RUNNING, and clearing on that flickers the dialog away.
            SessionState::Ready | SessionState::Thinking if self.pattern_waiting => {
                self.pattern_waiting = false;
                self.state = self.watcher_state.clone().unwrap_or(new_state);
            }
            _ if !self.ipc_state => {
                self.pattern_waiting = false;
                self.state = new_state;
            }
            _ => {}
        }
    }

    /// Apply a state from an authoritative source (JSONL/db watcher or IPC).
    /// Returns false if the report was *suppressed* rather than applied.
    /// Running/Thinking are recorded
    /// but not shown while a pattern-detected dialog is up — the watcher cannot
    /// see the dialog, and the CLI is genuinely mid-turn *and* blocked on the
    /// user. Any other report (Ready/Error/Dead/an explicit Waiting) means the
    /// dialog is resolved or superseded.
    pub fn apply_watcher_state(&mut self, new_state: SessionState) -> bool {
        self.watcher_state = Some(new_state.clone());
        self.ipc_state = true;
        self.ipc_state_set_at = Some(Instant::now());
        if self.pattern_waiting
            && matches!(new_state, SessionState::Running | SessionState::Thinking)
        {
            return false;
        }
        self.pattern_waiting = false;
        self.state = new_state;
        true
    }

    /// State text for display and reporting: paused sessions read PAUSED
    /// regardless of the frozen underlying state.
    pub fn state_label(&self) -> &str {
        if self.paused && self.state != SessionState::Dead {
            "PAUSED"
        } else {
            self.state.label()
        }
    }

    /// Pause (SIGSTOP) or resume (SIGCONT) the session's process. Signals the
    /// child's process group so grandchildren (the CLI's own subprocesses)
    /// stop too; falls back to the child alone if group signalling fails.
    pub fn set_paused(&mut self, pause: bool) -> Result<(), String> {
        if self.state == SessionState::Dead {
            return Err("session is dead".to_string());
        }
        let Some(pid) = self.child_pid else {
            return Err("no process id known for this session".to_string());
        };
        if self.paused == pause {
            return Ok(());
        }
        let sig = if pause { libc::SIGSTOP } else { libc::SIGCONT };
        let ok = unsafe { libc::kill(-(pid as i32), sig) == 0 || libc::kill(pid as i32, sig) == 0 };
        if !ok {
            return Err(std::io::Error::last_os_error().to_string());
        }
        self.paused = pause;
        Ok(())
    }

    pub fn elapsed_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    pub fn elapsed_display(&self) -> String {
        let s = self.elapsed_secs();
        if s < 60 {
            format!("{}s", s)
        } else {
            format!("{}m{}s", s / 60, s % 60)
        }
    }

    pub fn cost_display(&self) -> String {
        if self.pro_sub {
            "Pro".to_string()
        } else if self.stats.total_cost_usd == 0.0 {
            "—".to_string()
        } else if self.kind == SessionKind::Codex {
            let credits = self.stats.total_cost_usd;
            if credits >= 1000.0 {
                format!("{:.1}kcr", credits / 1000.0)
            } else {
                format!("{:.1}cr", credits)
            }
        } else {
            format!("${:.3}", self.stats.total_cost_usd)
        }
    }

    pub fn context_display(&self) -> String {
        match (self.stats.context_tokens, self.context_max) {
            (0, 0) => "—".to_string(),
            (c, 0) => fmt_count(c),
            (c, m) => format!("{}/{}", fmt_count(c), fmt_count(m)),
        }
    }

    pub fn tokens_display(&self) -> String {
        let total = self.stats.input_tokens + self.stats.output_tokens;
        if total == 0 {
            "—".to_string()
        } else {
            fmt_count(total)
        }
    }

    /// Accumulate per-turn stats into the running session total.
    /// Called once per completed output line; each turn contributes its own
    /// cost and token counts additively.
    pub fn accumulate_stats(&mut self, new: TokenStats) {
        self.stats.input_tokens += new.input_tokens;
        self.stats.output_tokens += new.output_tokens;
        self.stats.total_cost_usd += new.total_cost_usd;
    }

    /// Replace stats with an externally reported cumulative total, but only if
    /// it advances what we already have — protects against stale or restarted
    /// watchers regressing the counters. Cost is the primary signal; tokens
    /// break the tie for zero-cost sessions (local models report $0 forever).
    pub fn apply_reported_total(&mut self, new: TokenStats) {
        let cur = &self.stats;
        let advances = new.total_cost_usd > cur.total_cost_usd
            || (new.total_cost_usd == cur.total_cost_usd
                && new.input_tokens + new.output_tokens >= cur.input_tokens + cur.output_tokens);
        if advances {
            self.stats = new;
        }
    }

    /// Note that input was sent to this session. Whatever dialog the screen was
    /// showing has now been answered, so a pattern-detected WAITING must stop
    /// outranking watcher reports — otherwise the session reads WAITING for the
    /// whole tool run that follows the approval.
    pub fn note_input_sent(&mut self) {
        self.pattern_waiting = false;
    }

    /// Non-blocking send to PTY; silently drops if channel is full or closed
    pub fn write_bytes(&self, data: Vec<u8>) {
        if let Some(tx) = &self.pty_writer {
            let _ = tx.try_send(data);
        }
    }

    pub fn try_write_bytes(&self, data: Vec<u8>) -> Result<(), mpsc::error::TrySendError<Vec<u8>>> {
        match &self.pty_writer {
            Some(tx) => tx.try_send(data),
            None => Err(mpsc::error::TrySendError::Closed(data)),
        }
    }
}

/// Reduce a rendered TUI screen to its meaningful content for programmatic
/// readers and the chat pane. Drops window chrome — box-drawing borders,
/// the empty input box, keyboard-hint footers — and collapses blank runs,
/// while keeping real content: text, code, diffs, and permission dialogs
/// (their "❯ 1. Yes" options carry text, so they survive).
pub fn clean_tui_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<String> {
    const BORDER: &str = "─│╭╮╰╯┌┐└┘├┤┬┴┼━┃═║╔╗╚╝╠╣▔▁";
    fn is_chrome(line: &str) -> bool {
        let t = line.trim();
        if t.is_empty() {
            return false; // blanks are separators, handled by the caller
        }
        // Border-only rows and the empty input prompt
        if t.chars().all(|c| BORDER.contains(c) || c.is_whitespace()) {
            return true;
        }
        if matches!(t, "❯" | ">" | "›") {
            return true;
        }
        // Keyboard-hint footers ("? for shortcuts", "esc to interrupt", …)
        let lower = t.to_ascii_lowercase();
        [
            "? for shortcuts",
            "esc to interrupt",
            "esc to cancel",
            "shift+tab to cycle",
            "ctrl+c to quit",
        ]
        .iter()
        .any(|hint| lower.contains(hint))
    }

    let mut out: Vec<String> = Vec::new();
    for line in lines {
        if is_chrome(line) {
            continue;
        }
        // Strip framing borders at the edges, keep inner content
        let stripped = line
            .trim_end()
            .trim_start_matches(['│', '┃', '║'])
            .trim_end_matches(['│', '┃', '║'])
            .trim_end();
        if stripped.trim().is_empty() {
            // Collapse blank runs to a single separator
            if out.last().is_some_and(|l| !l.is_empty()) {
                out.push(String::new());
            }
            continue;
        }
        out.push(stripped.to_string());
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

pub fn extract_waiting_prompt(lines: &VecDeque<String>) -> Option<String> {
    lines
        .iter()
        .rev()
        .map(|line| line.trim())
        .filter(|line| {
            !line.is_empty()
                && !line
                    .chars()
                    .all(|character| "─│╭╮╰╯>❯ ".contains(character))
        })
        .find(|line| {
            line.ends_with('?') || line.to_ascii_lowercase().contains("(y/n)") || line.len() > 10
        })
        .map(|line| line.chars().take(80).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Scrollback capture ────────────────────────────────────────────────

    fn tui(kind: SessionKind, rows: u16, cols: u16) -> Session {
        Session::new(0, "s".into(), kind, "/tmp".into(), rows, cols, 1000)
    }

    fn history(s: &Session) -> Vec<String> {
        s.scrollback_lines.iter().cloned().collect()
    }

    #[test]
    fn decstbm_params_map_to_zero_based_inclusive_rows() {
        assert_eq!(parse_decstbm(b"1;25", 30), Some((0, 24)));
        assert_eq!(parse_decstbm(b"8;30", 30), Some((7, 29)));
        // Empty params mean the whole screen.
        assert_eq!(parse_decstbm(b"", 30), Some((0, 29)));
        // Degenerate regions reset rather than invert.
        assert_eq!(parse_decstbm(b"10;10", 30), None);
        assert_eq!(parse_decstbm(b"20;5", 30), None);
        // A bottom past the screen clamps.
        assert_eq!(parse_decstbm(b"1;99", 30), Some((0, 29)));
    }

    #[test]
    fn scrolled_off_locates_the_new_top_row_in_the_old_ones() {
        let rows = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let prev = rows(&["a", "b", "c", "d"]);
        assert_eq!(scrolled_off(&prev, &rows(&["a", "b", "c", "d"])), 0);
        assert_eq!(scrolled_off(&prev, &rows(&["b", "c", "d", "e"])), 1);
        assert_eq!(scrolled_off(&prev, &rows(&["c", "d", "e", "f"])), 2);
        // A full repaint is not a scroll, and must not be mistaken for one.
        assert_eq!(scrolled_off(&prev, &rows(&["w", "x", "y", "z"])), 0);
        // The tail may already hold new content written by the same chunk —
        // the case exact suffix matching gets wrong.
        assert_eq!(scrolled_off(&prev, &rows(&["c", "d", "NEW", "NEWER"])), 2);
        // Blank rows identify nothing, so they never imply a scroll.
        let blanks = rows(&["", "", "", ""]);
        assert_eq!(scrolled_off(&blanks, &blanks), 0);
        // A merely repeated line is not a scroll to that line.
        assert_eq!(scrolled_off(&prev, &rows(&["b", "ZZ", "YY", "XX"])), 0);
    }

    /// The bug this exists for: codex never enters the alternate screen and
    /// scrolls inside a DECSTBM region, whose evicted lines vt100 discards by
    /// spec. Both of linkshell's old paths therefore produced nothing.
    #[test]
    fn codex_style_region_scrolling_is_captured_as_history() {
        let mut s = tui(SessionKind::Codex, 10, 40);
        // Reserve the bottom three rows for a composer, exactly as codex does.
        s.process_bytes(b"\x1b[1;7r", true);
        assert!(!s.screen.screen().alternate_screen(), "normal screen");

        // Fill the region, then scroll it well past its height.
        for i in 1..=20 {
            s.process_bytes(format!("\x1b[7;1H\r\n line {i}").as_bytes(), true);
        }

        assert_eq!(
            s.screen.screen().scrollback(),
            0,
            "vt100 keeps no scrollback for a restricted region — the premise"
        );
        let seen = history(&s);
        assert!(seen.len() >= 10, "captured {} lines: {seen:?}", seen.len());
        assert!(seen.iter().any(|l| l.contains("line 1")), "{seen:?}");
        assert!(seen.iter().any(|l| l.contains("line 9")), "{seen:?}");
        // In order, and without duplicates from the repeated repaints.
        let numbers: Vec<usize> = seen
            .iter()
            .filter_map(|l| l.trim().strip_prefix("line ")?.parse().ok())
            .collect();
        assert!(
            numbers.windows(2).all(|w| w[1] > w[0]),
            "out of order or duplicated: {numbers:?}"
        );
    }

    /// A chunk can scroll by more than one line — codex emits runs of
    /// newlines — and the old capture only ever kept the row that happened to
    /// pass through the top.
    #[test]
    fn a_multi_line_scroll_in_one_chunk_keeps_every_line() {
        let mut s = tui(SessionKind::Codex, 8, 40);
        s.process_bytes(b"\x1b[1;5r", true); // rows 0..4 scroll, 5..7 pinned
        s.process_bytes(
            b"\x1b[5;1Hone\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\n",
            true,
        );
        let seen = history(&s);
        for want in ["one", "two"] {
            assert!(
                seen.iter().any(|l| l.trim() == want),
                "lost {want}: {seen:?}"
            );
        }
    }

    /// The complement: with no region and no alternate screen, vt100 keeps
    /// the scrollback itself, and capturing too would double every line.
    #[test]
    fn a_full_screen_scroll_is_left_to_vt100() {
        let mut s = tui(SessionKind::Codex, 6, 40);
        s.process_bytes(
            b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\n",
            true,
        );
        // Screen::scrollback() is the current offset, not the stored length,
        // so ask whether vt100 will scroll back at all.
        s.screen.set_scrollback(2);
        assert_eq!(s.screen.screen().scrollback(), 2, "vt100 holds it");
        assert!(history(&s).is_empty(), "we do not: {:?}", history(&s));
    }

    /// Codex resets its region dozens of times a second, so a chunk routinely
    /// straddles the change. Diffing across it compares the transcript with
    /// the composer pinned below and reads as a scroll — which is what put
    /// banner and composer fragments into the history.
    #[test]
    fn a_region_change_mid_chunk_does_not_capture_the_pinned_rows() {
        let mut s = tui(SessionKind::Codex, 8, 40);
        s.process_bytes(b"\x1b[1;5r", true);
        // Draw a composer into the pinned rows, then scroll the region and
        // reset it in one chunk, exactly as codex frames its output.
        s.process_bytes(b"\x1b[7;1H> composer prompt\x1b[8;1Hmodel: gpt", true);
        s.process_bytes(b"\x1b[5;1Halpha\r\nbeta\r\n\x1b[r", true);
        s.process_bytes(b"\x1b[1;5r\x1b[5;1Hgamma\r\n\x1b[r", true);

        let seen = history(&s);
        for banned in ["composer", "model: gpt"] {
            assert!(
                !seen.iter().any(|l| l.contains(banned)),
                "captured a pinned row ({banned}): {seen:?}"
            );
        }
    }

    #[test]
    fn decstbm_segments_split_after_each_region_change() {
        let segs = decstbm_segments(b"aa\x1b[1;5rbb\x1b[rcc");
        let parts: Vec<&[u8]> = segs.iter().map(|(b, _)| *b).collect();
        assert_eq!(
            parts,
            vec![&b"aa\x1b[1;5r"[..], &b"bb\x1b[r"[..], &b"cc"[..]]
        );
        assert_eq!(segs[0].1.as_deref(), Some(&b"1;5"[..]));
        assert_eq!(segs[1].1.as_deref(), Some(&b""[..]));
        assert_eq!(segs[2].1, None);
        // No DECSTBM: one segment, unchanged.
        let segs = decstbm_segments(b"plain\x1b[2Jtext");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].0, b"plain\x1b[2Jtext");
    }

    #[test]
    fn may_scroll_skips_pure_repaints() {
        assert!(!may_scroll(b"\x1b[1;1H\x1b[Khello\x1b[2;1Hworld"));
        assert!(may_scroll(b"hello\r\n"));
        assert!(may_scroll(b"\x1b[2S"));
        assert!(may_scroll(b"\x1bD"));
    }

    /// The safety property that lets this run on the normal screen: a TUI
    /// redrawing in place must not be mistaken for one that scrolled.
    #[test]
    fn an_in_place_repaint_captures_nothing() {
        let mut s = tui(SessionKind::Codex, 6, 40);
        s.process_bytes(b"\x1b[1;1Halpha\x1b[2;1Hbeta", true);
        let before = history(&s).len();
        for _ in 0..20 {
            // Same content, rewritten — a spinner frame, say.
            s.process_bytes(b"\x1b[1;1H\x1b[Kalpha\x1b[2;1H\x1b[Kbeta", true);
        }
        assert_eq!(history(&s).len(), before, "{:?}", history(&s));
    }

    #[test]
    fn shells_are_left_to_vt100s_own_scrollback() {
        let mut s = tui(SessionKind::Shell, 4, 40);
        for i in 0..20 {
            s.process_bytes(format!("line {i}\r\n").as_bytes(), true);
        }
        assert!(
            s.screen.screen().scrollback() > 0 || !s.kind.captures_scrollback(),
            "a shell scrolls the real grid, so vt100 holds its history"
        );
        assert!(
            history(&s).is_empty(),
            "and we do not double-capture it: {:?}",
            history(&s)
        );
    }

    #[test]
    fn fmt_count_tiers_and_width() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1_000), "1.0k");
        assert_eq!(fmt_count(42_340), "42.3k");
        assert_eq!(fmt_count(999_900), "999.9k");
        // The 1000k boundary rolls over to M instead of "1000.0k".
        assert_eq!(fmt_count(999_950), "1.00M");
        assert_eq!(fmt_count(1_234_000), "1.23M");
        assert_eq!(fmt_count(12_340_000), "12.3M");
        assert_eq!(fmt_count(123_400_000), "123M");
        for n in [
            0,
            999,
            1_000,
            999_949,
            999_950,
            9_994_999,
            99_949_999,
            u32::MAX as u64,
        ] {
            assert!(
                fmt_count(n).chars().count() <= 6,
                "{} too wide",
                fmt_count(n)
            );
        }
    }

    #[test]
    fn waiting_prompt_prefers_recent_question_and_ignores_box_drawing() {
        let lines = VecDeque::from([
            "Earlier useful output".to_string(),
            "────────────".to_string(),
            "Should I update the tests too?".to_string(),
            "❯".to_string(),
        ]);
        assert_eq!(
            extract_waiting_prompt(&lines).as_deref(),
            Some("Should I update the tests too?")
        );
    }

    fn session(kind: SessionKind) -> Session {
        Session::new(
            7,
            "test".into(),
            kind,
            "/tmp".into(),
            PTY_ROWS,
            PTY_COLS,
            2000,
        )
    }

    #[test]
    fn session_kind_labels_match_status_labels() {
        assert_eq!(SessionKind::Claude.label(), "claude");
        assert_eq!(SessionKind::Codex.label(), "codex");
        assert_eq!(SessionKind::Shell.label(), "shell");
        assert_eq!(SessionKind::Custom("tool".into()).label(), "custom");
    }

    #[test]
    fn session_state_labels_are_stable_for_ui_and_ipc() {
        assert_eq!(SessionState::Starting.label(), "STARTING");
        assert_eq!(SessionState::Ready.label(), "READY");
        assert_eq!(SessionState::Thinking.label(), "THINKING");
        assert_eq!(SessionState::Running.label(), "RUNNING");
        assert_eq!(SessionState::Waiting.label(), "WAITING!");
        assert_eq!(SessionState::Error.label(), "ERROR");
        assert_eq!(SessionState::Dead.label(), "DEAD");
    }

    #[test]
    fn new_session_initializes_screen_and_metadata() {
        let s = session(SessionKind::Shell);

        assert_eq!(s.id, 7);
        assert_eq!(s.name, "test");
        assert_eq!(s.cwd, "/tmp");
        assert_eq!(s.state, SessionState::Starting);
        assert!(!s.headless);
        assert_eq!(s.screen.screen().size(), (PTY_ROWS, PTY_COLS));
    }

    #[test]
    fn read_tail_uses_line_history_on_the_normal_screen() {
        let mut s = session(SessionKind::Shell);
        for i in 0..10 {
            s.push_output_line(format!("line-{i}"));
        }
        assert_eq!(s.read_tail(3), vec!["line-7", "line-8", "line-9"]);
    }

    #[test]
    fn read_tail_renders_the_screen_for_alternate_screen_tuis() {
        let mut s = session(SessionKind::Claude);
        // TUI-style output: enter the alternate screen, then paint with
        // cursor positioning — no newline-terminated lines ever arrive.
        s.process_bytes(b"\x1b[?1049h\x1b[1;1Hfirst row\x1b[2;1Hsecond row", true);
        assert!(s.output_lines.is_empty());

        let tail = s.read_tail(50);
        assert_eq!(tail, vec!["first row", "second row"]);
        // Bounded by n, taking the last lines
        assert_eq!(s.read_tail(1), vec!["second row"]);
    }

    #[test]
    fn clean_tui_lines_drops_chrome_and_keeps_dialogs_and_content() {
        let screen = [
            "╭──────────────────────────────╮",
            "│ Do you want to run this command? │",
            "│                              │",
            "│ ❯ 1. Yes                     │",
            "│   2. No, tell Claude what to do │",
            "╰──────────────────────────────╯",
            "",
            "",
            "⏺ I'll update the parser now.",
            "",
            "❯",
            "  ? for shortcuts",
        ];
        let cleaned = clean_tui_lines(screen.iter().copied());
        assert_eq!(
            cleaned,
            vec![
                " Do you want to run this command?",
                " ❯ 1. Yes",
                "   2. No, tell Claude what to do",
                "",
                "⏺ I'll update the parser now.",
            ]
        );
    }

    #[test]
    fn process_bytes_updates_vt100_screen_and_tick_counter() {
        let mut s = session(SessionKind::Shell);

        s.process_bytes(b"hello", true);

        assert_eq!(s.bytes_since_last_tick, 5);
        assert_eq!(s.screen.screen().contents().trim(), "hello");
    }

    #[test]
    fn process_bytes_reports_screen_change_only_on_visible_difference() {
        let mut s = session(SessionKind::Shell);

        // First paint changes the (blank) screen.
        assert!(s.process_bytes(b"hello", true));
        // Re-emitting an identical frame (as full-screen TUIs do) is a no-op.
        assert!(!s.process_bytes(b"\x1b[1;1Hhello", true));
        // Actual new content changes the frame again.
        assert!(s.process_bytes(b" world", true));
    }

    #[test]
    fn process_bytes_skips_change_detection_when_asked() {
        let mut s = session(SessionKind::Shell);

        // Off-screen sessions never report a change (nothing could redraw)...
        assert!(!s.process_bytes(b"hello", false));
        // ...and the first check after becoming visible sees the difference.
        assert!(s.process_bytes(b"", true));
    }

    #[test]
    fn pattern_waiting_outranks_watcher_until_the_dialog_clears() {
        let mut s = session(SessionKind::OpenCode);

        // A watcher owns the ordinary states.
        assert!(s.apply_watcher_state(SessionState::Running));
        assert_eq!(s.state, SessionState::Running);

        // A dialog the watcher cannot see takes over...
        s.apply_pattern_state(SessionState::Waiting);
        assert_eq!(s.state, SessionState::Waiting);

        // ...and survives further mid-turn watcher reports, and RUNNING
        // repaints of unrelated screen regions.
        assert!(!s.apply_watcher_state(SessionState::Running));
        assert_eq!(s.state, SessionState::Waiting);
        s.apply_pattern_state(SessionState::Running);
        assert_eq!(s.state, SessionState::Waiting);

        // An idle prompt on screen releases it, restoring the watcher's view.
        s.apply_pattern_state(SessionState::Ready);
        assert_eq!(s.state, SessionState::Running);
        assert!(!s.pattern_waiting);
    }

    #[test]
    fn watcher_ready_releases_a_stale_pattern_waiting() {
        let mut s = session(SessionKind::OpenCode);
        s.apply_watcher_state(SessionState::Running);
        s.apply_pattern_state(SessionState::Waiting);

        // The turn finished, so the dialog is answered no matter what the
        // (repaint-noisy) screen still shows.
        assert!(s.apply_watcher_state(SessionState::Ready));
        assert_eq!(s.state, SessionState::Ready);
        assert!(!s.pattern_waiting);
    }

    #[test]
    fn pattern_states_apply_freely_without_a_watcher() {
        let mut s = session(SessionKind::Aider);

        s.apply_pattern_state(SessionState::Waiting);
        assert_eq!(s.state, SessionState::Waiting);
        // No watcher state to fall back on: a complete RUNNING line clears it.
        s.apply_pattern_state(SessionState::Running);
        assert_eq!(s.state, SessionState::Running);
    }

    #[test]
    fn push_output_line_keeps_bounded_scrollback() {
        let mut s = session(SessionKind::Shell);

        for i in 0..2005 {
            s.push_output_line(format!("line-{i}"));
        }

        assert_eq!(s.output_lines.len(), 2000);
        assert_eq!(s.output_lines.front().unwrap(), "line-5");
        assert_eq!(s.output_lines.back().unwrap(), "line-2004");
        assert!(s.last_output_at.is_some());
    }

    #[test]
    fn display_helpers_format_empty_small_and_large_values() {
        let mut shell = session(SessionKind::Shell);
        assert_eq!(shell.tokens_display(), "—");
        assert_eq!(shell.context_display(), "—");
        assert_eq!(shell.cost_display(), "—");

        shell.stats.input_tokens = 999;
        shell.stats.output_tokens = 1;
        shell.stats.context_tokens = 1250;
        shell.stats.total_cost_usd = 0.1234;
        assert_eq!(shell.tokens_display(), "1.0k");
        assert_eq!(shell.context_display(), "1.2k");
        assert_eq!(shell.cost_display(), "$0.123");

        shell.context_max = 32768;
        assert_eq!(shell.context_display(), "1.2k/32.8k");
        shell.stats.context_tokens = 0;
        assert_eq!(shell.context_display(), "0/32.8k");

        let mut codex = session(SessionKind::Codex);
        codex.stats.total_cost_usd = 1234.0;
        assert_eq!(codex.cost_display(), "1.2kcr");
        codex.pro_sub = true;
        assert_eq!(codex.cost_display(), "Pro");
    }

    #[test]
    fn stats_accumulate_sums_input_output_and_cost() {
        let mut s = session(SessionKind::Shell);

        s.accumulate_stats(TokenStats {
            input_tokens: 10,
            output_tokens: 20,
            total_cost_usd: 0.2,
            context_tokens: 0,
        });
        s.accumulate_stats(TokenStats {
            input_tokens: 5,
            output_tokens: 7,
            total_cost_usd: 0.1,
            context_tokens: 0,
        });
        assert_eq!(s.stats.input_tokens, 15);
        assert_eq!(s.stats.output_tokens, 27);
        assert!((s.stats.total_cost_usd - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn write_bytes_sends_without_waiting_when_channel_is_open() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut s = session(SessionKind::Shell);
        s.pty_writer = Some(tx);

        s.write_bytes(b"cmd\n".to_vec());

        assert_eq!(rx.try_recv().unwrap(), b"cmd\n".to_vec());
    }

    #[test]
    fn push_output_line_uses_configured_scrollback_cap() {
        let mut session = Session::new(
            1,
            "test".into(),
            SessionKind::Shell,
            ".".into(),
            PTY_ROWS,
            PTY_COLS,
            3,
        );

        for i in 0..5 {
            session.push_output_line(format!("line-{i}"));
        }

        let lines: Vec<_> = session.output_lines.iter().cloned().collect();
        assert_eq!(lines, vec!["line-2", "line-3", "line-4"]);
    }
    #[test]
    fn command_base_name_skips_leading_env_assignments() {
        assert_eq!(command_base_name("claude --continue"), Some("claude"));
        assert_eq!(
            command_base_name("CLAUDE_CONFIG_DIR=~/w claude --continue"),
            Some("claude")
        );
        assert_eq!(
            command_base_name("A=1 B=two /usr/local/bin/codex"),
            Some("codex")
        );
        // '=' inside an option is not an assignment prefix
        assert_eq!(command_base_name("mytool --opt=1"), Some("mytool"));
        assert_eq!(command_base_name(""), None);
        assert_eq!(command_base_name("ONLY=assignments HERE=1"), None);
    }

    #[test]
    fn command_env_assignment_reads_only_leading_prefix() {
        assert_eq!(
            command_env_assignment("CLAUDE_CONFIG_DIR=/x/y claude", "CLAUDE_CONFIG_DIR"),
            Some("/x/y".to_string())
        );
        assert_eq!(
            command_env_assignment("CODEX_HOME='/quoted/path' codex", "CODEX_HOME"),
            Some("/quoted/path".to_string())
        );
        // assignments after the command word do not count
        assert_eq!(
            command_env_assignment("claude CLAUDE_CONFIG_DIR=/x", "CLAUDE_CONFIG_DIR"),
            None
        );
        assert_eq!(command_env_assignment("claude", "CLAUDE_CONFIG_DIR"), None);
    }

    #[test]
    fn env_prefixed_custom_commands_classify_as_claude_or_codex() {
        let k = SessionKind::Custom("CLAUDE_CONFIG_DIR=~/work claude -c".into());
        assert!(k.is_claude_based());
        let k = SessionKind::Custom("CODEX_HOME=~/alt codex".into());
        assert!(k.is_codex_based());
        let k = SessionKind::Custom("FOO=1 bash".into());
        assert!(!k.is_claude_based() && !k.is_codex_based());
    }

    #[test]
    fn wrapper_basenames_classify_as_claude_or_codex() {
        assert!(SessionKind::Custom("claude-work --continue".into()).is_claude_based());
        assert!(SessionKind::Custom("~/bin/claude.sh".into()).is_claude_based());
        assert!(SessionKind::Custom("claude_glm".into()).is_claude_based());
        assert!(SessionKind::Custom("codex-alt".into()).is_codex_based());
        // similar-but-unrelated names stay unclassified
        assert!(!SessionKind::Custom("claudette".into()).is_claude_based());
        assert!(!SessionKind::Custom("codexplorer".into()).is_codex_based());
    }

    #[test]
    fn session_base_defaults_from_kind_and_command() {
        let mk = |kind: SessionKind| {
            Session::new(0, "t".into(), kind, "/tmp".into(), PTY_ROWS, PTY_COLS, 100).base
        };
        assert_eq!(mk(SessionKind::Claude), BaseKind::Claude);
        assert_eq!(mk(SessionKind::Codex), BaseKind::Codex);
        assert_eq!(mk(SessionKind::Shell), BaseKind::Other);
        assert_eq!(mk(SessionKind::OpenCode), BaseKind::LocalAgent);
        assert_eq!(mk(SessionKind::OhMyPi), BaseKind::LocalAgent);
        assert_eq!(mk(SessionKind::Aider), BaseKind::LocalAgent);
        assert_eq!(
            mk(SessionKind::Custom("CLAUDE_CONFIG_DIR=/x claude".into())),
            BaseKind::Claude
        );
        assert_eq!(
            mk(SessionKind::Custom("my-wrapper".into())),
            BaseKind::Other
        );
    }

    #[test]
    fn kind_from_index_covers_every_dropdown_entry_with_custom_last() {
        assert_eq!(SessionKind::COUNT, KIND_LABELS.len());
        assert_eq!(SessionKind::from_index(0, ""), SessionKind::Claude);
        assert_eq!(SessionKind::from_index(1, ""), SessionKind::Codex);
        assert_eq!(SessionKind::from_index(2, ""), SessionKind::OpenCode);
        assert_eq!(SessionKind::from_index(3, ""), SessionKind::OhMyPi);
        assert_eq!(SessionKind::from_index(4, ""), SessionKind::Aider);
        assert_eq!(SessionKind::from_index(5, ""), SessionKind::Shell);
        assert_eq!(
            SessionKind::from_index(SessionKind::COUNT - 1, "run me"),
            SessionKind::Custom("run me".into())
        );
    }

    #[test]
    fn kind_from_name_parses_profile_and_ctl_spellings() {
        assert_eq!(SessionKind::from_name("claude"), Some(SessionKind::Claude));
        assert_eq!(SessionKind::from_name("codex"), Some(SessionKind::Codex));
        assert_eq!(
            SessionKind::from_name("opencode"),
            Some(SessionKind::OpenCode)
        );
        for spelling in ["oh-my-pi", "ohmypi", "omp"] {
            assert_eq!(SessionKind::from_name(spelling), Some(SessionKind::OhMyPi));
        }
        assert_eq!(SessionKind::from_name("aider"), Some(SessionKind::Aider));
        assert_eq!(SessionKind::from_name("shell"), Some(SessionKind::Shell));
        assert_eq!(SessionKind::from_name("bogus"), None);
    }

    #[test]
    fn kind_labels_fit_the_status_panel_column() {
        for kind in [
            SessionKind::Claude,
            SessionKind::Codex,
            SessionKind::OpenCode,
            SessionKind::OhMyPi,
            SessionKind::Aider,
            SessionKind::Shell,
            SessionKind::Custom(String::new()),
        ] {
            assert!(
                kind.label().len() <= 8,
                "label '{}' overflows the 8-char Kind column",
                kind.label()
            );
        }
    }

    #[test]
    fn alt_screen_scrolled_lines_are_captured_as_scrollback() {
        let mut s = Session::new(
            1,
            "codex".into(),
            SessionKind::Codex,
            "/tmp".into(),
            5,
            80,
            100,
        );
        s.process_bytes(
            b"\x1b[?1049h\x1b[HLine A\r\nLine B\r\nLine C\r\nLine D\r\nLine E",
            true,
        );
        assert!(s.scrollback_lines.is_empty());

        // Scroll up by 1: Line A leaves the top, everything shifts up
        s.process_bytes(b"\x1b[1S", true);

        assert_eq!(s.scrollback_lines.len(), 1);
        assert_eq!(s.scrollback_lines.front().unwrap(), "Line A");

        // Scroll up by 1 more: Line B leaves the top
        s.process_bytes(b"\x1b[1S", true);

        assert_eq!(s.scrollback_lines.len(), 2);
        assert_eq!(s.scrollback_lines.get(1).unwrap(), "Line B");
    }

    #[test]
    fn alt_screen_scrollback_deduplicates_identical_repaint() {
        let mut s = Session::new(
            0,
            "codex".into(),
            SessionKind::Codex,
            "/tmp".into(),
            3,
            80,
            100,
        );
        s.process_bytes(b"\x1b[?1049h\x1b[HHeader\r\nBody 1\r\nFooter", true);

        // Repaint with same top line (TUI re-rendering) — should NOT add to output_lines
        s.process_bytes(b"\x1b[HHeader\r\nBody 1\r\nFooter", true);
        assert!(s.output_lines.is_empty());
    }

    #[test]
    fn shell_session_no_alt_scrollback() {
        let mut s = Session::new(
            0,
            "shell".into(),
            SessionKind::Shell,
            "/tmp".into(),
            3,
            80,
            100,
        );
        s.process_bytes(b"hello world\n", true);
        assert!(s.output_lines.is_empty());
    }
}
