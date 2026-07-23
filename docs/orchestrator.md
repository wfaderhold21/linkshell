# Orchestrator Agent

Linkshell can run a resident agent that keeps track of every session, chats with
you in the chat pane, and acts on your behalf — "start a claude session in ~/proj
and have it fix the parser bug" from chat, no keystrokes in any session. It is
also woken proactively when a session hits WAITING, ERROR, or dies, and posts a
short summary of what's blocked.

```toml
[orchestrator]
enabled = true
provider = "anthropic"    # anthropic | openai | lmstudio  (API loop)
                          # claude | codex | opencode | omp (CLI session)
name = "agent"            # chat target: @agent ...
# model = "claude-opus-4-8"
# endpoint = "http://localhost:1234/v1"   # openai/lmstudio
# api_key = "..."          # else ANTHROPIC_API_KEY / OPENAI_API_KEY
# system = "extra instructions"
# skills_dir = "~/.config/linkshell/skills"  # *.md skill files; defaults to
#                                            # this path when the dir exists
# memory_file = "~/.config/linkshell/memory.md"  # persistent notes (default)
# hidden = true             # CLI class: keep the agent out of the session bar
# permission_mode = "accept-edits"  # CLI class: start with safe auto-approval
#                                   # flags (claude: --permission-mode acceptEdits,
#                                   # codex: --full-auto); "default" disables
# events = ["waiting", "error", "dead"]
# event_cooldown_secs = 30
```

## Provider classes

- **API class** (`anthropic`, `openai`, `lmstudio`): an in-process tool-use loop
  with tools for listing sessions, reading output, starting sessions (with cwd +
  initial prompt), typing into sessions, and managing pipes.
- **CLI class** (`claude`, `codex`, `opencode`, `omp`): the CLI runs as a session
  with operator-tier IPC capabilities and drives linkshell via `linkshell-ctl`
  (`list`, `read`, `new`, `input --wait`, `pipe`, `chat`). By default it is
  *hidden*: no session bar slot, no Alt+N digit, doesn't count against the
  8-session limit — you talk to it through the chat pane and it replies through
  `linkshell-ctl chat`. Set `hidden = false` (or use `:orchestrator show|hide` at
  runtime) to give it a visible session tab. CLI-class orchestrators launch with
  `permission_mode = "accept-edits"` by default — the CLI's own safe
  auto-approval flags — so routine edits don't stop to ask. Bypass-style modes
  are rejected, same as `--dangerously-skip-permissions` in session commands. If
  the hidden CLI still hits a permission dialog or errors, the prompt is posted
  to chat — answer it right there with `/yes` / `/no`, or type any other reply
  with `@agent <text>`; it is typed into its terminal.

## Skills

Skills give the orchestrator reusable playbooks. Drop `*.md` files into
`~/.config/linkshell/skills/` (or set `skills_dir`): the file stem is the skill
name, and the description comes from a `description:` line in leading `---`
frontmatter (or the first non-empty line). Only name + description go into the
prompt; the full text is loaded on demand — API-class orchestrators call a
`use_skill` tool, CLI-class orchestrators get the file paths in their briefing
and read them directly.

## Memory

Memory persists across restarts. The orchestrator carries a small notes file —
`~/.config/linkshell/memory.md` by default, or `memory_file` — that is injected
into its prompt each turn and appended to via a `remember` tool (project layout,
user preferences, recurring commands; one sentence per note). You curate the file
by hand; it is scaffolded automatically on first start. See
[docs/orchestrator-memory.md](orchestrator-memory.md) for details.

## Runtime control

In chat, unaddressed messages default to the orchestrator when one is running.
`:orchestrator start|stop|restart|reset|pause|resume|status|show|hide` manages it
at runtime (also usable from chat as `/orchestrator …`). If the agent dies — its
task exits or the CLI session ends — a chat notice appears with the restart
command. `pause` keeps the orchestrator's context but drops incoming chat and
session events until `resume` (CLI-class orchestrators are also SIGSTOPped),
unlike `stop`, which discards its conversation.

The orchestrator can never kill a session on its own: a kill request shows up in
chat and only `/confirm-kill` executes it (`/deny-kill` refuses).

If an API-class orchestrator gets stuck mid-turn — spinning through tool
iterations or blocked waiting on a session — `/interrupt` (alias `/stop`) breaks
the turn at the next safe point. Blocked tool calls return "interrupted by user"
to the model, so its history stays coherent and it can be redirected on the next
message.

`/reset` clears an API-class orchestrator's conversation context in place —
useful when the context has filled up with monitoring events — while keeping the
task and its token totals. If the agent task has died, `/reset` falls back to a
full restart, so it always leaves a working orchestrator behind.
