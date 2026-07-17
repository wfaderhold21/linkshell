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
}

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
        }
    }

    fn is_user_chat(&self) -> bool {
        matches!(self, OrchestratorMsg::UserChat(_))
    }
}

/// Spawn the orchestrator task. Only valid for API-class providers.
pub fn spawn(cfg: OrchestratorConfig, event_tx: mpsc::Sender<AppEvent>) -> OrchestratorHandle {
    let (tx, mut rx) = mpsc::channel::<OrchestratorMsg>(64);
    let name = cfg.name.clone();
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        // Provider-native message history (anthropic and openai shapes differ,
        // but both are serde_json Values in a flat Vec).
        let mut history: Vec<serde_json::Value> = Vec::new();
        while let Some(first) = rx.recv().await {
            // Coalesce whatever queued up while we were idle or mid-turn into
            // a single user turn — an event storm becomes one API call.
            let mut parts = vec![first.render()];
            let mut any_user = first.is_user_chat();
            while let Ok(next) = rx.try_recv() {
                any_user |= next.is_user_chat();
                parts.push(next.render());
            }
            let user_text = parts.join("\n\n");

            let result = match cfg.class() {
                Ok(OrchestratorClass::Api(ApiProvider::Anthropic)) => {
                    anthropic::run_turn(&cfg, &client, &mut history, &user_text, &event_tx).await
                }
                Ok(OrchestratorClass::Api(ApiProvider::OpenAi)) => {
                    openai::run_turn(&cfg, &client, &mut history, &user_text, &event_tx).await
                }
                _ => Err(anyhow::anyhow!(
                    "orchestrator task started for CLI provider"
                )),
            };
            let text = match result {
                Ok(text) => text,
                Err(e) => format!("[{}: error: {}]", cfg.name, e),
            };
            // Always answer a human; stay quiet only if an event turn produced
            // nothing (the model may have just filed tool calls / no comment).
            if any_user || !text.trim().is_empty() {
                let _ = event_tx
                    .send(AppEvent::ChatReply {
                        from: cfg.name.clone(),
                        text,
                    })
                    .await;
            }
        }
    });
    OrchestratorHandle { tx, name }
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
async fn exec_tool(
    cfg: &OrchestratorConfig,
    event_tx: &mpsc::Sender<AppEvent>,
    name: &str,
    args: &serde_json::Value,
) -> String {
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
with /confirm-kill.\n\
\n\
Messages starting with [linkshell event] are automatic notifications that a session \
changed state; investigate briefly (read_output) and tell the user in one or two \
sentences what happened, what it needs, and what you suggest. Messages starting \
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
    if !cfg.system.is_empty() {
        p.push_str("\n\n");
        p.push_str(&cfg.system);
    }
    p
}

/// Briefing typed into a CLI-class orchestrator session once it is READY.
pub fn cli_briefing(cfg: &OrchestratorConfig) -> String {
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
    if !cfg.system.is_empty() {
        p.push_str("\n\n");
        p.push_str(&cfg.system);
    }
    p
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
        let text = openai::run_turn(&cfg, &client, &mut history, "what's running?", &event_tx)
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
        let text = anthropic::run_turn(&cfg, &client, &mut history, "what's running?", &event_tx)
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
