//! Anthropic Messages API tool-use loop for the orchestrator agent.

use crate::config::OrchestratorConfig;
use crate::events::AppEvent;
use tokio::sync::mpsc;

/// Run one conversation turn: append the user text, loop through tool calls,
/// return the final assistant text. History uses the Messages API shape and
/// assistant content (including thinking blocks) is replayed verbatim.
pub async fn run_turn(
    cfg: &OrchestratorConfig,
    client: &reqwest::Client,
    history: &mut Vec<serde_json::Value>,
    user_text: &str,
    event_tx: &mpsc::Sender<AppEvent>,
) -> anyhow::Result<String> {
    let auth = cfg.resolve_anthropic_auth().ok_or_else(|| {
        anyhow::anyhow!(
            "no Anthropic credentials (set ANTHROPIC_AUTH_TOKEN / ANTHROPIC_API_KEY or \
             [orchestrator].auth_token / api_key)"
        )
    })?;
    let url = format!("{}/v1/messages", cfg.endpoint_url().trim_end_matches('/'));
    let tools = super::anthropic_tools();
    let system = super::system_prompt(cfg);

    history.push(serde_json::json!({"role": "user", "content": user_text}));
    super::trim_history(history, cfg.max_history_turns);

    let mut final_text = String::new();
    for _ in 0..cfg.max_tool_iterations {
        let body = serde_json::json!({
            "model": cfg.model_id(),
            "max_tokens": cfg.max_tokens,
            "system": system,
            "tools": tools,
            "messages": history,
        });
        let resp: serde_json::Value = auth
            .apply(client.post(&url))
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .timeout(std::time::Duration::from_secs(
                cfg.input_wait_timeout_secs + 120,
            ))
            .send()
            .await?
            .json()
            .await?;

        if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
            anyhow::bail!(
                "api error: {}",
                err["message"].as_str().unwrap_or("unknown")
            );
        }
        let input = resp["usage"]["input_tokens"].as_u64().unwrap_or(0);
        let output = resp["usage"]["output_tokens"].as_u64().unwrap_or(0);
        let _ = event_tx
            .send(AppEvent::OrchestratorUsage { input, output })
            .await;

        let content = resp["content"].clone();
        // Replay assistant content verbatim (keeps thinking blocks intact).
        history.push(serde_json::json!({"role": "assistant", "content": content}));

        final_text = content
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
            return Ok(final_text);
        }

        let mut results: Vec<serde_json::Value> = Vec::new();
        if let Some(blocks) = content.as_array() {
            for b in blocks.iter().filter(|b| b["type"] == "tool_use") {
                let name = b["name"].as_str().unwrap_or("");
                let result = super::exec_tool(cfg, event_tx, name, &b["input"]).await;
                results.push(serde_json::json!({
                    "type": "tool_result",
                    "tool_use_id": b["id"],
                    "content": result,
                }));
            }
        }
        history.push(serde_json::json!({"role": "user", "content": results}));
    }
    // Tool-iteration budget exhausted; whatever text we have is the answer.
    Ok(if final_text.is_empty() {
        "[tool iteration limit reached]".to_string()
    } else {
        final_text
    })
}
