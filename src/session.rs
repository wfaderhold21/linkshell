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

    pub fn command(&self) -> String {
        match self {
            SessionKind::Claude => "claude".to_string(),
            SessionKind::Codex => "codex".to_string(),
            SessionKind::Shell => {
                std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string())
            }
            SessionKind::Custom(c) => c.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
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
    pub last_output_at: Option<Instant>,
    pub cwd: String,
    /// Send bytes to the PTY writer task
    pub pty_writer: Option<mpsc::Sender<Vec<u8>>>,
    /// Send resize events to the PTY writer task
    pub pty_resizer: Option<mpsc::Sender<(u16, u16)>>,
    /// Scrollback of stripped output lines for pipe extraction
    pub output_lines: VecDeque<String>,
    /// Raw bytes received since the last tick — used to detect active generation
    /// without relying on newlines (Claude Code streams via cursor movement, not \n).
    pub bytes_since_last_tick: usize,
}

impl Session {
    pub fn new(
        id: usize,
        name: String,
        kind: SessionKind,
        cwd: String,
        rows: u16,
        cols: u16,
    ) -> Self {
        Self {
            id,
            name,
            kind,
            state: SessionState::Starting,
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
            bytes_since_last_tick: 0,
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
        if self.output_lines.len() > 2000 {
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

    /// Replace stats with an authoritative total (e.g. from /cost).
    /// Only replaces when the reported value exceeds what we have accumulated,
    /// so it acts as a correction when our running total drifted.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(kind: SessionKind) -> Session {
        Session::new(7, "test".into(), kind, "/tmp".into(), PTY_ROWS, PTY_COLS)
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
    fn stats_accumulate_and_reported_totals_only_replace_when_larger() {
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

        s.apply_reported_total(TokenStats {
            input_tokens: 1,
            output_tokens: 1,
            total_cost_usd: 0.1,
            context_tokens: 1,
        });
        assert_eq!(s.stats.input_tokens, 15);

        s.apply_reported_total(TokenStats {
            input_tokens: 100,
            output_tokens: 200,
            total_cost_usd: 1.0,
            context_tokens: 300,
        });
        assert_eq!(s.stats.input_tokens, 100);
        assert_eq!(s.stats.context_tokens, 300);
    }

    #[test]
    fn write_bytes_sends_without_waiting_when_channel_is_open() {
        let (tx, mut rx) = mpsc::channel(1);
        let mut s = session(SessionKind::Shell);
        s.pty_writer = Some(tx);

        s.write_bytes(b"cmd\n".to_vec());

        assert_eq!(rx.try_recv().unwrap(), b"cmd\n".to_vec());
    }
}
