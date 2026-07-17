# Linkshell recipes

## Reviewer pipe

```text
new codex implementer
new claude reviewer
pipe implementer reviewer --on=ready --extract=last-n=40 --prefix="Review this:"
```

Open `pipes` to inspect, pause, fire, or delete the route. Session names and
numbers are both accepted.

## Council quickstart

Run the shipped bounded author/critic loop:

```bash
linkshell --council examples/council.toml
```

Use `council status` or `council stop` at runtime. Copy the example to change
commands, routes, joins, and round limits.

## Remote agent

Start `linkshell --tcp 7373` on the compute node. TCP clients require a
capability token. Prefer tunneling the listener:

```bash
ssh -L 7373:127.0.0.1:7373 compute-node
```

Authenticated environments can use `linkshell-ctl list`, `state`, and `send`
through the same protocol as local agents.

## Local LLM

Configure an OpenAI-compatible endpoint:

```toml
[agents.qwen]
endpoint = "http://localhost:8080/v1"
model = "qwen3"
system = "Be concise and identify concrete defects."
```

Address it from agent chat with `@qwen review this result`. `llama-cli`,
`ollama`, `aider`, and `opencode` can also run as custom sessions.


## Propose mode (trust calibration for local orchestrators)

Run the orchestrator with a leash while you calibrate trust in a new model:

```toml
[orchestrator]
enabled = true
provider = "openai"            # e.g. LM Studio / llama.cpp server
endpoint = "http://localhost:1234/v1"
model = "qwen3.6-27b"
approval = "propose"
max_tool_iterations = 24       # local models take smaller steps
```

The agent observes freely (`read_output`, `list_sessions`, `use_skill` are
auto-approved) but every `start_session` and `send_input` lands in the chat
pane as a proposal you answer with `/approve` or `/deny [reason]`. Deny
reasons go back to the model as the tool result, so "wrong session, use 3"
steers it mid-turn. Your approval history is the eval: once the proposals are
consistently sensible, flip `approval = "auto"` — or keep the gate and grow
`auto_approve` tool by tool.
