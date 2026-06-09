use crossterm::event::KeyEvent;
use tokio::sync::mpsc;

pub enum AppEvent {
    Key(KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    /// Raw PTY bytes — fed into the vt100 screen buffer for display
    SessionBytes {
        session_id: usize,
        data: Vec<u8>,
    },
    /// Complete line — used only for state inference / token parsing
    SessionOutput {
        session_id: usize,
        line: String,
    },
    /// Partial line — used only for state inference
    SessionCurrentLine {
        session_id: usize,
        text: String,
    },
    /// PTY write channel for a newly spawned session
    SessionWriter {
        session_id: usize,
        writer_tx: mpsc::Sender<Vec<u8>>,
    },
    /// PTY resize channel for a newly spawned session
    SessionResizer {
        session_id: usize,
        resizer_tx: mpsc::Sender<(u16, u16)>,
    },
    SessionDied {
        session_id: usize,
    },
    /// Authoritative cumulative token stats read from ~/.claude project JSONL
    SessionStats {
        session_id: usize,
        stats: crate::session::TokenStats,
    },
    /// Billing type detected from service_tier in JSONL; emitted once per session
    SessionBillingKnown {
        session_id: usize,
        is_pro: bool,
    },
    /// Content forwarded from a source session's pipe to a destination session's PTY
    PipeRelay {
        dest_id: usize,
        message: String,
    },
    /// State override injected by an external orchestrator via the IPC socket
    IpcStateOverride {
        session_id: usize,
        state: crate::session::SessionState,
    },
    /// Token/cost update injected by an external orchestrator via the IPC socket
    IpcTokenUpdate {
        session_id: usize,
        stats: crate::session::TokenStats,
    },
    /// IPC request/response — caller awaits a reply on response_tx
    IpcQuery {
        payload: IpcQueryPayload,
        response_tx: tokio::sync::oneshot::Sender<serde_json::Value>,
    },
    /// Persistent agent connected; app stores the write channel keyed by session_id
    IpcAgentConnected {
        session_id: usize,
        agent_tx: mpsc::Sender<String>,
    },
    /// Agent socket closed; app removes the write channel and marks session Dead
    IpcAgentDisconnected {
        session_id: usize,
    },
    /// Push a JSON message to a connected agent
    IpcSend {
        session_id: usize,
        message: serde_json::Value,
    },
    /// Fire manual pipes from source; dest=None fires all manual pipes from source
    IpcFirePipe {
        source: usize,
        dest: Option<usize>,
    },
    /// Agent sent a chat message via IPC.
    ChatInbound {
        from_session_id: usize,
        text: String,
    },
    /// User submitted a line from the chat input box.
    ChatOutbound {
        text: String,
    },
    /// Fire all manual pipes from every session in a named group
    IpcGroupFire {
        source_group: String,
    },
    /// Broadcast a raw JSON message to all agent_writers in the named group
    IpcBroadcast {
        group: String,
        msg: serde_json::Value,
    },
    /// IPC message addressed by session name instead of numeric id
    IpcNamedAction {
        session_name: String,
        msg: serde_json::Value,
    },
    /// Add a pipe declared via IPC
    IpcPipeAdd {
        source: usize,
        dest: usize,
        trigger: String,
        extract: String,
        prefix: Option<String>,
    },
    /// Remove pipe(s) via IPC
    IpcPipeRemove {
        source: usize,
        dest: Option<usize>,
    },
    Tick,
}

#[derive(Debug)]
pub enum IpcQueryPayload {
    SessionCreate {
        kind_str: String,
        name: String,
        cwd: String,
    },
    SessionInputWait {
        session_id: usize,
        text: String,
    },
    Register {
        name: String,
        group: Option<String>,
    },
    /// Synchronous snapshot query: "sessions" or "pipes"
    Query {
        what: String,
    },
}
