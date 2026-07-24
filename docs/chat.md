# Agent Chat

Press `alt-t` for a chat pane that talks to everything linkshell manages —
council members, individual sessions, and configured local LLMs — without
switching panes:

```
@critic what did you find?      address a session by name (or @2 by number)
@qwen summarize this diff       address a local LLM from [agents.*]
@all status update please       broadcast to every AI session
looks good, continue            bare messages go to the last target
/new claude worker              any command-bar command works with /
/yes  /no                       answer a pending permission prompt
/agents                         list everyone you can talk to
```

Messages to sessions are injected into their PTY; when the session returns to
READY its answer is extracted (last code block, falling back to recent lines)
back into the transcript. Local LLM agents keep a bounded per-agent conversation
history.

The transcript scrolls with the mouse wheel or `PageUp`/`PageDown` (a marker on
the input separator shows how far up you are). Drag to select transcript text —
it is copied to the clipboard on release, like the session panes. Pasting into
the chat input works too; multi-line pastes are delivered to sessions via
bracketed paste so they arrive as one message. Dock the pane with `alt-g`.

The input supports the usual line-editing keys (`Home`/`End`/`Delete` alongside
arrows and backspace). `Up`/`Down` recall previously sent messages, restoring
any in-progress draft when you scroll back down. Typing `/` as the first
character opens a command popup that narrows as you type — `Up`/`Down` pick an
entry, `Tab` completes it, `Enter` sends.

## Answering permission prompts

When an AI session stops on a permission dialog or y/n question, the prompt is
posted into the chat transcript. `/yes` and `/no` answer the most recent request
with the CLI's own keys (claude: `1`/Esc, codex: `y`/`n`); use `/yes <session>`
or `/no <session>` to target a specific one, or type anything else with
`@name <text>`.

## Local LLM agents

Local LLM agents are any OpenAI-compatible endpoint — llama.cpp server, Ollama,
vLLM, LM Studio:

```toml
[agents.qwen]
endpoint = "http://localhost:8080/v1"   # /v1 optional
model = "qwen3.6-27b"
system = "You are a concise coding assistant."
# api_key = "..."                       # sent as Bearer if set
```

## Orchestration pattern

Spawn a Claude session as your foreman, promote it with `/grant 1 operator`, and
delegate from chat — it can then use `linkshell-ctl` to create sessions, inject
prompts, wait for READY, and wire pipes, while you stay in the chat pane. For a
resident agent that does this automatically, see the
[orchestrator](orchestrator.md).
