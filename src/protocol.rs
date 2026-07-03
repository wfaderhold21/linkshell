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
    /// Write `text` into a session's PTY and block until it returns to READY.
    /// Sent by `linkshell-ctl wait-ready` (with empty text) and by orchestrators
    /// that want synchronous prompt→response semantics.
    SessionInputWait {
        session_id: usize,
        #[serde(default)]
        text: String,
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
        Message::SessionInputWait { .. } => InjectInput,
        // handshake + server-origin messages need no capability
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trips_the_documented_wire_format() {
        let wire = r#"{"id":7,"msg":{"type":"state","state":"READY"}}"#;
        let env: Envelope = serde_json::from_str(wire).unwrap();
        assert_eq!(env.id, Some(7));
        assert!(matches!(
            env.msg,
            Message::State { state: SessionState::Ready, .. }
        ));
        // id is omitted from fire-and-forget messages when serialized
        let out = serde_json::to_string(&Envelope {
            id: None,
            msg: Message::Query { what: "sessions".into() },
        })
        .unwrap();
        assert!(!out.contains("\"id\""));
        assert!(out.contains("\"type\":\"query\""));
    }

    #[test]
    fn hello_accepts_optional_fields_and_session_input_wait_defaults_text() {
        let hello: Envelope =
            serde_json::from_str(r#"{"msg":{"type":"hello","protocol":1}}"#).unwrap();
        assert!(matches!(
            hello.msg,
            Message::Hello { protocol: 1, token: None, name: None, group: None }
        ));

        let wait: Envelope = serde_json::from_str(
            r#"{"id":1,"msg":{"type":"session_input_wait","session_id":3}}"#,
        )
        .unwrap();
        match wait.msg {
            Message::SessionInputWait { session_id, text } => {
                assert_eq!(session_id, 3);
                assert!(text.is_empty());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn required_cap_gates_every_privileged_message() {
        use crate::auth::Capability::*;
        let cases: Vec<(Message, Option<crate::auth::Capability>)> = vec![
            (Message::Query { what: "sessions".into() }, Some(Query)),
            (
                Message::SessionInputWait { session_id: 0, text: String::new() },
                Some(InjectInput),
            ),
            (
                Message::SessionCreate { kind: "shell".into(), name: None, cwd: None },
                Some(CreateSession),
            ),
            (
                Message::Hello { protocol: 1, token: None, name: None, group: None },
                None,
            ),
            (Message::Relay { content: String::new() }, None),
        ];
        for (msg, expected) in cases {
            assert_eq!(required_cap(&msg), expected);
        }
    }

    #[test]
    fn worker_caps_cannot_inject_input_or_create_sessions() {
        let worker = crate::auth::worker_caps();
        assert!(!worker.contains(&crate::auth::Capability::InjectInput));
        assert!(!worker.contains(&crate::auth::Capability::CreateSession));
        assert!(worker.contains(&crate::auth::Capability::SignalState));

        let council = crate::auth::council_caps();
        assert_eq!(council.len(), 1);
        assert!(council.contains(&crate::auth::Capability::SignalState));

        let op = crate::auth::operator_caps();
        assert!(worker.is_subset(&op));
        assert!(council.is_subset(&worker));
    }

    #[test]
    fn minted_tokens_are_32_hex_chars_and_unique() {
        let a = crate::auth::mint_token();
        let b = crate::auth::mint_token();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
