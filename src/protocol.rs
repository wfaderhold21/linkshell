use crate::auth::Capability;
use crate::session::SessionState;

pub const PROTOCOL_VERSION: u32 = 1;

/// Wire envelope: nested JSON format.
/// Wire format: {"id": 7, "msg": {"type": "state", "state": "READY"}}
/// The `id` field goes at the outer level; `msg` is a nested object.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Envelope {
    /// Present on requests that want a reply; echoed on the matching response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub msg: Message,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    // ── handshake ──
    Hello {
        protocol: u32,
        #[serde(default)]
        token: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        group: Option<String>,
    },
    Welcome {
        protocol: u32,
        session_id: Option<usize>,
        capabilities: Vec<Capability>,
        server: String,
    },

    // ── agent → linkshell ──
    State {
        state: SessionState,
        #[serde(default)]
        detail: Option<String>,
        #[serde(default)]
        session_id: Option<usize>,
        #[serde(default)]
        session_name: Option<String>,
    },
    Tokens {
        input: u64,
        output: u64,
        cost: f64,
        #[serde(default)]
        session_id: Option<usize>,
        #[serde(default)]
        session_name: Option<String>,
    },
    Output {
        line: String,
        #[serde(default)]
        session_id: Option<usize>,
        #[serde(default)]
        session_name: Option<String>,
    },
    AgentSend {
        dest: String,
        message: String,
        #[serde(default)]
        wait: bool,
    },
    FirePipe {
        #[serde(default)]
        source: Option<usize>,
        #[serde(default)]
        dest: Option<usize>,
        #[serde(default)]
        source_group: Option<String>,
    },
    Broadcast {
        group: String,
        message: serde_json::Value,
    },
    PipeAdd {
        source: usize,
        dest: usize,
        trigger: String,
        extract: String,
        #[serde(default)]
        prefix: Option<String>,
    },
    PipeRemove {
        source: usize,
        #[serde(default)]
        dest: Option<usize>,
    },
    SessionCreate {
        kind: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        cwd: Option<String>,
    },
    Query {
        what: String,
    },

    // ── linkshell → agent ──
    Relay {
        content: String,
    },
    Reply {
        ok: bool,
        #[serde(flatten)]
        data: serde_json::Value,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadProtocol,
    Unauthenticated,
    Forbidden,
    UnknownSession,
    BadRequest,
    Timeout,
    Oversize,
}

pub fn required_cap(m: &Message) -> Option<Capability> {
    use Capability::*;
    Some(match m {
        Message::State { .. } | Message::Tokens { .. } | Message::Output { .. } => SignalState,
        Message::Query { .. } => Query,
        Message::AgentSend { .. } => AgentSend,
        Message::FirePipe { .. } => FirePipe,
        Message::PipeAdd { .. } | Message::PipeRemove { .. } => ManagePipes,
        Message::Broadcast { .. } => Broadcast,
        Message::SessionCreate { .. } => CreateSession,
        // handshake + server-origin messages need no capability
        _ => return None,
    })
}
