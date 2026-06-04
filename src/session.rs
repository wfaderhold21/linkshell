use std::collections::VecDeque;
use std::time::Instant;
use tokio::sync::mpsc;

pub const MAX_SESSIONS: usize = 8;
pub const RING_BUFFER_LINES: usize = 1000;

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
            SessionKind::Claude    => "claude",
            SessionKind::Codex     => "codex",
            SessionKind::Shell     => "shell",
            SessionKind::Custom(_) => "custom",
        }
    }

    pub fn command(&self) -> String {
        match self {
            SessionKind::Claude    => "claude".to_string(),
            SessionKind::Codex     => "codex".to_string(),
            SessionKind::Shell     => {
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
            SessionState::Ready    => "READY",
            SessionState::Thinking => "THINKING",
            SessionState::Running  => "RUNNING",
            SessionState::Waiting  => "WAITING!",
            SessionState::Error    => "ERROR",
            SessionState::Dead     => "DEAD",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TokenStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_cost_usd: f64,
}

pub struct Session {
    pub id: usize,
    pub name: String,
    pub kind: SessionKind,
    pub state: SessionState,
    pub output: VecDeque<String>,
    pub stats: TokenStats,
    pub started_at: Instant,
    pub last_output_at: Option<Instant>,
    pub cwd: String,
    /// Send bytes to the PTY writer task
    pub pty_writer: Option<mpsc::Sender<Vec<u8>>>,
}

impl Session {
    pub fn new(id: usize, name: String, kind: SessionKind, cwd: String) -> Self {
        Self {
            id,
            name,
            kind,
            state: SessionState::Starting,
            output: VecDeque::with_capacity(RING_BUFFER_LINES),
            stats: TokenStats::default(),
            started_at: Instant::now(),
            last_output_at: None,
            cwd,
            pty_writer: None,
        }
    }

    pub fn push_line(&mut self, line: String) {
        self.last_output_at = Some(Instant::now());
        if self.output.len() >= RING_BUFFER_LINES {
            self.output.pop_front();
        }
        self.output.push_back(line);
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
        if self.stats.total_cost_usd == 0.0 {
            "—".to_string()
        } else {
            format!("${:.3}", self.stats.total_cost_usd)
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

    /// Non-blocking send to PTY; silently drops if channel is full or closed
    pub fn write_bytes(&self, data: Vec<u8>) {
        if let Some(tx) = &self.pty_writer {
            let _ = tx.try_send(data);
        }
    }
}
