use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    SignalState,   // state / tokens / output for own session
    SignalAny,     // state / tokens / output for arbitrary sessions (privileged)
    Query,         // query sessions, pipes
    AgentSend,     // direct message another agent
    FirePipe,      // fire a manual pipe
    ManagePipes,   // pipe_add / pipe_remove
    Broadcast,     // broadcast to a group
    CreateSession, // session_create  (privileged)
    InjectInput,   // session_input_wait / write into a PTY (privileged)
    RequestKill,   // session_kill_request — asks the user; never kills directly
    PostChat,      // chat_post — write a line into the chat pane
}

pub type CapSet = HashSet<Capability>;

/// Full operator: the human at the TUI, or a same-uid Unix peer.
pub fn operator_caps() -> CapSet {
    use Capability::*;
    [
        SignalState,
        SignalAny,
        Query,
        AgentSend,
        FirePipe,
        ManagePipes,
        Broadcast,
        CreateSession,
        InjectInput,
        RequestKill,
        PostChat,
    ]
    .into_iter()
    .collect()
}

/// The resident orchestrator agent: everything the operator can do. Actual
/// session kills still require human confirmation in the TUI — RequestKill
/// only files a request.
pub fn orchestrator_caps() -> CapSet {
    operator_caps()
}

/// Default for a spawned worker agent — can report and talk, cannot escalate.
pub fn worker_caps() -> CapSet {
    use Capability::*;
    [SignalState, Query, AgentSend, FirePipe]
        .into_iter()
        .collect()
}

/// Council members are driven externally by linkshell; they only report state.
#[allow(dead_code)] // only called from launch_council, which has no callers in the binary yet
pub fn council_caps() -> CapSet {
    [Capability::SignalState].into_iter().collect()
}

/// Mint a 128-bit capability token, hex-encoded.
///
/// Fails closed: if the system CSPRNG is unavailable the error is propagated
/// rather than swallowed. Swallowing it (the previous `.ok()`) left `buf` at
/// its zero initializer, so every token became 32 zeros — a fully predictable
/// credential. That is reachable in practice: a container or bubblewrap
/// sandbox with no `/dev` bound, a seccomp filter, or fd exhaustion all make
/// the open/read fail while the process otherwise runs fine.
pub fn mint_token() -> std::io::Result<String> {
    let mut buf = [0u8; 16];
    // /dev/urandom keeps the dep surface at zero; swap for `getrandom` if preferred.
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    // Defence in depth: a short read can't get here (read_exact errors), but an
    // all-zero buffer would be indistinguishable from the old bug, so reject it.
    if buf.iter().all(|&b| b == 0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "CSPRNG returned all zero bytes",
        ));
    }
    Ok(buf.iter().map(|b| format!("{:02x}", b)).collect())
}
