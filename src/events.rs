use crossterm::event::KeyEvent;
use tokio::sync::mpsc;

pub enum AppEvent {
    Key(KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    /// Raw PTY bytes — fed into the vt100 screen buffer for display
    SessionBytes      { session_id: usize, data: Vec<u8> },
    /// Complete line — used only for state inference / token parsing
    SessionOutput     { session_id: usize, line: String },
    /// Partial line — used only for state inference
    SessionCurrentLine { session_id: usize, text: String },
    /// PTY write channel for a newly spawned session
    SessionWriter     { session_id: usize, writer_tx: mpsc::Sender<Vec<u8>> },
    SessionDied       { session_id: usize },
    /// Authoritative cumulative token stats read from ~/.claude project JSONL
    SessionStats      { session_id: usize, stats: crate::session::TokenStats },
    Tick,
}
