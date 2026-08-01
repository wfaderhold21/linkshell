//! The resident orchestrator agent (Class A — API providers).
//!
//! A background task owns the conversation history and a tool-use loop
//! against the configured LLM API. Tools are executed by sending
//! `AppEvent::OrchestratorRequest` to the main loop and awaiting the reply;
//! final answers surface in the chat pane as `AppEvent::ChatReply`.
//!
//! CLI-class providers (claude/codex/opencode/omp) don't use this task: they
//! run as a regular linkshell session with operator capabilities and drive
//! linkshell through `linkshell-ctl` (see `CLI_BRIEFING`).

mod anthropic;
mod openai;
mod skills;

use crate::config::{ApiProvider, OrchestratorClass, OrchestratorConfig};
use crate::events::{AppEvent, OrchestratorReq};
use tokio::sync::mpsc;

pub struct OrchestratorHandle {
    pub tx: mpsc::Sender<OrchestratorMsg>,
    pub name: String,
    /// Generation counter bumped by /interrupt; the turn loop snapshots it
    /// at turn start and breaks at the next safe point once it changes.
    interrupt_tx: tokio::sync::watch::Sender<u64>,
}

impl OrchestratorHandle {
    /// Build a handle around an existing channel (tests); the interrupt
    /// signal goes nowhere.
    #[cfg(test)]
    pub fn detached(tx: mpsc::Sender<OrchestratorMsg>, name: String) -> Self {
        let (interrupt_tx, _) = tokio::sync::watch::channel(0u64);
        Self {
            tx,
            name,
            interrupt_tx,
        }
    }

    /// Ask the current turn to stop at its next safe point (between tool
    /// iterations, or immediately if it's blocked inside a tool call).
    pub fn interrupt(&self) {
        self.interrupt_tx.send_modify(|g| *g += 1);
    }
}

/// Per-turn view of the interrupt counter. Checked only at points where
/// breaking leaves the conversation history API-valid (every tool_use
/// answered by a tool_result).
pub struct Interrupt {
    rx: tokio::sync::watch::Receiver<u64>,
    start: u64,
}

impl Interrupt {
    fn hit(&self) -> bool {
        *self.rx.borrow() != self.start
    }

    /// Resolves when the user interrupts; pends forever otherwise.
    async fn wait(&mut self) {
        let start = self.start;
        if self.rx.wait_for(|g| *g != start).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Cross-turn repeat-call suppressor.
///
/// `max_tool_iterations` bounds a single turn; nothing bounds the *sequence*
/// of turns, so a flapping session can drive the model through the same tool
/// call indefinitely. The log remembers recent (name, args) pairs and turns
/// an exact repeat into a tool result that says so — which is information the
/// model can act on, rather than a silent loop.
pub(crate) struct CallLog {
    seen: Vec<(u64, std::time::Instant)>,
    window: std::time::Duration,
}

impl CallLog {
    pub(crate) fn new(window_secs: u64) -> Self {
        Self {
            seen: Vec::new(),
            window: std::time::Duration::from_secs(window_secs),
        }
    }

    /// Drop the history (used by /reset, so a fresh context is not haunted by
    /// the calls of the previous one).
    pub(crate) fn clear(&mut self) {
        self.seen.clear();
    }

    fn key(name: &str, args: &serde_json::Value) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut h);
        // Serialized form: serde_json preserves insertion order, and both
        // providers hand us arguments the model just generated, so identical
        // calls serialize identically in practice.
        args.to_string().hash(&mut h);
        h.finish()
    }

    /// Record a call. Returns Some(age) when the identical call was already
    /// made inside the window.
    fn check(&mut self, name: &str, args: &serde_json::Value) -> Option<std::time::Duration> {
        if self.window.is_zero() {
            return None;
        }
        let now = std::time::Instant::now();
        self.seen
            .retain(|(_, t)| now.duration_since(*t) < self.window);
        let key = Self::key(name, args);
        if let Some((_, t)) = self.seen.iter().find(|(k, _)| *k == key) {
            return Some(now.duration_since(*t));
        }
        self.seen.push((key, now));
        None
    }
}

/// Tool result injected for calls cut short by /interrupt.
const INTERRUPTED_RESULT: &str = "{\"error\": \"interrupted by user\"}";
/// Turn text surfaced in the chat pane after an interrupt.
const INTERRUPTED_NOTE: &str = "[turn interrupted]";

pub enum OrchestratorMsg {
    /// A chat message from the user addressed to the orchestrator.
    UserChat(String),
    /// A proactive session event (WAITING / ERROR / DEAD).
    SessionEvent {
        session_id: usize,
        name: String,
        kind: String,
        state: String,
        waiting_prompt: Option<String>,
        tail: String,
    },
    /// Out-of-band note (kill approved/denied, etc.).
    SystemNote(String),
    /// Drop the conversation history (/reset). Anything queued before the
    /// reset is discarded with it; messages after it start the fresh context.
    Reset,
}

impl OrchestratorMsg {
    /// Render as one user-turn line. Events and notes are wrapped so the
    /// model can tell them apart from the human.
    fn render(&self) -> String {
        match self {
            OrchestratorMsg::UserChat(text) => text.clone(),
            OrchestratorMsg::SessionEvent {
                session_id,
                name,
                kind,
                state,
                waiting_prompt,
                tail,
            } => {
                let prompt = waiting_prompt
                    .as_deref()
                    .map(|p| format!("\nprompt: {}", p))
                    .unwrap_or_default();
                format!(
                    "[linkshell event] session {} \"{}\" ({}) is now {}.{}\nrecent output:\n{}",
                    session_id, name, kind, state, prompt, tail
                )
            }
            OrchestratorMsg::SystemNote(note) => format!("[linkshell] {}", note),
            OrchestratorMsg::Reset => String::new(),
        }
    }

    fn is_user_chat(&self) -> bool {
        matches!(self, OrchestratorMsg::UserChat(_))
    }
}

/// Fold a queued batch of messages into one user turn. A Reset anywhere in
/// the batch wipes the conversation history and everything queued before it;
/// messages after the reset start the fresh context. Returns None text when
/// the batch was a pure reset with nothing left to say to the model.
fn coalesce_batch(
    msgs: &[OrchestratorMsg],
    history: &mut Vec<serde_json::Value>,
) -> (Option<String>, bool, bool) {
    let mut parts = Vec::new();
    let mut any_user = false;
    let mut any_actionable = false;
    for m in msgs {
        if matches!(m, OrchestratorMsg::Reset) {
            history.clear();
            parts.clear();
            any_user = false;
            any_actionable = false;
        } else {
            any_user |= m.is_user_chat();
            // WAITING / ERROR / DEAD events (and system notes) must always
            // produce a visible report; only pure informational batches
            // (e.g. READY transitions) may be answered with a silent `ok`.
            any_actionable |= match m {
                OrchestratorMsg::SessionEvent { state, .. } => {
                    let s = state.trim_end_matches('!');
                    !matches!(s, "READY" | "STARTING" | "THINKING" | "RUNNING")
                }
                OrchestratorMsg::SystemNote(_) => true,
                _ => false,
            };
            parts.push(m.render());
        }
    }
    if parts.is_empty() {
        (None, any_user, any_actionable)
    } else {
        (Some(parts.join("\n\n")), any_user, any_actionable)
    }
}

/// Spawn the orchestrator task. Only valid for API-class providers.
pub fn spawn(cfg: OrchestratorConfig, event_tx: mpsc::Sender<AppEvent>) -> OrchestratorHandle {
    cfg.ensure_agent_files();
    let (tx, mut rx) = mpsc::channel::<OrchestratorMsg>(64);
    let (interrupt_tx, interrupt_rx) = tokio::sync::watch::channel(0u64);
    let name = cfg.name.clone();
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        // Provider-native message history (anthropic and openai shapes differ,
        // but both are serde_json Values in a flat Vec).
        let mut history: Vec<serde_json::Value> = Vec::new();
        let mut calls = CallLog::new(cfg.tool_dedup_secs);
        while let Some(first) = rx.recv().await {
            // Coalesce whatever queued up while we were idle or mid-turn into
            // a single user turn — an event storm becomes one API call.
            let mut msgs = vec![first];
            while let Ok(next) = rx.try_recv() {
                msgs.push(next);
            }
            let had_reset = msgs.iter().any(|m| matches!(m, OrchestratorMsg::Reset));
            let (user_text, any_user, any_actionable) = coalesce_batch(&msgs, &mut history);
            if had_reset {
                calls.clear();
            }
            let Some(user_text) = user_text else {
                // Pure reset, nothing to say to the model.
                continue;
            };

            // Snapshot at turn start so an /interrupt sent while idle
            // doesn't cancel the next turn.
            let mut interrupt = Interrupt {
                rx: interrupt_rx.clone(),
                start: *interrupt_rx.borrow(),
            };
            let result = match cfg.class() {
                Ok(OrchestratorClass::Api(ApiProvider::Anthropic)) => {
                    anthropic::run_turn(
                        &cfg,
                        &client,
                        &mut history,
                        &mut calls,
                        &user_text,
                        &event_tx,
                        &mut interrupt,
                    )
                    .await
                }
                Ok(OrchestratorClass::Api(ApiProvider::OpenAi)) => {
                    openai::run_turn(
                        &cfg,
                        &client,
                        &mut history,
                        &mut calls,
                        &user_text,
                        &event_tx,
                        &mut interrupt,
                    )
                    .await
                }
                _ => Err(anyhow::anyhow!(
                    "orchestrator task started for CLI provider"
                )),
            };
            let _ = event_tx.send(AppEvent::OrchestratorStatus(None)).await;
            let text = match result {
                Ok(text) => text,
                Err(e) => format!("[{}: error: {}]", cfg.name, e),
            };
            // Always answer a human. The bare `ok` acknowledgment is only a
            // valid no-op for purely informational batches; small models
            // over-apply it, so for actionable events (WAITING/ERROR/DEAD)
            // an `ok` or empty reply is replaced with the raw event text —
            // the user must hear about a blocked session even when the
            // model under-delivers.
            let trimmed = text.trim();
            let noop_ack =
                trimmed.eq_ignore_ascii_case("ok") || trimmed.eq_ignore_ascii_case("ok.");
            let text = if any_actionable && (noop_ack || trimmed.is_empty()) {
                user_text.clone()
            } else {
                text
            };
            if any_user || any_actionable || (!text.trim().is_empty() && !noop_ack) {
                let _ = event_tx
                    .send(AppEvent::ChatReply {
                        from: cfg.name.clone(),
                        text,
                    })
                    .await;
            }
        }
    });
    OrchestratorHandle {
        tx,
        name,
        interrupt_tx,
    }
}

// ── Tools ──────────────────────────────────────────────────────────────────

/// One logical tool set; converted per provider wire format below.
/// (name, description, JSON Schema for the arguments)
fn tool_specs() -> Vec<(&'static str, &'static str, serde_json::Value)> {
    vec![
        (
            "list_sessions",
            "List all linkshell sessions with id, name, kind, state, cwd, waiting_prompt and token/cost stats. `id` is what every other tool expects; `display` is the 1-based number the user sees.",
            serde_json::json!({"type": "object", "properties": {}}),
        ),
        (
            "read_output",
            "Read the last N output lines of a session.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "integer"},
                    "lines": {"type": "integer", "default": 50}
                },
                "required": ["session_id"]
            }),
        ),
        (
            "start_session",
            "Start a new session. kind is one of claude, codex, opencode, omp, aider, shell. cwd is the working directory. initial_prompt, if given, is typed into the session once it is ready.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["claude", "codex", "opencode", "omp", "aider", "shell"]},
                    "name": {"type": "string"},
                    "cwd": {"type": "string"},
                    "initial_prompt": {"type": "string"}
                },
                "required": ["kind"]
            }),
        ),
        (
            "send_input",
            "Type text (plus Enter) into a session's terminal. With wait_ready=true, blocks until the session returns to READY and returns the output produced in between — use it to ask a session something and read its answer.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "integer"},
                    "text": {"type": "string"},
                    "wait_ready": {"type": "boolean", "default": false}
                },
                "required": ["session_id", "text"]
            }),
        ),
        (
            "pipe_add",
            "Add a pipe that relays an extract of the source session's output into the dest session when the trigger state is hit.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "source": {"type": "integer"},
                    "dest": {"type": "integer"},
                    "trigger": {"type": "string", "enum": ["on_ready", "on_waiting", "manual"], "default": "on_ready"},
                    "extract": {"type": "string", "description": "last-block | last-n=N | diff | summarize=N", "default": "last-block"},
                    "prefix": {"type": "string"}
                },
                "required": ["source", "dest"]
            }),
        ),
        (
            "pipe_remove",
            "Remove pipe(s) from a source session (all of them, or just the one to dest).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "source": {"type": "integer"},
                    "dest": {"type": "integer"}
                },
                "required": ["source"]
            }),
        ),
        (
            "use_skill",
            "Load the full text of a named skill from the skills list in your instructions. Returns the skill's markdown; follow it for the task at hand.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }),
        ),
        (
            "remember",
            "Append one short durable note to your persistent memory file (shown in your instructions each turn). Use it for facts worth carrying across sessions: project layout, user preferences, recurring commands. One sentence per note; the user prunes the file by hand.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string"}
                },
                "required": ["text"]
            }),
        ),
        (
            "pause_session",
            "Pause a session's process (SIGSTOP). The session stays alive with its full context but uses no CPU until resumed — use this when sessions compete for limited system resources and one should yield.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "integer"}
                },
                "required": ["session_id"]
            }),
        ),
        (
            "resume_session",
            "Resume a previously paused session (SIGCONT).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "integer"}
                },
                "required": ["session_id"]
            }),
        ),
        (
            "request_kill",
            "Ask the user for permission to kill a session. This never kills directly — the user must approve with /confirm-kill.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "integer"},
                    "reason": {"type": "string"}
                },
                "required": ["session_id", "reason"]
            }),
        ),
    ]
}

/// Anthropic `tools` array shape.
/// Injected as a final user-turn note when the tool-iteration budget runs
/// out: forces a text answer so partial progress reaches the user instead of
/// a dead-end sentinel. The final request also sets tool_choice to none, so
/// the model cannot spend the landing turn on another tool call.
const EXHAUSTION_NUDGE: &str = "[linkshell] Tool iteration budget for this turn is exhausted. \
Do not call any more tools. Summarize what you did, what you learned from the tool results \
above, and what (if anything) remains to be done.";

fn anthropic_tools() -> serde_json::Value {
    tool_specs()
        .into_iter()
        .map(|(name, desc, schema)| {
            serde_json::json!({"name": name, "description": desc, "input_schema": schema})
        })
        .collect()
}

/// OpenAI `tools` array shape.
fn openai_tools() -> serde_json::Value {
    tool_specs()
        .into_iter()
        .map(|(name, desc, schema)| {
            serde_json::json!({
                "type": "function",
                "function": {"name": name, "description": desc, "parameters": schema}
            })
        })
        .collect()
}

/// Parse a tool call into a request, dispatch it to the main loop, and wait.
/// Always returns a JSON string suitable as a tool result.
/// Announce tool execution in the chat-pane status line.
async fn send_status(event_tx: &mpsc::Sender<AppEvent>, status: impl Into<String>) {
    let _ = event_tx
        .send(AppEvent::OrchestratorStatus(Some(status.into())))
        .await;
}

/// Compact, human-readable rendering of a gated tool call for the proposal
/// line — enough to judge the call without reading raw JSON.
fn proposal_detail(name: &str, args: &serde_json::Value) -> String {
    fn trunc(s: &str, max: usize) -> String {
        if s.chars().count() <= max {
            s.to_string()
        } else {
            let cut: String = s.chars().take(max).collect();
            format!("{}…", cut)
        }
    }
    match name {
        "send_input" => format!(
            "session {} ← {:?}",
            args["session_id"].as_u64().unwrap_or(0),
            trunc(args["text"].as_str().unwrap_or(""), 80),
        ),
        "start_session" => {
            let cwd = args["cwd"].as_str().unwrap_or("");
            format!(
                "{} \"{}\"{}{}",
                args["kind"].as_str().unwrap_or("?"),
                args["name"].as_str().unwrap_or("unnamed"),
                if cwd.is_empty() {
                    String::new()
                } else {
                    format!(" in {}", cwd)
                },
                args["initial_prompt"]
                    .as_str()
                    .map(|p| format!(" — prompt: {:?}", trunc(p, 60)))
                    .unwrap_or_default(),
            )
        }
        _ => trunc(&args.to_string(), 120),
    }
}

async fn exec_tool(
    cfg: &OrchestratorConfig,
    event_tx: &mpsc::Sender<AppEvent>,
    calls: &mut CallLog,
    name: &str,
    args: &serde_json::Value,
) -> String {
    // Repeat-call suppression, before the approval gate: an identical call
    // inside the window is answered rather than executed, so the user is not
    // asked to approve the same proposal twice either.
    if let Some(age) = calls.check(name, args) {
        return serde_json::json!({
            "error": "duplicate_call",
            "detail": format!(
                "you already called {} with these exact arguments {}s ago and the result is \
                 above in this conversation. Nothing was re-run. If you are waiting for a \
                 session to change, use send_input with wait_ready=true, or tell the user \
                 what you are blocked on.",
                name,
                age.as_secs()
            ),
        })
        .to_string();
    }
    // Propose mode: gated tools block here until the human answers in the
    // chat pane (/approve, /deny [reason]) or the timeout fires. Only the
    // orchestrator's own tokio task waits — no HTTP request is held open and
    // the main loop is untouched; from the model's perspective this is just
    // a slow tool, so its context stays coherent.
    if cfg.approval_required(name) {
        let detail = proposal_detail(name, args);
        send_status(event_tx, format!("proposing {} (awaiting approval)", name)).await;
        let (verdict_tx, verdict_rx) = tokio::sync::oneshot::channel();
        if event_tx
            .send(AppEvent::OrchestratorProposal {
                tool: name.to_string(),
                detail,
                response_tx: verdict_tx,
            })
            .await
            .is_err()
        {
            return "{\"error\": \"main loop unavailable\"}".to_string();
        }
        let timeout = std::time::Duration::from_secs(cfg.approval_timeout_secs.max(1));
        match tokio::time::timeout(timeout, verdict_rx).await {
            Ok(Ok(crate::events::ProposalVerdict::Approved)) => {}
            Ok(Ok(crate::events::ProposalVerdict::Denied(reason))) => {
                let reason = if reason.trim().is_empty() {
                    "user denied the request".to_string()
                } else {
                    reason
                };
                return serde_json::json!({"denied": reason}).to_string();
            }
            // Receiver dropped or timed out: treat as a denial the model can
            // report on, not an error that aborts the turn.
            Ok(Err(_)) | Err(_) => {
                return "{\"denied\": \"no response from user (approval timed out)\"}".to_string();
            }
        }
    }
    send_status(event_tx, format!("running {}", name)).await;
    // use_skill reads from the skills directory directly — no main-loop trip.
    if name == "use_skill" {
        let Some(skill_name) = args["name"].as_str() else {
            return "{\"error\": \"missing required arguments\"}".to_string();
        };
        let Some(dir) = cfg.skills_path() else {
            return "{\"error\": \"no skills directory configured\"}".to_string();
        };
        return match skills::read_skill(&dir, skill_name) {
            Ok(content) => serde_json::json!({"name": skill_name, "content": content}).to_string(),
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        };
    }
    // remember appends to the memory file directly — no main-loop trip.
    if name == "remember" {
        let Some(text) = args["text"].as_str().filter(|t| !t.trim().is_empty()) else {
            return "{\"error\": \"missing required arguments\"}".to_string();
        };
        let Some(path) = cfg.memory_path() else {
            return "{\"error\": \"no memory file available\"}".to_string();
        };
        cfg.ensure_agent_files();
        let entry = format!("- ({}) {}\n", today_utc(), text.trim().replace('\n', " "));
        use std::io::Write;
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(entry.as_bytes()));
        return match result {
            Ok(()) => {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let mut reply = serde_json::json!({"remembered": text.trim(), "file_bytes": size});
                if size > 8 * 1024 {
                    reply["warning"] = serde_json::json!(
                        "memory file exceeds 8 KiB and will be truncated in your prompt — suggest the user prune it"
                    );
                }
                reply.to_string()
            }
            Err(e) => serde_json::json!({"error": e.to_string()}).to_string(),
        };
    }

    let sid = |key: &str| args[key].as_u64().map(|v| v as usize);
    let req = match name {
        "list_sessions" => Some(OrchestratorReq::ListSessions),
        "read_output" => sid("session_id").map(|session_id| OrchestratorReq::ReadOutput {
            session_id,
            lines: args["lines"].as_u64().unwrap_or(50) as usize,
        }),
        "start_session" => args["kind"]
            .as_str()
            .map(|kind| OrchestratorReq::StartSession {
                kind: kind.to_string(),
                name: args["name"].as_str().unwrap_or("").to_string(),
                cwd: args["cwd"].as_str().unwrap_or(".").to_string(),
                initial_prompt: args["initial_prompt"].as_str().map(|s| s.to_string()),
            }),
        "send_input" => match (sid("session_id"), args["text"].as_str()) {
            (Some(session_id), Some(text)) => Some(OrchestratorReq::SendInput {
                session_id,
                text: text.to_string(),
                wait_ready: args["wait_ready"].as_bool().unwrap_or(false),
            }),
            _ => None,
        },
        "pipe_add" => match (sid("source"), sid("dest")) {
            (Some(source), Some(dest)) => Some(OrchestratorReq::PipeAdd {
                source,
                dest,
                trigger: args["trigger"].as_str().unwrap_or("on_ready").to_string(),
                extract: args["extract"].as_str().unwrap_or("last-block").to_string(),
                prefix: args["prefix"].as_str().map(|s| s.to_string()),
            }),
            _ => None,
        },
        "pipe_remove" => sid("source").map(|source| OrchestratorReq::PipeRemove {
            source,
            dest: sid("dest"),
        }),
        "pause_session" => sid("session_id").map(|session_id| OrchestratorReq::SetPaused {
            session_id,
            paused: true,
        }),
        "resume_session" => sid("session_id").map(|session_id| OrchestratorReq::SetPaused {
            session_id,
            paused: false,
        }),
        "request_kill" => sid("session_id").map(|session_id| OrchestratorReq::RequestKill {
            session_id,
            reason: args["reason"].as_str().unwrap_or("").to_string(),
        }),
        _ => return format!("{{\"error\": \"unknown tool: {}\"}}", name),
    };
    let Some(req) = req else {
        return "{\"error\": \"missing required arguments\"}".to_string();
    };

    let timeout = std::time::Duration::from_secs(match &req {
        OrchestratorReq::SendInput {
            wait_ready: true, ..
        } => cfg.input_wait_timeout_secs,
        OrchestratorReq::StartSession { .. } => 10,
        _ => 5,
    });
    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
    if event_tx
        .send(AppEvent::OrchestratorRequest {
            req,
            response_tx: resp_tx,
        })
        .await
        .is_err()
    {
        return "{\"error\": \"linkshell main loop is gone\"}".to_string();
    }
    match tokio::time::timeout(timeout, resp_rx).await {
        Ok(Ok(v)) => v.to_string(),
        Ok(Err(_)) => "{\"error\": \"request dropped\"}".to_string(),
        Err(_) => "{\"error\": \"timed out waiting for linkshell\"}".to_string(),
    }
}

// ── Prompts ────────────────────────────────────────────────────────────────

fn system_prompt(cfg: &OrchestratorConfig) -> String {
    let mut p = String::from(
        "You are the resident orchestrator agent inside linkshell, a terminal \
multiplexer for AI coding sessions (claude, codex, opencode, oh-my-pi, aider, shells). \
You watch over all sessions on the user's behalf.\n\
\n\
Session states: STARTING, READY (idle, will accept input), THINKING/RUNNING (busy), \
WAITING (blocked on user input — the waiting_prompt field says what it asked), \
ERROR, DEAD. Use your tools to inspect sessions, start new ones in any directory, \
type into them, and wire pipes between them. Tool session_id arguments take the raw \
`id` from list_sessions; the user sees 1-based `display` numbers, so when talking to \
the user, call sessions by their display number or name.\n\
\n\
You cannot kill sessions. request_kill only files a request the user must approve \
with /confirm-kill. You can however pause_session/resume_session: pausing stops a \
session's process (SIGSTOP) without losing its context — its state shows PAUSED — \
which is the right lever when concurrent sessions contend for limited CPU or RAM.\n\
\n\
Messages starting with [linkshell event] are automatic notifications that a session \
changed state. For WAITING, ERROR, and DEAD events you MUST report: investigate \
briefly (read_output) and tell the user in one or two sentences what happened, what \
it needs, and what you suggest — never answer these with just `ok`. Only when ALL \
events in the message are informational (READY/STARTING/THINKING/RUNNING) and nothing \
depends on them: make no tool calls and reply with exactly `ok` — that reply is \
suppressed and never shown to the user. \
Messages starting \
with [linkshell] are system notes.\n\
\n\
Your replies render in a small chat pane: be concise, no markdown headers.",
    );
    if let Some(list) = skills_section(cfg, false) {
        p.push_str(
            "\n\nSkills — playbooks you can load with the use_skill tool. When a task \
matches a skill's description, load it and follow it:\n",
        );
        p.push_str(&list);
    }
    if cfg.approval == "propose" {
        p.push_str(
            "\nSome of your tool calls require the user's approval before they run; \
they may take a while to return while the user decides. A tool result of \
{\"denied\": \"...\"} means the user refused that call — do not retry it \
unchanged. If the denial carries a reason, adjust your approach accordingly \
and continue; otherwise report what you wanted to do and why.\n",
        );
    }
    if !cfg.system.is_empty() {
        p.push_str("\n\n");
        p.push_str(&cfg.system);
    }
    // Memory goes last: it is the only part of the system prompt that
    // mutates mid-conversation (remember writes), so keeping everything
    // above it byte-stable lets prompt prefix caches (Anthropic caching,
    // llama.cpp prefix cache) re-serve the static bulk after each write.
    if let Some(memory) = memory_section(cfg) {
        p.push_str(&memory);
    }
    p
}

/// Briefing typed into a CLI-class orchestrator session once it is READY.
pub fn cli_briefing(cfg: &OrchestratorConfig) -> String {
    cfg.ensure_agent_files();
    let mut p = String::from(
        "You are the resident orchestrator agent for this linkshell instance, a terminal \
multiplexer running AI coding sessions. Your job: keep track of every session, act on \
the user's instructions, and report through the chat pane.\n\
Use the `linkshell-ctl` CLI (already authorized via your environment):\n\
  linkshell-ctl list                              # all sessions: id, state, cwd, tokens\n\
  linkshell-ctl read <id> [n]                     # last n output lines of a session\n\
  linkshell-ctl new <kind> [name] [--cwd=PATH]    # start claude|codex|opencode|omp|aider|shell\n\
  linkshell-ctl input <id> <text...> [--wait]     # type into a session; --wait returns its answer\n\
  linkshell-ctl pipe add <src> <dst> [--extract=X] [--trigger=X]\n\
  linkshell-ctl kill <id> [reason]                # only ASKS the user; they must /confirm-kill\n\
  linkshell-ctl chat <message>                    # speak to the user in the chat pane\n\
Session ids here are the raw `id` field from `list`; when talking to the user, use the \
`display` number from `list` — that is what they see in the UI.\n\
ALWAYS reply to the user with `linkshell-ctl chat` — plain text you print may not reach them. \
Keep chat messages short.\n\
Lines arriving that start with [linkshell event] mean a session changed state \
(WAITING/ERROR/DEAD): investigate with `list`/`read`, then summarize for the user via \
`linkshell-ctl chat`. Do not modify files unless the user asks; your role is coordination.",
    );
    if let Some(list) = skills_section(cfg, true) {
        p.push_str(
            "\nSkills — playbooks stored as files. When a task matches a skill's \
description, read the file and follow it:\n",
        );
        p.push_str(&list);
    }
    if let Some(path) = cfg.memory_path() {
        p.push_str(&format!(
            "\nPersistent memory: {} — read it now; it carries durable notes from \
previous sessions. When you learn something durable (project layout, user \
preferences, recurring commands), append a short dated bullet there. Keep it \
concise; the user prunes it by hand.\n",
            path.display()
        ));
    }
    if !cfg.system.is_empty() {
        p.push_str("\n\n");
        p.push_str(&cfg.system);
    }
    p
}

/// Memory block for the API-class system prompt: the memory file verbatim,
/// or None when it is missing/empty. Injected every turn — the size guard
/// keeps a bloated file from eating the context and makes the bloat visible
/// so the user knows to prune.
fn memory_section(cfg: &OrchestratorConfig) -> Option<String> {
    const MAX_BYTES: usize = 8 * 1024;
    let path = cfg.memory_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    let (body, truncated) = if content.len() > MAX_BYTES {
        let mut end = MAX_BYTES;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        (&content[..end], true)
    } else {
        (content.as_str(), false)
    };
    let mut section = format!(
        "\n\nMemory — durable notes from previous sessions, kept in {} (the user \
curates this file; treat entries as possibly stale). Use the `remember` tool \
to add short dated notes when you learn something durable:\n{}",
        path.display(),
        body
    );
    if truncated {
        section.push_str(
            "\n[memory truncated at 8 KiB — tell the user their memory.md needs pruning]\n",
        );
    }
    Some(section)
}

/// UTC date as YYYY-MM-DD without a date dependency (civil-from-days,
/// Howard Hinnant's algorithm).
fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Skills list for a prompt, or None when no skills are configured/present.
fn skills_section(cfg: &OrchestratorConfig, with_paths: bool) -> Option<String> {
    let dir = cfg.skills_path()?;
    let list = skills::load_skills(&dir);
    if list.is_empty() {
        return None;
    }
    Some(skills::skill_list(&list, with_paths))
}

/// Stub text substituted for aged-out tool results whose originating call
/// could not be identified. Also the minimum length worth eliding.
const ELIDED_RESULT: &str =
    "[elided to save context — re-run the tool if this result is still needed]";

/// Map tool-call id -> (tool name, arguments) across both provider shapes.
///
/// Aged-out results are replaced by a stub that *names the call it answered*.
/// A bare "[elided]" leaves the model reading its own `tool_use` block
/// followed by nothing, which reads as an unfinished action and invites an
/// immediate re-call; naming the call turns it into a completed one.
fn call_index(
    history: &[serde_json::Value],
) -> std::collections::HashMap<String, (String, serde_json::Value)> {
    let mut map = std::collections::HashMap::new();
    for m in history.iter().filter(|m| m["role"] == "assistant") {
        // Anthropic: content array of blocks, tool_use carries id/name/input.
        if let Some(blocks) = m["content"].as_array() {
            for b in blocks.iter().filter(|b| b["type"] == "tool_use") {
                if let Some(id) = b["id"].as_str() {
                    map.insert(
                        id.to_string(),
                        (
                            b["name"].as_str().unwrap_or("tool").to_string(),
                            b["input"].clone(),
                        ),
                    );
                }
            }
        }
        // OpenAI: tool_calls array, arguments is a JSON-encoded string.
        if let Some(calls) = m["tool_calls"].as_array() {
            for c in calls {
                if let Some(id) = c["id"].as_str() {
                    let args: serde_json::Value = c["function"]["arguments"]
                        .as_str()
                        .and_then(|a| serde_json::from_str(a).ok())
                        .unwrap_or(serde_json::json!({}));
                    map.insert(
                        id.to_string(),
                        (
                            c["function"]["name"].as_str().unwrap_or("tool").to_string(),
                            args,
                        ),
                    );
                }
            }
        }
    }
    map
}

/// Stub naming the call whose result was dropped, with the original size.
fn elision_stub(call: Option<&(String, serde_json::Value)>, original_len: usize) -> String {
    match call {
        Some((name, args)) => format!(
            "[elided: {} {} returned {} chars — re-run only if you still need the detail]",
            name,
            proposal_detail(name, args),
            original_len
        ),
        None => ELIDED_RESULT.to_string(),
    }
}

/// Count plain user-text turns (the boundaries trim_history cuts at).
fn user_turns(history: &[serde_json::Value]) -> usize {
    history
        .iter()
        .filter(|m| m["role"] == "user" && m["content"].is_string())
        .count()
}

/// Rough token estimate for the serialized history (~4 chars/token).
fn estimate_tokens(history: &[serde_json::Value]) -> usize {
    history
        .iter()
        .map(|m| m.to_string().chars().count())
        .sum::<usize>()
        / 4
}

/// Replace tool results older than the last `keep_turns` plain user turns
/// with a short stub. Structure stays API-valid for both providers: the
/// anthropic shape keeps its tool_result blocks (ids intact) with stubbed
/// content, the openai shape keeps its role:"tool" messages likewise.
fn age_tool_results(history: &mut [serde_json::Value], keep_turns: usize) {
    if keep_turns == 0 {
        return;
    }
    let calls = call_index(history);
    // Index of the keep_turns-th plain user turn from the end; everything
    // before it is "old".
    let mut seen = 0;
    let mut boundary = 0;
    for (i, m) in history.iter().enumerate().rev() {
        if m["role"] == "user" && m["content"].is_string() {
            seen += 1;
            if seen == keep_turns {
                boundary = i;
                break;
            }
        }
    }
    if seen < keep_turns {
        return; // whole history is within the keep window
    }
    for m in history[..boundary].iter_mut() {
        // OpenAI shape: {"role": "tool", "content": "..."}
        if m["role"] == "tool" {
            let len = m["content"].as_str().map_or(0, |s| s.len());
            if len > ELIDED_RESULT.len() {
                let call = m["tool_call_id"].as_str().and_then(|id| calls.get(id));
                m["content"] = serde_json::json!(elision_stub(call, len));
            }
            continue;
        }
        // Anthropic shape: user message whose content is an array of
        // tool_result blocks.
        if m["role"] == "user" {
            if let Some(blocks) = m["content"].as_array_mut() {
                for b in blocks.iter_mut().filter(|b| b["type"] == "tool_result") {
                    let len = match &b["content"] {
                        serde_json::Value::String(s) => s.len(),
                        v => v.to_string().len(),
                    };
                    if len > ELIDED_RESULT.len() {
                        let call = b["tool_use_id"].as_str().and_then(|id| calls.get(id));
                        b["content"] = serde_json::json!(elision_stub(call, len));
                    }
                }
            }
        }
    }
}

/// Full history compaction pass, run once per turn before hitting the API:
/// age old tool results, apply the turn cap, then drop oldest turns until
/// the token estimate fits the budget (always keeping the latest turn).
pub(crate) fn compact_history(history: &mut Vec<serde_json::Value>, cfg: &OrchestratorConfig) {
    age_tool_results(history, cfg.tool_result_keep_turns);
    trim_history(history, cfg.max_history_turns);
    if cfg.max_context_tokens == 0 {
        return;
    }
    while estimate_tokens(history) > cfg.max_context_tokens {
        let turns = user_turns(history);
        if turns <= 1 {
            break; // never drop the turn we're about to answer
        }
        trim_history(history, turns - 1);
    }
}

/// Trim provider history in place, dropping oldest turns but only cutting at
/// plain user-text boundaries so tool_use/tool_result pairs stay intact.
fn trim_history(history: &mut Vec<serde_json::Value>, max_turns: usize) {
    let user_turns = history
        .iter()
        .filter(|m| m["role"] == "user" && m["content"].is_string())
        .count();
    if user_turns <= max_turns {
        return;
    }
    let mut to_drop = user_turns - max_turns;
    let mut cut = 0;
    for (i, m) in history.iter().enumerate() {
        if m["role"] == "user" && m["content"].is_string() {
            if to_drop == 0 {
                cut = i;
                break;
            }
            to_drop -= 1;
        }
    }
    if cut == 0 {
        // All remaining drops accounted; find the (max_turns)-th user turn from the end
        return;
    }
    history.drain(..cut);
    // Guard: history must start on a plain user turn
    while let Some(first) = history.first() {
        if first["role"] == "user" && first["content"].is_string() {
            break;
        }
        history.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_specs_convert_to_both_provider_shapes() {
        let a = anthropic_tools();
        let o = openai_tools();
        let n = tool_specs().len();
        assert_eq!(a.as_array().unwrap().len(), n);
        assert_eq!(o.as_array().unwrap().len(), n);
        assert!(a[0]["input_schema"].is_object());
        assert_eq!(o[0]["type"], "function");
        assert!(o[0]["function"]["parameters"].is_object());
    }

    #[test]
    fn today_utc_is_a_plausible_iso_date() {
        let d = today_utc();
        assert_eq!(d.len(), 10, "{}", d);
        assert_eq!(&d[4..5], "-");
        assert_eq!(&d[7..8], "-");
        let year: i32 = d[..4].parse().unwrap();
        assert!((2024..2100).contains(&year), "{}", d);
    }

    #[tokio::test]
    async fn remember_appends_dated_note_and_memory_section_injects_it() {
        let dir = std::env::temp_dir().join(format!("ls-mem-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("memory.md");
        let _ = std::fs::remove_file(&file);

        let cfg = OrchestratorConfig {
            memory_file: file.to_string_lossy().to_string(),
            approval: "propose".to_string(),
            ..Default::default()
        };
        let (tx, _rx) = mpsc::channel::<AppEvent>(8);

        // remember is auto-approved even in propose mode.
        assert!(!cfg.approval_required("remember"));

        let out = exec_tool(
            &cfg,
            &tx,
            &mut CallLog::new(0),
            "remember",
            &serde_json::json!({"text": "user prefers rebase\nover merge"}),
        )
        .await;
        assert!(out.contains("remembered"), "{}", out);

        let content = std::fs::read_to_string(&file).unwrap();
        assert!(
            content.contains("user prefers rebase over merge"),
            "newlines collapse to one line: {}",
            content
        );
        assert!(content.contains(&format!("({})", today_utc())));

        let section = memory_section(&cfg).expect("non-empty memory injects");
        assert!(section.contains("user prefers rebase over merge"));
        assert!(section.contains("remember"));

        // Empty file: no section.
        std::fs::write(&file, "   \n").unwrap();
        assert!(memory_section(&cfg).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn propose_mode_gates_tools_and_returns_deny_reason() {
        let cfg = OrchestratorConfig {
            approval: "propose".to_string(),
            ..Default::default()
        };
        let (tx, mut rx) = mpsc::channel::<AppEvent>(8);

        let handle = tokio::spawn({
            let tx = tx.clone();
            async move {
                exec_tool(
                    &cfg,
                    &tx,
                    &mut CallLog::new(0),
                    "send_input",
                    &serde_json::json!({"session_id": 2, "text": "cargo test"}),
                )
                .await
            }
        });

        // First event: "proposing" status; then the proposal itself.
        let verdict_tx = loop {
            match rx.recv().await {
                Some(AppEvent::OrchestratorProposal {
                    tool,
                    detail,
                    response_tx,
                }) => {
                    assert_eq!(tool, "send_input");
                    assert!(detail.contains("session 2"), "detail: {}", detail);
                    assert!(detail.contains("cargo test"), "detail: {}", detail);
                    break response_tx;
                }
                Some(_) => continue,
                None => panic!("channel closed before proposal"),
            }
        };

        verdict_tx
            .send(crate::events::ProposalVerdict::Denied(
                "wrong session, use 3".into(),
            ))
            .unwrap();
        let result = handle.await.unwrap();
        assert!(
            result.contains("denied") && result.contains("wrong session, use 3"),
            "denial reason reaches the model: {}",
            result
        );
    }

    #[tokio::test]
    async fn auto_approve_tools_skip_the_gate() {
        let mut cfg = OrchestratorConfig {
            approval: "propose".to_string(),
            ..Default::default()
        };
        assert!(cfg.approval_required("send_input"));
        assert!(cfg.approval_required("start_session"));
        assert!(!cfg.approval_required("read_output"));
        assert!(!cfg.approval_required("list_sessions"));
        assert!(!cfg.approval_required("use_skill"));
        // kill_session keeps its dedicated /confirm-kill flow.
        assert!(!cfg.approval_required("kill_session"));
        cfg.approval = "auto".to_string();
        assert!(!cfg.approval_required("send_input"));
    }

    #[tokio::test]
    async fn exec_tool_announces_status_before_running() {
        let cfg = OrchestratorConfig::default();
        let (tx, mut rx) = mpsc::channel::<AppEvent>(8);
        // use_skill with no skills dir: fails fast without a main-loop trip,
        // but the status announcement must still come first.
        let _ = exec_tool(
            &cfg,
            &tx,
            &mut CallLog::new(0),
            "use_skill",
            &serde_json::json!({"name": "x"}),
        )
        .await;
        match rx.try_recv() {
            Ok(AppEvent::OrchestratorStatus(Some(s))) => {
                assert!(s.contains("use_skill"), "status names the tool: {}", s)
            }
            other => panic!("expected OrchestratorStatus, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn reset_clears_history_and_discards_earlier_queued_messages() {
        let mut history = vec![
            serde_json::json!({"role": "user", "content": "old"}),
            serde_json::json!({"role": "assistant", "content": "old reply"}),
        ];
        // Stale note queued before the reset is discarded with the history;
        // the message after the reset starts the fresh context.
        let msgs = vec![
            OrchestratorMsg::SystemNote("stale".into()),
            OrchestratorMsg::Reset,
            OrchestratorMsg::UserChat("two".into()),
        ];
        let (text, any_user, _) = coalesce_batch(&msgs, &mut history);
        assert_eq!(text.as_deref(), Some("two"));
        assert!(any_user);
        assert!(history.is_empty());

        // Pure reset: history wiped, nothing to send.
        let mut history = vec![serde_json::json!({"role": "user", "content": "old"})];
        let (text, _, _) = coalesce_batch(&[OrchestratorMsg::Reset], &mut history);
        assert!(text.is_none());
        assert!(history.is_empty());

        // No reset: plain coalescing.
        let mut history = vec![serde_json::json!({"role": "user", "content": "kept"})];
        let msgs = vec![
            OrchestratorMsg::UserChat("a".into()),
            OrchestratorMsg::SystemNote("b".into()),
        ];
        let (text, any_user, _) = coalesce_batch(&msgs, &mut history);
        assert_eq!(text.as_deref(), Some("a\n\n[linkshell] b"));
        assert!(any_user);
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn waiting_events_are_actionable_ready_events_are_not() {
        let ev = |state: &str| OrchestratorMsg::SessionEvent {
            session_id: 1,
            name: "claude".into(),
            kind: "claude".into(),
            state: state.into(),
            waiting_prompt: None,
            tail: String::new(),
        };
        let mut h = Vec::new();
        let (_, _, actionable) = coalesce_batch(&[ev("READY")], &mut h);
        assert!(!actionable, "READY alone must be suppressible");
        let (_, _, actionable) = coalesce_batch(&[ev("READY"), ev("WAITING!")], &mut h);
        assert!(
            actionable,
            "WAITING (with bar decoration) must always surface"
        );
        let (_, _, actionable) = coalesce_batch(&[ev("ERROR")], &mut h);
        assert!(actionable);
        let (_, _, actionable) =
            coalesce_batch(&[OrchestratorMsg::SystemNote("note".into())], &mut h);
        assert!(actionable, "system notes must always surface");
    }

    #[test]
    fn aging_stubs_only_old_tool_results() {
        let big = "x".repeat(500);
        let mut h = vec![
            serde_json::json!({"role": "user", "content": "one"}),
            serde_json::json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "a", "name": "read_output",
                 "input": {"session_id": 2}}]}),
            serde_json::json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "content": big.clone()}]}),
            serde_json::json!({"role": "assistant", "content": "reply"}),
            serde_json::json!({"role": "user", "content": "two"}),
            serde_json::json!({"role": "assistant", "content": [{"type": "tool_use", "id": "b"}]}),
            serde_json::json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "b", "content": big.clone()}]}),
        ];
        age_tool_results(&mut h, 1);
        // Old result stubbed, id preserved
        let stub = h[2]["content"][0]["content"].as_str().unwrap();
        assert!(stub.starts_with("[elided:"), "{stub}");
        // The stub names the call it answered, so the model reads a completed
        // action rather than a tool_use followed by nothing.
        assert!(stub.contains("read_output"), "{stub}");
        assert!(stub.contains("500 chars"), "{stub}");
        assert_eq!(h[2]["content"][0]["tool_use_id"], "a");
        // Result within the keep window untouched
        assert_eq!(h[6]["content"][0]["content"], big);
        // keep_turns larger than history: no-op
        let before = h.clone();
        age_tool_results(&mut h, 10);
        assert_eq!(h, before);
    }

    #[test]
    fn repeat_calls_are_suppressed_within_the_window() {
        let mut log = CallLog::new(60);
        let args = serde_json::json!({"session_id": 2, "lines": 50});
        assert!(log.check("read_output", &args).is_none());
        // Identical call: reported as a repeat rather than executed again.
        assert!(log.check("read_output", &args).is_some());
        // Different arguments are a different call.
        assert!(log
            .check(
                "read_output",
                &serde_json::json!({"session_id": 3, "lines": 50})
            )
            .is_none());
        // Reset clears the log so a fresh context is not haunted by the old one.
        log.clear();
        assert!(log.check("read_output", &args).is_none());
    }

    #[test]
    fn a_zero_window_disables_suppression() {
        let mut log = CallLog::new(0);
        let args = serde_json::json!({});
        assert!(log.check("list_sessions", &args).is_none());
        assert!(log.check("list_sessions", &args).is_none());
    }

    #[test]
    fn aging_stubs_openai_tool_messages() {
        let big = "y".repeat(500);
        let mut h = vec![
            serde_json::json!({"role": "user", "content": "one"}),
            serde_json::json!({"role": "tool", "content": big}),
            serde_json::json!({"role": "user", "content": "two"}),
        ];
        age_tool_results(&mut h, 1);
        // No matching tool_call_id in history: falls back to the bare stub.
        assert_eq!(h[1]["content"], ELIDED_RESULT);
    }

    #[test]
    fn budget_trim_drops_oldest_turns_but_keeps_the_last() {
        let big = "z".repeat(4000); // ~1000 tokens per turn
        let mut h: Vec<serde_json::Value> = (0..10)
            .flat_map(|i| {
                vec![
                    serde_json::json!({"role": "user", "content": format!("{} {}", i, big)}),
                    serde_json::json!({"role": "assistant", "content": "ok"}),
                ]
            })
            .collect();
        let cfg = OrchestratorConfig {
            max_history_turns: 40,
            max_context_tokens: 3000,
            tool_result_keep_turns: 0,
            ..Default::default()
        };
        compact_history(&mut h, &cfg);
        let turns = user_turns(&h);
        assert!(turns < 10, "should have dropped turns, kept {}", turns);
        assert!(turns >= 1, "must keep at least the latest turn");
        // Latest turn survives
        assert!(h
            .iter()
            .any(|m| m["content"].as_str().is_some_and(|s| s.starts_with("9 "))));
        // Budget disabled: only the turn cap applies
        let mut h2 = vec![serde_json::json!({"role": "user", "content": "u".repeat(100000)})];
        let cfg2 = OrchestratorConfig {
            max_context_tokens: 0,
            ..Default::default()
        };
        compact_history(&mut h2, &cfg2);
        assert_eq!(h2.len(), 1);
    }

    #[test]
    fn trim_history_cuts_only_at_plain_user_turns() {
        // 3 user turns, with an assistant tool_use + user tool_result between
        let mut h = vec![
            serde_json::json!({"role": "user", "content": "one"}),
            serde_json::json!({"role": "assistant", "content": [{"type": "tool_use"}]}),
            serde_json::json!({"role": "user", "content": [{"type": "tool_result"}]}),
            serde_json::json!({"role": "assistant", "content": "reply"}),
            serde_json::json!({"role": "user", "content": "two"}),
            serde_json::json!({"role": "assistant", "content": "reply"}),
            serde_json::json!({"role": "user", "content": "three"}),
        ];
        trim_history(&mut h, 2);
        assert_eq!(h.len(), 3);
        assert_eq!(h[0]["content"], "two");
        // Under the cap: untouched
        let mut h2 = vec![serde_json::json!({"role": "user", "content": "only"})];
        trim_history(&mut h2, 2);
        assert_eq!(h2.len(), 1);
    }

    /// Serve a fixed sequence of JSON bodies, one HTTP connection each.
    async fn mock_http(bodies: Vec<serde_json::Value>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for body in bodies {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let _ = sock.read(&mut buf).await;
                let payload = body.to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    payload.len(),
                    payload
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        format!("http://{}", addr)
    }

    /// Answer OrchestratorRequest events, flagging when ListSessions arrives.
    fn spawn_tool_responder(
        mut rx: mpsc::Receiver<AppEvent>,
        saw_list: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if let AppEvent::OrchestratorRequest { req, response_tx } = ev {
                    if matches!(req, crate::events::OrchestratorReq::ListSessions) {
                        saw_list.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    let _ = response_tx.send(serde_json::json!([{"id": 0, "name": "s1"}]));
                }
            }
        });
    }

    /// An Interrupt that never fires (the dropped sender closes the channel,
    /// which `wait` treats as "pend forever").
    fn test_interrupt() -> Interrupt {
        let (_tx, rx) = tokio::sync::watch::channel(0u64);
        Interrupt { rx, start: 0 }
    }

    #[tokio::test]
    async fn interrupt_breaks_the_turn_before_the_next_iteration() {
        let (tx, rx) = tokio::sync::watch::channel(0u64);
        let mut interrupt = Interrupt { rx, start: 0 };
        tx.send(1).unwrap(); // user hit /interrupt

        let cfg = OrchestratorConfig {
            provider: "anthropic".into(),
            endpoint: "http://127.0.0.1:1".into(), // must never be contacted
            api_key: "test-key".into(),
            ..Default::default()
        };
        let (event_tx, _event_rx) = mpsc::channel::<AppEvent>(32);
        let client = reqwest::Client::new();
        let mut history = Vec::new();
        let text = anthropic::run_turn(
            &cfg,
            &client,
            &mut history,
            &mut CallLog::new(0),
            "do something",
            &event_tx,
            &mut interrupt,
        )
        .await
        .unwrap();
        assert_eq!(text, INTERRUPTED_NOTE);
        assert_eq!(history.len(), 1); // just the user turn; no API call made
    }

    #[tokio::test]
    async fn openai_loop_executes_tools_and_returns_final_text() {
        let endpoint = mock_http(vec![
            serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c1", "type": "function",
                     "function": {"name": "list_sessions", "arguments": "{}"}}
                ]}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
            }),
            serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "one session running"}}],
                "usage": {"prompt_tokens": 20, "completion_tokens": 6}
            }),
        ])
        .await;

        let cfg = OrchestratorConfig {
            provider: "lmstudio".into(),
            endpoint,
            model: "test".into(),
            ..Default::default()
        };
        let (event_tx, event_rx) = mpsc::channel::<AppEvent>(32);
        let saw_list = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        spawn_tool_responder(event_rx, saw_list.clone());

        let client = reqwest::Client::new();
        let mut history = Vec::new();
        let mut interrupt = test_interrupt();
        let text = openai::run_turn(
            &cfg,
            &client,
            &mut history,
            &mut CallLog::new(0),
            "what's running?",
            &event_tx,
            &mut interrupt,
        )
        .await
        .unwrap();
        assert_eq!(text, "one session running");
        assert!(saw_list.load(std::sync::atomic::Ordering::SeqCst));
        // user, assistant(tool_calls), tool result, final assistant
        assert_eq!(history.len(), 4);
        assert_eq!(history[2]["role"], "tool");
    }

    #[tokio::test]
    async fn anthropic_loop_executes_tools_and_replays_content_verbatim() {
        let endpoint = mock_http(vec![
            serde_json::json!({
                "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "tool_use", "id": "t1", "name": "list_sessions", "input": {}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }),
            serde_json::json!({
                "content": [{"type": "text", "text": "one session running"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 20, "output_tokens": 6}
            }),
        ])
        .await;

        let cfg = OrchestratorConfig {
            provider: "anthropic".into(),
            endpoint,
            api_key: "test-key".into(),
            ..Default::default()
        };
        let (event_tx, event_rx) = mpsc::channel::<AppEvent>(32);
        let saw_list = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        spawn_tool_responder(event_rx, saw_list.clone());

        let client = reqwest::Client::new();
        let mut history = Vec::new();
        let mut interrupt = test_interrupt();
        let text = anthropic::run_turn(
            &cfg,
            &client,
            &mut history,
            &mut CallLog::new(0),
            "what's running?",
            &event_tx,
            &mut interrupt,
        )
        .await
        .unwrap();
        assert_eq!(text, "one session running");
        assert!(saw_list.load(std::sync::atomic::Ordering::SeqCst));
        // user, assistant(thinking+tool_use), tool_result user, final assistant
        assert_eq!(history.len(), 4);
        // Thinking block replayed unchanged in history
        assert_eq!(history[1]["content"][0]["type"], "thinking");
        assert_eq!(history[2]["content"][0]["type"], "tool_result");
    }

    #[test]
    fn event_rendering_wraps_and_user_chat_passes_through() {
        let ev = OrchestratorMsg::SessionEvent {
            session_id: 3,
            name: "codex-1".into(),
            kind: "codex".into(),
            state: "WAITING".into(),
            waiting_prompt: Some("continue? [y/n]".into()),
            tail: "…".into(),
        };
        let s = ev.render();
        assert!(s.starts_with("[linkshell event]"));
        assert!(s.contains("continue? [y/n]"));
        assert!(!ev.is_user_chat());
        let u = OrchestratorMsg::UserChat("hi".into());
        assert_eq!(u.render(), "hi");
        assert!(u.is_user_chat());
    }
}
