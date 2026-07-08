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
    Shell,
    Custom(String),
}

impl SessionKind {
    pub fn label(&self) -> &str {
        match self {
            SessionKind::Claude => "claude",
            SessionKind::Codex => "codex",
            SessionKind::Shell => "shell",
            SessionKind::Custom(_) => "custom",
        }
    }

    pub fn is_claude_based(&self) -> bool {
        matches!(self, SessionKind::Claude) || self.custom_base_name() == Some("claude")
    }

    pub fn is_codex_based(&self) -> bool {
        matches!(self, SessionKind::Codex) || self.custom_base_name() == Some("codex")
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

#[derive(Debug, Clone, Default)]
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
    /// When true, state was set by IPC and should not be auto-reverted by the tick timeout.
    /// Cleared when pattern matching updates the state from PTY output.
    pub ipc_state: bool,
    /// When ipc_state was last set; used to expire stale overrides in handle_tick.
    pub ipc_state_set_at: Option<Instant>,
    /// Agent group for broadcast addressing and group-triggered pipes.
    pub group: Option<String>,
    /// vt100 screen buffer — updated with raw PTY bytes, used for display
    pub screen: vt100::Parser,
    pub stats: TokenStats,
    pub pro_sub: bool,
    pub started_at: Instant,
    #[allow(dead_code)] // only read in tests; kept for debugging and future use
    pub cwd: String,
    /// Send bytes to the PTY writer task
    pub pty_writer: Option<mpsc::Sender<Vec<u8>>>,
    /// Send resize events to the PTY writer task
    pub pty_resizer: Option<mpsc::Sender<(u16, u16)>>,
    /// Scrollback of stripped output lines for pipe extraction
    pub output_lines: VecDeque<String>,
    pub scroll_buffer_lines: usize,
    /// Raw bytes received since the last tick — used to detect active generation
    /// without relying on newlines (Claude Code streams via cursor movement, not \n).
    pub bytes_since_last_tick: usize,
    /// Scroll offset (in lines) into `output_lines` history. Used when the
    /// application occupies the alternate screen (full-screen TUIs), where
    /// vt100 keeps no scrollback; unifies scrolling across session types.
    pub history_scroll: usize,
    /// Resolved CLI identity: which JSONL watcher / stats pipeline applies.
    /// Defaults from the command's base name; spawn_session refines it with
    /// the config alias table.
    pub base: BaseKind,
    /// Last time we received output from this session; used for idle timeout detection.
    pub last_output_at: Option<Instant>,
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
        } else if kind
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
            ipc_state: false,
            ipc_state_set_at: None,
            group: None,
            screen: vt100::Parser::new(rows, cols, 1000),
            stats: TokenStats::default(),
            pro_sub: false,
            started_at: Instant::now(),
            last_output_at: None,
            cwd,
            pty_writer: None,
            pty_resizer: None,
            output_lines: VecDeque::new(),
            scroll_buffer_lines,
            bytes_since_last_tick: 0,
            history_scroll: 0,
            base,
        }
    }

    pub fn process_bytes(&mut self, data: &[u8]) {
        self.bytes_since_last_tick += data.len();
        self.screen.process(data);
    }

    pub fn resize_screen(&mut self, rows: u16, cols: u16) {
        self.screen.set_size(rows, cols);
    }

    pub fn push_output_line(&mut self, line: String) {
        self.last_output_at = Some(Instant::now());
        self.output_lines.push_back(line);
        if self.output_lines.len() > self.scroll_buffer_lines {
            self.output_lines.pop_front();
        }
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
        let ctx = self.stats.context_tokens;
        if ctx == 0 {
            return "—".to_string();
        }
        if ctx >= 1000 {
            format!("{:.1}k ctx", ctx as f64 / 1000.0)
        } else {
            format!("{} ctx", ctx)
        }
    }

    pub fn tokens_display(&self) -> String {
        let total = self.stats.input_tokens + self.stats.output_tokens;
        if total == 0 {
            "—".to_string()
        } else if total >= 1000 {
            format!("{:.1}k tok", total as f64 / 1000.0)
        } else {
            format!("{} tok", total)
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
    /// the reported cost is higher than what we already have — protects against
    /// stale or restarted watchers regressing the counters.
    pub fn apply_reported_total(&mut self, new: TokenStats) {
        if new.total_cost_usd > self.stats.total_cost_usd {
            self.stats = new;
        }
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
    fn process_bytes_updates_vt100_screen_and_tick_counter() {
        let mut s = session(SessionKind::Shell);

        s.process_bytes(b"hello");

        assert_eq!(s.bytes_since_last_tick, 5);
        assert_eq!(s.screen.screen().contents().trim(), "hello");
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
        assert_eq!(shell.tokens_display(), "1.0k tok");
        assert_eq!(shell.context_display(), "1.2k ctx");
        assert_eq!(shell.cost_display(), "$0.123");

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
    fn session_base_defaults_from_kind_and_command() {
        let mk = |kind: SessionKind| {
            Session::new(0, "t".into(), kind, "/tmp".into(), PTY_ROWS, PTY_COLS, 100).base
        };
        assert_eq!(mk(SessionKind::Claude), BaseKind::Claude);
        assert_eq!(mk(SessionKind::Codex), BaseKind::Codex);
        assert_eq!(mk(SessionKind::Shell), BaseKind::Other);
        assert_eq!(
            mk(SessionKind::Custom("CLAUDE_CONFIG_DIR=/x claude".into())),
            BaseKind::Claude
        );
        assert_eq!(
            mk(SessionKind::Custom("my-wrapper".into())),
            BaseKind::Other
        );
    }
}
