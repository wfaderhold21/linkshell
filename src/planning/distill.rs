//! Committing a thread: distilling a conversation into a usable plan.
//!
//! A planning thread is a conversation — digressions, corrections, abandoned
//! branches. What an implementation session needs is the conclusion. The
//! distiller is a separate one-shot call over the whole transcript that
//! produces that artifact.
//!
//! The distiller backend is chosen independently of the thread backend, and
//! that is the point: a thread can be built cheaply on a local model while
//! the distillation — the part that actually gets handed to an agent — gets
//! the frontier model. It is one call over an existing transcript, so it is
//! the cheapest place in the pipeline to spend good tokens.
//!
//! Plans are written into linkshell's own store, never into the workspace.
//! The workspace is read-only for the whole planning flow; a commit step that
//! wrote a file into the repository would punch a hole straight through that
//! guarantee, and would leave an artifact every project had to gitignore.
//! Exporting a plan into a repo is a separate, deliberate user action.

use super::store::{self, PlanRevision, Role, Thread};
use super::{Backend, TurnError, Wire};
use crate::events::AppEvent;
use tokio::sync::mpsc;

const DISTILL_PROMPT: &str = "\
Below is a planning conversation between an engineer and a planning agent. \
Distill it into a single implementation plan that a coding agent can execute \
without having read the conversation.

Rules:
- Include only what was actually decided. Drop digressions, abandoned \
approaches, and anything left open — unless a decision explicitly depends on \
it, in which case state it as an open question at the end.
- Where the conversation names real files, functions, or types, keep those \
names exactly. They are the plan's anchors.
- Where the conversation gave a reason for a decision, keep the reason. An \
agent that knows why a constraint exists will not quietly violate it.
- Do not invent detail that was not discussed. If the plan is thin in a \
place, let it be thin.

Structure the output as markdown: a short summary, then the concrete changes \
in the order they should be made, then open questions if any remain. Output \
only the plan itself.";

/// Render a thread as a plain transcript for the distiller.
fn transcript(thread: &Thread) -> String {
    let mut out = String::new();
    for m in &thread.messages {
        let who = match m.role {
            Role::User => "ENGINEER",
            Role::Assistant => "PLANNER",
        };
        out.push_str(&format!("--- {} ---\n{}\n\n", who, m.text.trim()));
    }
    out
}

/// Run the distiller and write a new plan revision.
///
/// Returns the revision written. The thread's `revisions` counter is bumped
/// so the next commit lands beside this one rather than on top of it —
/// revisions are diffable, and comparing two distillations is most of the
/// value of committing more than once.
pub async fn commit(
    thread: &mut Thread,
    distiller: &Backend,
    client: &reqwest::Client,
) -> Result<PlanRevision, TurnError> {
    if thread.messages.is_empty() {
        return Err(TurnError::Request(anyhow::anyhow!(
            "nothing to distill: the thread is empty"
        )));
    }

    let body_text = transcript(thread);
    let prompt = format!("{}\n\n{}", DISTILL_PROMPT, body_text);

    // The distiller sees the whole transcript in one shot, so it is the most
    // likely place to overflow a small local window — check before sending.
    let estimate = prompt.len() / 4;
    if distiller.max_context_tokens > 0 && estimate > distiller.max_context_tokens {
        return Err(TurnError::ContextOverflow {
            estimate,
            limit: distiller.max_context_tokens,
            backend: distiller.label(),
        });
    }

    let text = match distiller.wire().map_err(TurnError::Request)? {
        Wire::Anthropic => distill_anthropic(distiller, &prompt, client).await,
        Wire::OpenAi => distill_openai(distiller, &prompt, client).await,
    }
    .map_err(TurnError::Request)?;

    if text.trim().is_empty() {
        return Err(TurnError::Request(anyhow::anyhow!(
            "distiller returned an empty plan"
        )));
    }

    let revision = store::write_plan(thread, &text, distiller).map_err(TurnError::Request)?;
    thread.revisions = revision.revision;
    thread.updated = store::now_secs();
    store::save(thread).map_err(TurnError::Request)?;
    Ok(revision)
}

async fn distill_anthropic(
    backend: &Backend,
    prompt: &str,
    client: &reqwest::Client,
) -> anyhow::Result<String> {
    let url = format!(
        "{}/v1/messages",
        backend.endpoint_url().trim_end_matches('/')
    );
    let body = serde_json::json!({
        "model": backend.model,
        "max_tokens": backend.max_tokens.max(4096),
        "messages": [{"role": "user", "content": prompt}],
    });
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
        anyhow::bail!("no Anthropic credentials for distiller {}", backend.name);
    }
    let resp: serde_json::Value = req.send().await?.json().await?;
    if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
        anyhow::bail!(
            "api error: {}",
            err["message"].as_str().unwrap_or("unknown")
        );
    }
    Ok(resp["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default())
}

async fn distill_openai(
    backend: &Backend,
    prompt: &str,
    client: &reqwest::Client,
) -> anyhow::Result<String> {
    let url = crate::agent_llm::completions_url(&backend.endpoint_url());
    let body = serde_json::json!({
        "model": backend.model,
        "messages": [{"role": "user", "content": prompt}],
    });
    let mut req = client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(600));
    if let Some(key) = backend.resolve_api_key() {
        req = req.bearer_auth(key);
    }
    let resp: serde_json::Value = req.send().await?.json().await?;
    if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
        anyhow::bail!(
            "api error: {}",
            err["message"].as_str().unwrap_or("unknown")
        );
    }
    Ok(resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_string())
}

/// Spawn a commit on a background task.
pub fn spawn_commit(mut thread: Thread, distiller: Backend, tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let event = match commit(&mut thread, &distiller, &client).await {
            Ok(rev) => AppEvent::PlanningCommitted {
                thread_id: thread.id.clone(),
                path: rev.path.to_string_lossy().to_string(),
                revision: rev.revision,
                stale: thread.stale_reads(),
            },
            Err(e) => AppEvent::PlanningFailed {
                thread_id: thread.id.clone(),
                draft: String::new(),
                error: format!("commit failed: {}", e),
                overflow: matches!(e, TurnError::ContextOverflow { .. }),
            },
        };
        let _ = tx.send(event).await;
    });
}

/// Brief handed to an implementation session when a plan is opened as work.
///
/// The contract is deliberately just a file path: an implementation session
/// runs as a subprocess under bwrap, and a single read-only bind mount of the
/// plan file is far simpler to arrange than serializing a thread into a
/// prompt.
pub fn session_brief(plan_path: &std::path::Path, stale: &[String]) -> String {
    let mut brief = format!(
        "Implement the plan at {}. Read it first, in full, before making any change.",
        plan_path.display()
    );
    if !stale.is_empty() {
        brief.push_str(&format!(
            "\n\nNote: these files changed after the plan was written, so parts of it may be \
             out of date — verify them against the current source before following it: {}.",
            stale.join(", ")
        ));
    }
    brief
}

#[cfg(test)]
mod tests {
    use super::super::store::Message;
    use super::*;
    use std::path::PathBuf;

    fn backend() -> Backend {
        Backend {
            name: "opus".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-opus-4".to_string(),
            max_context_tokens: 200_000,
            ..Backend::default()
        }
    }

    fn thread() -> Thread {
        let mut t = Thread::new("planning pane", PathBuf::from("/tmp/repo"));
        t.messages.push(Message::user("Where does layout live?"));
        t.messages.push(Message::assistant(
            "src/layout.rs, a binary tree.",
            &backend(),
        ));
        t
    }

    #[test]
    fn transcript_labels_both_speakers() {
        let t = transcript(&thread());
        assert!(t.contains("--- ENGINEER ---"));
        assert!(t.contains("--- PLANNER ---"));
        assert!(t.contains("src/layout.rs"));
    }

    #[tokio::test]
    async fn empty_threads_are_rejected_before_any_request() {
        let mut t = Thread::new("empty", PathBuf::from("/tmp/repo"));
        let client = reqwest::Client::new();
        let err = commit(&mut t, &backend(), &client).await.unwrap_err();
        assert!(err.to_string().contains("nothing to distill"));
    }

    #[tokio::test]
    async fn oversized_transcripts_fail_the_budget_check_not_the_network() {
        let mut t = thread();
        t.messages.push(Message::user("x".repeat(100_000)));
        let small = Backend {
            name: "local".to_string(),
            provider: "lmstudio".to_string(),
            endpoint: "http://127.0.0.1:1".to_string(),
            model: "qwen".to_string(),
            max_context_tokens: 1_000,
            ..Backend::default()
        };
        let client = reqwest::Client::new();
        let err = commit(&mut t, &small, &client).await.unwrap_err();
        assert!(matches!(err, TurnError::ContextOverflow { .. }));
        assert!(err.to_string().contains("larger model"));
    }

    #[test]
    fn session_brief_is_a_path_plus_a_staleness_warning() {
        let p = PathBuf::from("/home/u/.local/share/linkshell/planning/plans/t/001.md");
        let plain = session_brief(&p, &[]);
        assert!(plain.contains("001.md"));
        assert!(!plain.contains("out of date"));

        let warned = session_brief(&p, &["src/layout.rs".to_string()]);
        assert!(warned.contains("src/layout.rs"));
        assert!(warned.contains("out of date"));
    }
}
