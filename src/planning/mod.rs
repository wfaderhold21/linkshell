//! Planning threads: a persistent, single-agent chat for designing work
//! before an implementation session starts.
//!
//! This is deliberately *not* the orchestrator chat. That pane is a log —
//! append-only, ephemeral, tail-oriented, you only care about the last few
//! lines. A planning thread is a document: you scroll back, you edit it, you
//! fork it, and its output is an artifact handed to an agent. Different
//! enough that sharing one widget would be a mistake.
//!
//! Three properties shape everything here:
//!
//! * **Read-only.** Planning grounds itself in the repository but never
//!   mutates it. The tool surface in [`tools`] contains no write primitive,
//!   so the guarantee is structural rather than prompted.
//! * **Runtime backend choice.** Anthropic, OpenAI, LM Studio and llama.cpp
//!   are all selectable per turn, so a thread can be built cheaply on a local
//!   model and distilled by a frontier one. Which model produced which turn
//!   is recorded per message.
//! * **Global threads, pinned roots.** Threads are discoverable from
//!   anywhere; the directory a thread was grounded in is a property of the
//!   thread, never re-derived from wherever it was opened.
//!
//! Note that a planning agent is not a subprocess — it is an HTTP client
//! inside linkshell, with tool calls executed in this address space. There is
//! nothing for `bwrap` to contain, which is why the sandbox is a scoped tool
//! registry rather than a namespace. If planning ever needs to run a real
//! binary (`cargo check`, an LSP query), that changes and bwrap comes back.

pub mod distill;
pub mod store;
pub mod tools;

use std::path::Path;

use crate::events::AppEvent;
use store::{Message, Role, Thread};
use tokio::sync::mpsc;

/// One selectable model endpoint, from `[planning.backends.NAME]`.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct Backend {
    /// Key in the config table; shown in the picker and stored per message.
    #[serde(skip)]
    pub name: String,
    /// "anthropic", "openai", "lmstudio", or "llamacpp".
    pub provider: String,
    /// Base URL. Optional for anthropic/openai (env or default is used).
    pub endpoint: String,
    pub model: String,
    pub api_key: String,
    /// Anthropic bearer token, for gateways that require it.
    pub auth_token: String,
    pub max_tokens: u32,
    /// Soft budget for the request, estimated at ~4 chars/token. A thread
    /// that fits Anthropic's window will not fit LM Studio's, so this is a
    /// per-backend number and switching model re-evaluates it.
    pub max_context_tokens: usize,
    /// Tool round-trips allowed in one turn. Grounding a plan in a codebase
    /// is a grep-read-grep-read walk, and a question worth asking often needs
    /// dozens of hops — this is a runaway-loop bound, not a budget the model
    /// is expected to work within. Per-backend because a small local model
    /// takes more hops to reach the same place than a frontier one.
    pub max_tool_iterations: usize,
}

impl Default for Backend {
    fn default() -> Self {
        Backend {
            name: String::new(),
            provider: "anthropic".to_string(),
            endpoint: String::new(),
            model: String::new(),
            api_key: String::new(),
            auth_token: String::new(),
            max_tokens: 4096,
            max_context_tokens: 60_000,
            max_tool_iterations: 40,
        }
    }
}

/// Which wire protocol a backend speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Anthropic,
    /// OpenAI-compatible `/v1/chat/completions`; also LM Studio and llama.cpp.
    OpenAi,
}

impl Backend {
    pub fn wire(&self) -> anyhow::Result<Wire> {
        match self.provider.as_str() {
            "anthropic" => Ok(Wire::Anthropic),
            "openai" | "lmstudio" | "llamacpp" | "llama.cpp" | "ollama" | "vllm" => {
                Ok(Wire::OpenAi)
            }
            other => anyhow::bail!(
                "unknown planning provider {:?} (expected anthropic, openai, lmstudio, llamacpp)",
                other
            ),
        }
    }

    /// Base URL for requests, falling back to provider defaults and env vars.
    pub fn endpoint_url(&self) -> String {
        if !self.endpoint.is_empty() {
            return self.endpoint.clone();
        }
        match self.wire() {
            Ok(Wire::Anthropic) => std::env::var("ANTHROPIC_BASE_URL")
                .unwrap_or_else(|_| "https://api.anthropic.com".to_string()),
            _ => std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com".to_string()),
        }
    }

    pub fn resolve_api_key(&self) -> Option<String> {
        if !self.api_key.is_empty() {
            return Some(self.api_key.clone());
        }
        let var = match self.wire() {
            Ok(Wire::Anthropic) => "ANTHROPIC_API_KEY",
            _ => "OPENAI_API_KEY",
        };
        std::env::var(var).ok().filter(|s| !s.is_empty())
    }

    pub fn resolve_auth_token(&self) -> Option<String> {
        if !self.auth_token.is_empty() {
            return Some(self.auth_token.clone());
        }
        std::env::var("ANTHROPIC_AUTH_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
    }

    /// Whether the picker should ask this endpoint what it serves.
    ///
    /// True for any OpenAI-wire endpoint that is not one of the hosted
    /// catalogues. A self-hosted server's model list is live state — it serves
    /// whatever is loaded — so asking beats writing a model id into config and
    /// watching it go stale. The hosted APIs list hundreds of models and are
    /// not going to surprise you, so they keep the configured id.
    pub fn is_probeable(&self) -> bool {
        if !matches!(self.wire(), Ok(Wire::OpenAi)) {
            return false;
        }
        let url = self.endpoint_url();
        !url.is_empty() && !url.contains("api.openai.com")
    }

    /// Tool round-trips allowed in one turn, with the 0 that a config
    /// predating the field deserializes to treated as "unset".
    pub fn tool_iterations(&self) -> usize {
        if self.max_tool_iterations == 0 {
            DEFAULT_TOOL_ITERATIONS
        } else {
            self.max_tool_iterations
        }
    }

    /// Label for the picker and status bar.
    pub fn label(&self) -> String {
        if self.model.is_empty() {
            self.name.clone()
        } else {
            format!("{} · {}", self.name, self.model)
        }
    }
}

/// Why a turn could not run or did not finish cleanly.
#[derive(Debug)]
pub enum TurnError {
    /// The thread does not fit this backend's window. Surfaced rather than
    /// silently compacted: in a planning thread the early turns are usually
    /// the ones that matter, and quietly eating the design premises is worse
    /// than asking.
    ContextOverflow {
        estimate: usize,
        limit: usize,
        backend: String,
    },
    /// Transport or API failure. The caller keeps the draft and the thread so
    /// the user can switch backend and retry the same message.
    Request(anyhow::Error),
}

impl std::fmt::Display for TurnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TurnError::ContextOverflow {
                estimate,
                limit,
                backend,
            } => write!(
                f,
                "thread is ~{} tokens, over {}'s {} limit — compact, fork, or pick a larger model",
                estimate, backend, limit
            ),
            TurnError::Request(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for TurnError {}

/// Rough token estimate at ~4 characters per token — the same heuristic the
/// orchestrator uses. Good enough to catch an overflow before the request.
pub fn estimate_tokens(messages: &[Message], pending: &str) -> usize {
    let chars: usize = messages.iter().map(|m| m.text.len() + 16).sum::<usize>() + pending.len();
    chars / 4
}

/// Check the thread against a backend's window before sending.
pub fn check_budget(thread: &Thread, backend: &Backend, pending: &str) -> Result<usize, TurnError> {
    let estimate = estimate_tokens(&thread.messages, pending);
    if backend.max_context_tokens > 0 && estimate > backend.max_context_tokens {
        return Err(TurnError::ContextOverflow {
            estimate,
            limit: backend.max_context_tokens,
            backend: backend.label(),
        });
    }
    Ok(estimate)
}

/// Drop oldest turns until the thread fits, always keeping the most recent
/// exchange. Only called on explicit user confirmation after a
/// [`TurnError::ContextOverflow`] — never automatically.
pub fn compact(thread: &mut Thread, backend: &Backend, pending: &str) -> usize {
    let limit = backend.max_context_tokens;
    if limit == 0 {
        return 0;
    }
    let mut dropped = 0;
    while thread.messages.len() > 2 && estimate_tokens(&thread.messages, pending) > limit {
        thread.messages.remove(0);
        dropped += 1;
    }
    dropped
}

const SYSTEM_PROMPT: &str = "\
You are a planning partner inside linkshell, a terminal multiplexer for AI \
coding agents. You are working through a design or plan with an engineer \
before any code is written.

You have read-only access to one directory tree. Use `grep` to locate things \
and `read_file` to read them: a plan grounded in the actual source is worth \
far more than one built from assumptions, so check the code rather than \
guessing at its structure. You cannot modify anything, and no write, edit, or \
shell tool exists for you to reach for.

Be concrete. Name real files, functions, and types you have actually read. \
When you are uncertain whether something exists, look rather than hedge. \
Disagree when you think an approach is wrong, and say what you would do \
instead. Prefer identifying the one decision that matters over enumerating \
every option.";

fn system_prompt(thread: &Thread) -> String {
    let mut p = String::from(SYSTEM_PROMPT);
    p.push_str(&format!(
        "\n\nScope root (all paths resolve against it): {}",
        thread.root.display()
    ));
    // The root is chosen once, when the thread is created, and nothing in the
    // UI can move it afterwards. Without this the model re-opens the question
    // every turn — "this isn't the right directory for that" — which the
    // engineer cannot act on and has already read.
    p.push_str(
        "\nThe root was fixed when this thread was created and cannot be changed, by you \
or by the engineer. Do not assess whether it is the right directory and do not suggest \
moving or re-rooting the work. If something you need falls outside it, say once, in a \
line, what you cannot see, then plan with what is in front of you.",
    );
    // `thread` does not yet include the message being sent, so an empty
    // transcript really does mean this is the opening turn.
    if !thread.messages.is_empty() {
        p.push_str(
            "\n\nThis is a continuing conversation. Everything you have already established — \
scope, caveats, what you can and cannot see, how you read the codebase — stands, and the \
engineer has read it. Do not restate it. Pick up from the transcript and answer what was \
just asked.",
        );
    }
    if !thread.reads.is_empty() {
        let stale = thread.stale_reads();
        if !stale.is_empty() {
            p.push_str(&format!(
                "\n\nThese files changed on disk since this thread read them: {}. \
                 Re-read any of them you intend to rely on.",
                stale.join(", ")
            ));
        }
    }
    p
}

/// Fallback when a backend carries no explicit `max_tool_iterations` (a
/// hand-written config predating the field deserializes it as 0).
const DEFAULT_TOOL_ITERATIONS: usize = 40;

async fn status(tx: &mpsc::Sender<AppEvent>, thread_id: &str, text: impl Into<String>) {
    let _ = tx
        .send(AppEvent::PlanningStatus {
            thread_id: thread_id.to_string(),
            status: text.into(),
        })
        .await;
}

/// Run one planning turn to completion.
///
/// Appends the user message and the assistant reply to `thread` and returns
/// the reply text. Tool results are consumed within the turn and never stored:
/// the thread records only *that* a file was read, so reopening it re-reads
/// current contents instead of carrying a stale snapshot forever.
pub async fn run_turn(
    thread: &mut Thread,
    backend: &Backend,
    user_text: &str,
    client: &reqwest::Client,
    tx: &mpsc::Sender<AppEvent>,
) -> Result<(String, usize), TurnError> {
    check_budget(thread, backend, user_text)?;
    let root = tools::canonical_root(&thread.root).map_err(TurnError::Request)?;

    let wire = backend.wire().map_err(TurnError::Request)?;
    let reply = match wire {
        Wire::Anthropic => run_anthropic(thread, backend, user_text, &root, client, tx).await,
        Wire::OpenAi => run_openai(thread, backend, user_text, &root, client, tx).await,
    };

    match reply {
        Ok(outcome) => {
            for r in outcome.reads {
                thread.record_read(r);
            }
            thread.messages.push(Message::user(user_text));
            thread
                .messages
                .push(Message::assistant(&outcome.text, backend));
            thread.updated = store::now_secs();
            Ok((outcome.text, outcome.peak_tokens))
        }
        // On failure the thread is left exactly as it was, so the caller can
        // keep the user's draft and retry it against a different backend.
        Err(e) => Err(TurnError::Request(e)),
    }
}

/// What one turn actually sent, beyond the reply itself.
pub struct TurnOutcome {
    pub text: String,
    pub reads: Vec<tools::ReadRecord>,
    /// Largest request this turn built, in estimated tokens.
    ///
    /// The persisted thread is a poor proxy for what a turn costs: tool
    /// results are consumed inside the turn and never stored, so a turn that
    /// read 40k of source leaves a transcript of a few hundred tokens. The
    /// meter has to report the peak the request actually reached, or it reads
    /// as "nothing is being tracked" exactly when the window is filling up.
    pub peak_tokens: usize,
}

/// Estimated tokens in a request payload, at the same ~4 chars/token the rest
/// of the module uses. Serializing is the honest measure here: it counts tool
/// results and their JSON envelopes, which is where a code-reading turn's
/// context actually goes.
fn payload_tokens(system: &str, history: &[serde_json::Value]) -> usize {
    let body: usize = history.iter().map(|m| m.to_string().len()).sum::<usize>();
    (system.len() + body) / 4
}

/// Build the wire history from persisted messages. Tool exchanges are absent
/// by construction, which is what keeps threads portable across a 32k local
/// model and a 200k hosted one.
fn base_history(thread: &Thread) -> Vec<serde_json::Value> {
    thread
        .messages
        .iter()
        .map(|m| {
            let role = match m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            serde_json::json!({"role": role, "content": m.text})
        })
        .collect()
}

async fn run_anthropic(
    thread: &Thread,
    backend: &Backend,
    user_text: &str,
    root: &Path,
    client: &reqwest::Client,
    tx: &mpsc::Sender<AppEvent>,
) -> anyhow::Result<TurnOutcome> {
    let url = format!(
        "{}/v1/messages",
        backend.endpoint_url().trim_end_matches('/')
    );
    let mut history = base_history(thread);
    history.push(serde_json::json!({"role": "user", "content": user_text}));

    let tool_defs = tools::anthropic_tools();
    let system = system_prompt(thread);
    let mut reads: Vec<tools::ReadRecord> = Vec::new();
    let mut peak_tokens = 0usize;

    let limit = backend.tool_iterations();
    for i in 0..=limit {
        // The last pass runs without tools: hitting the ceiling should cost
        // the user an answer written from what was already gathered, not the
        // whole turn. Dropping the tools is what ends the loop — a model that
        // cannot call one has to reply.
        let last = i == limit;
        status(
            tx,
            &thread.id,
            if last {
                "wrapping up (tool limit reached)".to_string()
            } else {
                format!("thinking ({}/{})", i + 1, limit)
            },
        )
        .await;
        let mut body = serde_json::json!({
            "model": backend.model,
            "max_tokens": backend.max_tokens,
            "system": system,
            "tools": tool_defs,
            "messages": history,
        });
        if last {
            if let Some(o) = body.as_object_mut() {
                o.remove("tools");
            }
        }
        peak_tokens = peak_tokens.max(payload_tokens(&system, &history));
        let mut req = client
            .post(&url)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .timeout(std::time::Duration::from_secs(600));
        if let Some(token) = backend.resolve_auth_token() {
            req = req.bearer_auth(token);
        } else if let Some(key) = backend.resolve_api_key() {
            req = req.header("x-api-key", key);
        } else {
            anyhow::bail!(
                "no Anthropic credentials for planning backend {} (set ANTHROPIC_API_KEY or \
                 [planning.backends.{}].api_key)",
                backend.name,
                backend.name
            );
        }

        let resp: serde_json::Value = req.send().await?.json().await?;
        if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
            anyhow::bail!("api error: {}", api_error_message(err));
        }

        let content = resp["content"].clone();
        history.push(serde_json::json!({"role": "assistant", "content": content}));

        let text = content
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();

        if resp["stop_reason"] != "tool_use" {
            return Ok(TurnOutcome {
                text,
                reads,
                peak_tokens,
            });
        }

        let mut results: Vec<serde_json::Value> = Vec::new();
        if let Some(blocks) = content.as_array() {
            for b in blocks.iter().filter(|b| b["type"] == "tool_use") {
                let name = b["name"].as_str().unwrap_or("");
                status(
                    tx,
                    &thread.id,
                    format!("{} {}", name, tool_hint(&b["input"])),
                )
                .await;
                let outcome = tools::exec(root, name, &b["input"]);
                if let Some(r) = outcome.read {
                    reads.push(r);
                }
                results.push(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": b["id"],
                    "content": outcome.text,
                }));
            }
        }
        history.push(serde_json::json!({"role": "user", "content": results}));
    }

    // Unreachable: the tool-less final pass cannot return stop_reason
    // "tool_use", so the loop always returns from inside.
    Ok(TurnOutcome {
        text: String::new(),
        reads,
        peak_tokens,
    })
}

async fn run_openai(
    thread: &Thread,
    backend: &Backend,
    user_text: &str,
    root: &Path,
    client: &reqwest::Client,
    tx: &mpsc::Sender<AppEvent>,
) -> anyhow::Result<TurnOutcome> {
    let endpoint = backend.endpoint_url();
    if endpoint.is_empty() {
        anyhow::bail!(
            "no endpoint configured for planning backend {}",
            backend.name
        );
    }
    let url = crate::agent_llm::completions_url(&endpoint);
    let tool_defs = tools::openai_tools();

    let mut history = base_history(thread);
    history.push(serde_json::json!({"role": "user", "content": user_text}));
    let mut reads: Vec<tools::ReadRecord> = Vec::new();
    let mut peak_tokens = 0usize;

    let limit = backend.tool_iterations();
    for i in 0..=limit {
        // See run_anthropic: the final pass drops the tools so the turn ends
        // with an answer rather than a discarded walk of the codebase.
        let last = i == limit;
        status(
            tx,
            &thread.id,
            if last {
                "wrapping up (tool limit reached)".to_string()
            } else {
                format!("thinking ({}/{})", i + 1, limit)
            },
        )
        .await;
        let system = system_prompt(thread);
        let mut messages = vec![serde_json::json!({"role": "system", "content": system})];
        messages.extend(history.iter().cloned());
        peak_tokens = peak_tokens.max(payload_tokens("", &messages));
        let mut body = serde_json::json!({
            "model": backend.model,
            "messages": messages,
            "tools": tool_defs,
            "tool_choice": "auto",
        });
        if last {
            if let Some(o) = body.as_object_mut() {
                o.remove("tools");
                o.remove("tool_choice");
            }
        }
        let mut req = client
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(600));
        if let Some(key) = backend.resolve_api_key() {
            req = req.bearer_auth(key);
        }

        let resp: serde_json::Value = req.send().await?.json().await?;
        if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
            anyhow::bail!("api error: {}", api_error_message(err));
        }
        let message = resp["choices"][0]["message"].clone();
        if message.is_null() {
            anyhow::bail!("no choices in response");
        }
        history.push(message.clone());

        let calls = message["tool_calls"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if calls.is_empty() {
            return Ok(TurnOutcome {
                text: message["content"].as_str().unwrap_or("").to_string(),
                reads,
                peak_tokens,
            });
        }
        for call in &calls {
            let name = call["function"]["name"].as_str().unwrap_or("");
            // `arguments` is a JSON-encoded string per the OpenAI spec. Local
            // models get this wrong often enough that a parse failure must
            // become a tool error the model can recover from, not a dead turn.
            let args: serde_json::Value = call["function"]["arguments"]
                .as_str()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::json!({}));
            status(tx, &thread.id, format!("{} {}", name, tool_hint(&args))).await;
            let outcome = tools::exec(root, name, &args);
            if let Some(r) = outcome.read {
                reads.push(r);
            }
            history.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": call["id"],
                "content": outcome.text,
            }));
        }
    }

    // Unreachable: with no tools in the request there are no tool_calls to
    // dispatch, so the final pass returns from inside the loop.
    Ok(TurnOutcome {
        text: String::new(),
        reads,
        peak_tokens,
    })
}

/// Human-readable text out of an `error` field, whatever shape it arrived in.
///
/// Not every server wraps it the same way: OpenAI and Anthropic send
/// `{"error": {"message": ...}}`, while LM Studio and llama.cpp often send a
/// bare string. Reaching only for `["message"]` turned the second case into
/// "api error: unknown" — a failure report that says nothing, on the one class
/// of backend whose failures you are most likely to have to debug.
fn api_error_message(err: &serde_json::Value) -> String {
    if let Some(s) = err.as_str() {
        return s.to_string();
    }
    if let Some(s) = err["message"].as_str() {
        return s.to_string();
    }
    let raw = err.to_string();
    raw.chars().take(300).collect()
}

/// Short description of a tool call for the status line.
fn tool_hint(args: &serde_json::Value) -> String {
    args.get("path")
        .or_else(|| args.get("pattern"))
        .and_then(|v| v.as_str())
        .map(|s| s.chars().take(48).collect())
        .unwrap_or_default()
}

/// Spawn a turn on a background task, delivering the result as an
/// [`AppEvent`]. Mirrors `agent_llm::spawn_chat_request` so the app loop
/// stays uniform.
pub fn spawn_turn(
    mut thread: Thread,
    backend: Backend,
    user_text: String,
    tx: mpsc::Sender<AppEvent>,
) {
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let event = match run_turn(&mut thread, &backend, &user_text, &client, &tx).await {
            Ok((text, peak_tokens)) => {
                // Persist before notifying: if the write fails the user should
                // hear about it now, not discover it on reopen.
                let save_err = store::save(&thread).err().map(|e| e.to_string());
                AppEvent::PlanningReply {
                    thread_id: thread.id.clone(),
                    text,
                    peak_tokens,
                    backend: backend.name.clone(),
                    model: backend.model.clone(),
                    save_error: save_err,
                }
            }
            Err(e) => AppEvent::PlanningFailed {
                thread_id: thread.id.clone(),
                // The draft rides back with the failure so the pane can restore
                // it into the input box for a retry on another backend.
                draft: user_text.clone(),
                error: e.to_string(),
                overflow: matches!(e, TurnError::ContextOverflow { .. }),
            },
        };
        let _ = tx.send(event).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn backend(name: &str, provider: &str, ctx: usize) -> Backend {
        Backend {
            name: name.to_string(),
            provider: provider.to_string(),
            model: "m".to_string(),
            max_context_tokens: ctx,
            ..Backend::default()
        }
    }

    /// The persisted transcript is not a measure of what a turn cost: tool
    /// results are consumed inside the turn, so a turn that read a lot of
    /// source leaves almost no trace in `estimate_tokens`.
    #[test]
    fn tool_traffic_is_invisible_to_the_transcript_estimate() {
        let mut t = Thread::new("t", PathBuf::from("/tmp"));
        t.messages.push(Message::user("read the whole module"));
        t.messages.push(Message::assistant(
            "Done.",
            &backend("local", "lmstudio", 0),
        ));
        let transcript = estimate_tokens(&t.messages, "");
        assert!(transcript < 100, "a short transcript, got {transcript}");

        // The same turn's request, with one file's contents in a tool result.
        let history = vec![
            serde_json::json!({"role": "user", "content": "read the whole module"}),
            serde_json::json!({"role": "tool", "content": "x".repeat(40_000)}),
        ];
        let payload = payload_tokens("system prompt", &history);
        assert!(
            payload > 9_000,
            "the file contents dominate the request, got {payload}"
        );
    }

    /// A local server's error is usually a bare string; reaching only for
    /// ["message"] reported "unknown" and lost the only diagnostic there was.
    #[test]
    fn api_errors_are_readable_whatever_shape_they_arrive_in() {
        use serde_json::json;
        assert_eq!(
            api_error_message(&json!({"message": "context length exceeded"})),
            "context length exceeded"
        );
        assert_eq!(
            api_error_message(&json!("model does not support tools")),
            "model does not support tools"
        );
        // No recognized shape: show the payload rather than the word "unknown".
        let odd = api_error_message(&json!({"code": 500, "detail": "boom"}));
        assert!(odd.contains("boom"), "got {odd}");
    }

    /// Grounding a plan in a codebase is a grep-read-grep-read walk; ten hops
    /// is a couple of files. The default has to be a runaway bound, not a
    /// budget the model is expected to finish inside.
    #[test]
    fn the_tool_iteration_default_leaves_room_to_read_a_codebase() {
        assert_eq!(Backend::default().tool_iterations(), 40);
    }

    /// A config written before the field existed deserializes it as 0, which
    /// would otherwise mean "no tool calls at all".
    #[test]
    fn a_zero_iteration_limit_means_unset_not_zero() {
        let b: Backend = toml::from_str("provider = \"anthropic\"\nmodel = \"m\"\n").unwrap();
        assert_eq!(b.max_tool_iterations, 40, "serde(default) fills the field");
        let explicit_zero = Backend {
            max_tool_iterations: 0,
            ..Backend::default()
        };
        assert_eq!(explicit_zero.tool_iterations(), DEFAULT_TOOL_ITERATIONS);
    }

    #[test]
    fn provider_names_map_to_the_right_wire_protocol() {
        assert_eq!(
            backend("a", "anthropic", 0).wire().unwrap(),
            Wire::Anthropic
        );
        for p in ["openai", "lmstudio", "llamacpp", "ollama", "vllm"] {
            assert_eq!(backend("x", p, 0).wire().unwrap(), Wire::OpenAi, "{}", p);
        }
        assert!(backend("x", "nonsense", 0).wire().is_err());
    }

    #[test]
    fn overflow_is_reported_rather_than_silently_compacted() {
        let mut t = Thread::new("t", PathBuf::from("/tmp"));
        for _ in 0..40 {
            t.messages.push(Message::user("x".repeat(1000)));
        }
        let small = backend("local", "lmstudio", 1_000);
        let err = check_budget(&t, &small, "next").unwrap_err();
        match err {
            TurnError::ContextOverflow { limit, backend, .. } => {
                assert_eq!(limit, 1_000);
                assert!(backend.contains("local"));
            }
            other => panic!("expected overflow, got {:?}", other),
        }
        // The same thread is fine on a larger window — switching model
        // re-evaluates the budget rather than carrying a fixed verdict.
        let big = backend("opus", "anthropic", 200_000);
        assert!(check_budget(&t, &big, "next").is_ok());
        // And nothing was dropped as a side effect of checking.
        assert_eq!(t.messages.len(), 40);
    }

    #[test]
    fn compact_only_runs_when_asked_and_keeps_the_latest_exchange() {
        let mut t = Thread::new("t", PathBuf::from("/tmp"));
        for i in 0..20 {
            t.messages
                .push(Message::user(format!("{}{}", i, "x".repeat(500))));
        }
        let b = backend("local", "lmstudio", 1_000);
        let dropped = compact(&mut t, &b, "");
        assert!(dropped > 0);
        assert!(t.messages.len() >= 2, "never compacts below one exchange");
        assert!(
            t.messages.last().unwrap().text.starts_with("19"),
            "the newest turn survives"
        );
    }

    #[test]
    fn endpoint_defaults_by_provider_and_config_wins() {
        let mut b = backend("x", "anthropic", 0);
        assert!(b.endpoint_url().contains("anthropic.com"));
        b.endpoint = "http://localhost:1234".to_string();
        assert_eq!(b.endpoint_url(), "http://localhost:1234");
    }

    #[test]
    fn history_carries_no_tool_traffic() {
        let mut t = Thread::new("t", PathBuf::from("/tmp"));
        t.messages.push(Message::user("read src/lib.rs"));
        t.messages.push(Message::assistant(
            "It defines two functions.",
            &backend("local", "lmstudio", 0),
        ));
        let h = base_history(&t);
        assert_eq!(h.len(), 2);
        assert!(h.iter().all(|m| m["role"] != "tool"));
        assert_eq!(h[1]["content"], "It defines two functions.");
    }

    #[test]
    fn system_prompt_pins_the_root_and_flags_stale_grounding() {
        let mut t = Thread::new("t", PathBuf::from("/tmp/repo"));
        assert!(system_prompt(&t).contains("/tmp/repo"));
        // The root cannot be moved, so re-litigating it wastes the turn.
        assert!(system_prompt(&t).contains("cannot be changed"));
        t.record_read(tools::ReadRecord {
            rel: "vanished.rs".to_string(),
            hash: 7,
            mtime: None,
        });
        let p = system_prompt(&t);
        assert!(p.contains("vanished.rs"), "stale reads are surfaced: {}", p);
    }

    #[test]
    fn a_continuing_thread_is_told_not_to_restate_what_it_already_said() {
        // The opening turn has nothing to repeat yet.
        let mut t = Thread::new("t", PathBuf::from("/tmp/repo"));
        assert!(!system_prompt(&t).contains("continuing conversation"));

        // `thread` never carries the message being sent, so one prior
        // exchange is what makes the next turn a continuation.
        t.messages.push(Message::user("how should retries work"));
        t.messages.push(Message::assistant(
            "Back off exponentially.",
            &Backend::default(),
        ));
        assert!(system_prompt(&t).contains("continuing conversation"));
    }
}
