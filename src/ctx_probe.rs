/// Probe a local model backend for the loaded model's context window size.
///
/// The backend is chosen from what we already know about the session — the
/// command base name at spawn time (llama-cli/llama-server → llama.cpp) or
/// the providerID OpenCode records in its DB (→ LM Studio / llama.cpp) — so
/// only the matching endpoint is polled:
///   - LM Studio's REST API: GET /api/v0/models — loaded models report
///     `loaded_context_length` (falling back to `max_context_length`).
///   - llama-server: GET /props — `default_generation_settings.n_ctx`.
///
/// Polls slowly and fails silently — if the backend isn't running yet the
/// task keeps retrying at a low rate, since it may be started (or a
/// different model loaded) later.
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;

use crate::events::AppEvent;

const LMSTUDIO_URL: &str = "http://127.0.0.1:1234/api/v0/models";
const LLAMA_SERVER_URL: &str = "http://127.0.0.1:8080/props";
const POLL_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Backend {
    LmStudio,
    LlamaServer,
}

/// Map a provider identifier (OpenCode providerID, config value) to a
/// probeable backend. Cloud providers and ones without a context endpoint
/// (ollama manages n_ctx per request) return None.
pub fn backend_for_provider(provider: &str) -> Option<Backend> {
    match provider {
        "lmstudio" => Some(Backend::LmStudio),
        "llamacpp" | "llama.cpp" | "llama" | "llama-server" => Some(Backend::LlamaServer),
        _ => None,
    }
}

/// Backend implied by the session's command itself (no provider indirection).
pub fn backend_for_command(base_name: &str) -> Option<Backend> {
    match base_name {
        "llama-cli" | "llama" | "llama-server" => Some(Backend::LlamaServer),
        "lms" => Some(Backend::LmStudio),
        _ => None,
    }
}

pub fn spawn_probe(session_id: usize, backend: Backend, tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut last_sent: u64 = 0;
        loop {
            let max = match backend {
                Backend::LmStudio => probe_lmstudio(&client).await,
                Backend::LlamaServer => probe_llama_server(&client).await,
            };
            if let Some(max) = max {
                if max > 0 && max != last_sent {
                    if tx
                        .send(AppEvent::SessionContextMax { session_id, max })
                        .await
                        .is_err()
                    {
                        return; // main loop gone
                    }
                    last_sent = max;
                }
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

async fn get_json(client: &reqwest::Client, url: &str) -> Option<Value> {
    client.get(url).send().await.ok()?.json().await.ok()
}

/// LM Studio: pick the first loaded model's context length. When several
/// models are loaded there is no reliable way to know which one the agent
/// uses, so first-loaded is the best available guess.
async fn probe_lmstudio(client: &reqwest::Client) -> Option<u64> {
    let v = get_json(client, LMSTUDIO_URL).await?;
    v["data"].as_array()?.iter().find_map(|m| {
        if m["state"].as_str() != Some("loaded") {
            return None;
        }
        m["loaded_context_length"]
            .as_u64()
            .or_else(|| m["max_context_length"].as_u64())
    })
}

async fn probe_llama_server(client: &reqwest::Client) -> Option<u64> {
    let v = get_json(client, LLAMA_SERVER_URL).await?;
    v["default_generation_settings"]["n_ctx"].as_u64()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_and_command_map_to_expected_backends() {
        assert_eq!(backend_for_provider("lmstudio"), Some(Backend::LmStudio));
        assert_eq!(backend_for_provider("llamacpp"), Some(Backend::LlamaServer));
        assert_eq!(backend_for_provider("anthropic"), None);
        assert_eq!(backend_for_provider("ollama"), None);
        assert_eq!(backend_for_command("llama-cli"), Some(Backend::LlamaServer));
        assert_eq!(backend_for_command("opencode"), None);
    }
}
