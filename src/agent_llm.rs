//! Chat-addressable local LLM agents.
//!
//! Talks to any OpenAI-compatible `/v1/chat/completions` endpoint —
//! llama.cpp server, Ollama, vLLM, LM Studio, etc. — configured under
//! `[agents.NAME]` in linkshell.toml. Requests run on background tasks and
//! deliver replies as `AppEvent::ChatReply`.

use crate::config::LocalAgent;
use crate::events::AppEvent;
use tokio::sync::mpsc;

/// Build the chat-completions URL from a configured endpoint, accepting the
/// base with or without a trailing `/v1` (or slash).
pub fn completions_url(endpoint: &str) -> String {
    let base = endpoint.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{}/chat/completions", base)
    } else if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{}/v1/chat/completions", base)
    }
}

/// Fire a chat request in the background. `history` is (role, content) pairs,
/// oldest first, not including the system prompt (added from config here).
pub fn spawn_chat_request(
    name: String,
    agent: LocalAgent,
    history: Vec<(String, String)>,
    tx: mpsc::Sender<AppEvent>,
) {
    tokio::spawn(async move {
        let mut messages: Vec<serde_json::Value> = Vec::new();
        if let Some(system) = &agent.system {
            messages.push(serde_json::json!({"role": "system", "content": system}));
        }
        for (role, content) in &history {
            messages.push(serde_json::json!({"role": role, "content": content}));
        }

        let url = completions_url(&agent.endpoint);
        let client = reqwest::Client::new();
        let mut req = client
            .post(&url)
            .json(&serde_json::json!({
                "model": agent.model,
                "messages": messages,
            }))
            .timeout(std::time::Duration::from_secs(600));
        if let Some(key) = &agent.api_key {
            req = req.bearer_auth(key);
        }

        let text = match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                match resp.json::<serde_json::Value>().await {
                    Ok(body) => body["choices"][0]["message"]["content"]
                        .as_str()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            format!(
                                "[{}: unexpected response ({}): {}]",
                                name,
                                status,
                                truncate(&body.to_string(), 200)
                            )
                        }),
                    Err(e) => format!("[{}: bad response: {}]", name, e),
                }
            }
            Err(e) => format!("[{}: request failed: {}]", name, e),
        };

        let _ = tx.send(AppEvent::ChatReply { from: name, text }).await;
    });
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_url_handles_all_endpoint_spellings() {
        assert_eq!(
            completions_url("http://localhost:8080"),
            "http://localhost:8080/v1/chat/completions"
        );
        assert_eq!(
            completions_url("http://localhost:11434/v1"),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            completions_url("http://localhost:1234/v1/"),
            "http://localhost:1234/v1/chat/completions"
        );
        assert_eq!(
            completions_url("http://h/v1/chat/completions"),
            "http://h/v1/chat/completions"
        );
    }
}
