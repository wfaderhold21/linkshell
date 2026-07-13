//! OpenAI-compatible chat-completions tool loop for the orchestrator agent.
//! Serves both `provider = "openai"` and `provider = "lmstudio"`.

use crate::config::OrchestratorConfig;
use crate::events::AppEvent;
use tokio::sync::mpsc;

pub async fn run_turn(
    cfg: &OrchestratorConfig,
    client: &reqwest::Client,
    history: &mut Vec<serde_json::Value>,
    user_text: &str,
    event_tx: &mpsc::Sender<AppEvent>,
) -> anyhow::Result<String> {
    let endpoint = cfg.endpoint_url();
    if endpoint.is_empty() {
        anyhow::bail!("no endpoint configured for provider {}", cfg.provider);
    }
    let url = crate::agent_llm::completions_url(&endpoint);
    let tools = super::openai_tools();

    history.push(serde_json::json!({"role": "user", "content": user_text}));
    super::trim_history(history, cfg.max_history_turns);

    for _ in 0..cfg.max_tool_iterations {
        // System prompt is prepended per request so history stays trimmable.
        let mut messages =
            vec![serde_json::json!({"role": "system", "content": super::system_prompt(cfg)})];
        messages.extend(history.iter().cloned());
        let body = serde_json::json!({
            "model": cfg.model_id(),
            "messages": messages,
            "tools": tools,
            "tool_choice": "auto",
        });
        let mut req = client
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(
                cfg.input_wait_timeout_secs + 120,
            ));
        if let Some(key) = cfg.resolve_api_key() {
            req = req.bearer_auth(key);
        }
        let resp: serde_json::Value = req.send().await?.json().await?;

        if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
            anyhow::bail!(
                "api error: {}",
                err["message"].as_str().unwrap_or("unknown")
            );
        }
        let input = resp["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        let output = resp["usage"]["completion_tokens"].as_u64().unwrap_or(0);
        let _ = event_tx
            .send(AppEvent::OrchestratorUsage { input, output })
            .await;

        let message = resp["choices"][0]["message"].clone();
        if message.is_null() {
            anyhow::bail!("no choices in response");
        }
        history.push(message.clone());

        let tool_calls = message["tool_calls"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if tool_calls.is_empty() {
            return Ok(message["content"].as_str().unwrap_or("").to_string());
        }
        for call in &tool_calls {
            let name = call["function"]["name"].as_str().unwrap_or("");
            // `arguments` is a JSON-encoded string per the OpenAI spec.
            let args: serde_json::Value = call["function"]["arguments"]
                .as_str()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::json!({}));
            let result = super::exec_tool(cfg, event_tx, name, &args).await;
            history.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": call["id"],
                "content": result,
            }));
        }
    }
    Ok("[tool iteration limit reached]".to_string())
}
