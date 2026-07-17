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

    for i in 0..cfg.max_tool_iterations {
        super::send_status(
            event_tx,
            format!("thinking ({}/{})", i + 1, cfg.max_tool_iterations),
        )
        .await;
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
            return Ok(text);
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

    // Tool-iteration budget exhausted with the model still mid-task: give it
    // one final tool-less turn to summarize its progress, so partial work
    // reaches the user instead of "[tool iteration limit reached]". The nudge
    // rides in the pending tool_results user turn (roles must alternate) and
    // tool_choice "none" hard-blocks further calls; `tools` stays in the body
    // because the API requires definitions while tool blocks are in history.
    super::send_status(event_tx, "summarizing (budget exhausted)").await;
    if let Some(serde_json::Value::Array(blocks)) =
        history.last_mut().map(|m| &mut m["content"])
    {
        blocks.push(serde_json::json!({"type": "text", "text": super::EXHAUSTION_NUDGE}));
    }
    let body = serde_json::json!({
        "model": cfg.model_id(),
        "max_tokens": cfg.max_tokens,
        "system": system,
        "tools": tools,
        "tool_choice": {"type": "none"},
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
    history.push(serde_json::json!({"role": "assistant", "content": content}));
    let summary = content
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
    Ok(if summary.is_empty() {
        "[tool iteration limit reached]".to_string()
    } else {
        format!("{}\n\n[tool iteration limit reached — partial progress above]", summary)
    })
}
