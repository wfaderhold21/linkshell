use crossterm::event::KeyEvent;
use tokio::sync::mpsc;

pub enum AppEvent {
    Key(KeyEvent),
    SessionOutput  { session_id: usize, line: String },
    /// PTY write channel for a newly spawned session
    SessionWriter  { session_id: usize, writer_tx: mpsc::Sender<Vec<u8>> },
    SessionDied    { session_id: usize },
    Tick,
}
