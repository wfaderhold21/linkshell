use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    SignalState,   // state / tokens / output for own session
    Query,         // query sessions, pipes
    AgentSend,     // direct message another agent
    FirePipe,      // fire a manual pipe
    ManagePipes,   // pipe_add / pipe_remove
    Broadcast,     // broadcast to a group
    CreateSession, // session_create  (privileged)
    InjectInput,   // session_input_wait / write into a PTY (privileged)
}

pub type CapSet = HashSet<Capability>;

/// Full operator: the human at the TUI, or a same-uid Unix peer.
pub fn operator_caps() -> CapSet {
    use Capability::*;
    [
        SignalState,
        Query,
        AgentSend,
        FirePipe,
        ManagePipes,
        Broadcast,
        CreateSession,
        InjectInput,
    ]
    .into_iter()
    .collect()
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

pub fn mint_token() -> String {
    let mut buf = [0u8; 16];
    // /dev/urandom keeps the dep surface at zero; swap for `getrandom` if preferred.
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .ok();
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}
