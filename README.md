# ◈ linkshell

A terminal multiplexer built for AI coding agents. Run Claude, Codex, and shell sessions side by side — and actually know what each one is doing.

See the [workflow recipes](docs/recipes.md), [configuration reference](docs/config-reference.md),
and [changelog](CHANGELOG.md).

```
┌─────────────────────────────────────────────────────────────────┐
│                                                                 │
│   [active session output]                                       │
│                                                                 │
│   > Analyzing the UCC transport layer...                        │
│   > Found 3 potential issues in ucp_tag_send.c                  │
│                                                                 │
├─────────────────────────────────────────────────────────────────┤
│          │   1    │   2    │   3    │                           │
│          │🟠claude│🔵codex │ shell  │                           │
├─────────────────────────────────────────────────────────────────┤
│ 1 🟠 →2   THINKING  1m 32s  │  ~450 tok  │  ~$0.02             │
│ 2 🔵       READY    0m 08s  │  ~1.2k tok │  ~$0.05             │
│ 3          RUNNING  0m 45s  │  —         │  —                  │
└─────────────────────────────────────────────────────────────────┘
```

## Why

tmux doesn't know your Claude session is blocked waiting on you. It doesn't know your Codex session just hit an error. It can't tell you token usage or cost at a glance. Linkshell does.

## Features

- **Up to 8 sessions** — Claude, Codex, shell, or any custom command
- **Detach & reattach** — tmux-style client/server split; quit the client and sessions keep running (`alt-d` to detach, `linkshell` or `linkshell -r` to reattach)
- **Split panes** — view two sessions side by side (`alt-\` to split, `alt--` to rotate, `alt-o` to switch focus)
- **Startup profiles** — save a layout of sessions and pipes with `profile save <name>`, relaunch it with `--profile <name>`
- **Aliased identities** — env-prefixed commands (`CLAUDE_CONFIG_DIR=~/w claude`) and configured wrapper aliases get full Claude/Codex treatment, each with its own config home
- **Local agents & LLMs** — opencode, oh-my-pi, pi, aider, and llama.cpp sessions get agent-style state inference; llama.cpp/Ollama/vLLM/LM Studio endpoints are chat-addressable via `[agents.*]`
- **Agent chat pane** (`alt-t`) — talk to any session or local LLM by name, run commands with `/`, and orchestrate everything without leaving the pane
- **Unified scrollback** — full-screen TUIs and shells scroll with the same keys; the view holds position while output streams and returns to live when you type
- **Multi-agent councils** — declarative TOML topologies that relay output between agents on state transitions, with fan-in joins, round limits, and done signals
- **Live session state** — READY, THINKING, RUNNING, WAITING, ERROR inferred from PTY output and JSONL logs
- **Accurate token & cost tracking** — read directly from the Claude/Codex JSONL logs (config-home aware, `CLAUDE_CONFIG_DIR`/`CODEX_HOME` respected), not screen-scraped
- **Pro/Max subscription aware** — detects subscription automatically; shows real token counts, skips meaningless cost
- **Session pipes** — forward output between sessions on state change; flash indicator when a pipe fires
- **Agent communication** — sessions receive `LINKSHELL_SESSION_ID`, `LINKSHELL_SOCK`, and a per-session `LINKSHELL_TOKEN` at spawn; use `linkshell-ctl` to signal state, wait for READY, or trigger pipes from within a session
- **Capability security** — every IPC connection is scoped: human shells hold operator rights, AI agents get worker capabilities, council members can only report state; TCP requires a valid token
- **Remote agent support** — opt-in TCP listener for agents on other machines; same typed JSONL protocol as the Unix socket
- **WAITING alerts** — yellow border when an agent is blocked on your input
- **Desktop notifications** — configurable WAITING/ERROR alerts via notify-send, OSC 9, or bell
- **ERROR alerts** — red flashing border on failure
- **Mouse text selection** — drag to select, auto-copies to clipboard
- **Centered session bar** — slots reflow based on how many sessions are open
- **Full PTY passthrough** — your keystrokes go straight to the active session
- **Color coded by type** — 🟠 orange for Claude, 🔵 blue for Codex

## Install

```bash
cargo install linkshell        # installs linkshell + linkshell-ctl
```

Or grab a prebuilt binary for Linux (x86_64/aarch64) or macOS
(Intel/Apple Silicon) from the
[releases page](https://github.com/wfaderhold21/linkshell/releases).

From source:

```bash
git clone https://github.com/wfaderhold21/linkshell
cd linkshell
cargo build --release
cp target/release/linkshell ~/.local/bin/
cp target/release/linkshell-ctl ~/.local/bin/
```

Requires Rust 1.80+.

## Usage

```bash
linkshell               # Unix socket at $XDG_RUNTIME_DIR/linkshell/<pid>.sock
linkshell --tcp         # also open TCP agent listener on port 7373
linkshell --tcp 9000    # custom TCP port
linkshell --council examples/council.toml   # launch a multi-agent council
linkshell --profile ucc-dev                 # launch a named session profile
```

Then create your first session with `alt-n`.

Linkshell runs as a client/server pair, like tmux: the first `linkshell`
starts a background server that owns the sessions, and the foreground TUI is
a client attached to it. `alt-d` detaches — sessions keep running — and
running `linkshell` (or `linkshell -r`) again reattaches to the live server.

If something looks wrong (missing logs, stale socket, nested multiplexer,
limited terminal colors), run `linkshell doctor` for a diagnostic report.

## Aliased Claude / Codex sessions

Sessions running Claude or Codex under a different config home are recognized
and get the full treatment — state inference patterns, the JSONL token/cost
watcher, and pricing — instead of being treated as generic custom commands.

Two ways to spell them:

**Inline env prefix** — just works, no config needed. The classifier sees
through leading `VAR=value` assignments, and the watcher reads the config
home from the command itself:

```
new custom CLAUDE_CONFIG_DIR=~/.claude-work claude
new custom CODEX_HOME=~/.codex-personal codex
```

(or enter the same command line in the `alt-n` dialog's Custom field)

**Config alias** — for wrapper scripts or shell aliases whose name doesn't
contain `claude`/`codex`. Map the command basename in `[sessions.aliases]`:

```toml
[sessions.aliases.claude-work]
kind = "claude"
config_dir = "~/.claude-work"     # exported as CLAUDE_CONFIG_DIR

[sessions.aliases.cx]
kind = "codex"
config_dir = "~/.codex-personal"  # exported as CODEX_HOME
```

When `config_dir` comes from an alias it is also injected into the session's
environment, so the CLI and the log watcher agree on where the config home is.
Precedence for the watcher: inline env prefix → alias `config_dir` →
`$CLAUDE_CONFIG_DIR`/`$CODEX_HOME` in linkshell's own environment → the
default `~/.claude` / `~/.codex`.

## Councils

A council is a declarative multi-agent topology defined in a TOML file: named
agents plus routes that relay output between them on state transitions
(`ready`/`waiting`), with `join = "all"` fan-in, extraction modes, round limits,
and an optional `done_signal` for early termination. See
[`examples/council.toml`](examples/council.toml) for a fully commented
author/critic review loop.

Launch one at startup with `--council <file>` or at runtime from the command
bar (`alt-c`):

```
council <file.toml>   # spawn the agents and start routing
council status        # current round / completion state
council stop          # detach the router; sessions keep running
```

Council members are spawned with the minimal `SignalState` capability — they
can report their own state but cannot inject input, manage pipes, or create
sessions. Live progress (`round R/M`, done) is shown in the Status panel title.

## Agent Chat

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
back into the transcript. Local LLM agents keep a bounded per-agent
conversation history.

The transcript scrolls with the mouse wheel or `PageUp`/`PageDown` (a marker
on the input separator shows how far up you are). Drag to select transcript
text — it is copied to the clipboard on release, like the session panes.
Pasting into the chat input works too; multi-line pastes are delivered to
sessions via bracketed paste so they arrive as one message.

When an AI session stops on a permission dialog or y/n question, the prompt
is posted into the chat transcript. `/yes` and `/no` answer the most recent
request with the CLI's own keys (claude: `1`/Esc, codex: `y`/`n`); use
`/yes <session>` or `/no <session>` to target a specific one, or type
anything else with `@name <text>`.

Local LLM agents are any OpenAI-compatible endpoint — llama.cpp server,
Ollama, vLLM, LM Studio:

```toml
[agents.qwen]
endpoint = "http://localhost:8080/v1"   # /v1 optional
model = "qwen3.6-27b"
system = "You are a concise coding assistant."
# api_key = "..."                       # sent as Bearer if set
```

**Orchestration pattern**: spawn a Claude session as your foreman, promote it
with `/grant 1 operator`, and delegate from chat — it can then use
`linkshell-ctl` to create sessions, inject prompts, wait for READY, and wire
pipes, while you stay in the chat pane.

## Orchestrator Agent

Linkshell can run a resident agent that keeps track of every session, chats
with you in the chat pane, and acts on your behalf — "start a claude session
in ~/proj and have it fix the parser bug" from chat, no keystrokes in any
session. It is also woken proactively when a session hits WAITING, ERROR, or
dies, and posts a short summary of what's blocked.

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

Two provider classes:

- **API class** (`anthropic`, `openai`, `lmstudio`): an in-process tool-use
  loop with tools for listing sessions, reading output, starting sessions
  (with cwd + initial prompt), typing into sessions, and managing pipes.
- **CLI class** (`claude`, `codex`, `opencode`, `omp`): the CLI runs as a
  session with operator-tier IPC capabilities and drives linkshell via
  `linkshell-ctl` (`list`, `read`, `new`, `input --wait`, `pipe`, `chat`). By
  default it is *hidden*: no session bar slot, no Alt+N digit, doesn't count
  against the 8-session limit — you talk to it through the chat pane and it
  replies through `linkshell-ctl chat`. Set `hidden = false` (or use
  `:orchestrator show|hide` at runtime) to give it a visible session tab.
  CLI-class orchestrators launch with `permission_mode = "accept-edits"` by
  default — the CLI's own safe auto-approval flags — so routine edits don't
  stop to ask. Bypass-style modes are rejected, same as
  `--dangerously-skip-permissions` in session commands.
  If the hidden CLI still hits a permission dialog or errors, the prompt is
  posted to chat — answer it right there with `/yes` / `/no`, or type any
  other reply with `@agent <text>`; it is typed into its terminal.

**Skills** give the orchestrator reusable playbooks. Drop `*.md` files into
`~/.config/linkshell/skills/` (or set `skills_dir`): the file stem is the
skill name, and the description comes from a `description:` line in leading
`---` frontmatter (or the first non-empty line). Only name + description go
into the prompt; the full text is loaded on demand — API-class orchestrators
call a `use_skill` tool, CLI-class orchestrators get the file paths in their
briefing and read them directly.

**Memory** persists across restarts. The orchestrator carries a small notes
file — `~/.config/linkshell/memory.md` by default, or `memory_file` — that is
injected into its prompt each turn and appended to via a `remember` tool
(project layout, user preferences, recurring commands; one sentence per note).
You curate the file by hand; it is scaffolded automatically on first start.
See [docs/orchestrator-memory.md](docs/orchestrator-memory.md) for details.

In chat, unaddressed messages default to the orchestrator when one is running.
`:orchestrator start|stop|restart|reset|pause|resume|status|show|hide` manages it
at runtime (also usable from chat as `/orchestrator …`). If the agent dies — its
task exits or the CLI session ends — a chat notice appears with the restart
command. `pause` keeps the orchestrator's context but drops incoming chat and
session events until `resume` (CLI-class orchestrators are also SIGSTOPped),
unlike `stop`, which discards its conversation.

The orchestrator can never kill a session on its own: a kill request shows up
in chat and only `/confirm-kill` executes it (`/deny-kill` refuses).

If an API-class orchestrator gets stuck mid-turn — spinning through tool
iterations or blocked waiting on a session — `/interrupt` (alias `/stop`)
breaks the turn at the next safe point. Blocked tool calls return
"interrupted by user" to the model, so its history stays coherent and it
can be redirected on the next message.

`/reset` clears an API-class orchestrator's conversation context in place —
useful when the context has filled up with monitoring events — while keeping
the task and its token totals. If the agent task has died, `/reset` falls back
to a full restart, so it always leaves a working orchestrator behind.

## Local agent sessions

Sessions running `opencode`, `omp` (oh-my-pi), `pi`, `aider`, `llama-cli`, or
`ollama` are recognized as local agents: they get agent-style state inference
(THINKING on spinners/working verbs, READY on idle prompts) and terminal-based
token scraping. Wrappers with other names can be mapped with
`kind = "local"` in `[sessions.aliases]`.

## Scrollback

`alt-shift-PageUp/PageDown` (and `alt-shift-↑/↓`) scroll every session type
the same way. Shells use the terminal's native scrollback; full-screen TUIs
(claude, codex, opencode) scroll through linkshell's captured line history,
shown dimmed. The view holds position while new output streams in — typing
returns you to the live tail.

## Keybindings

| Key | Action |
|-----|--------|
| `alt-n` | New session dialog |
| `alt-c` | Open command bar |
| `alt-t` | Toggle agent chat pane |
| `alt-h` | Toggle help |
| `alt-x` | Kill active session |
| `alt-d` | Detach (sessions keep running) |
| `alt-\` | Toggle split pane |
| `alt--` | Rotate split layout |
| `alt-o` | Focus next pane |
| `alt-b` | Toggle broadcast input to all sessions |
| `alt-g` | Dock the chat pane |
| `alt-1` … `alt-8` | Switch to session by number |
| `alt-←` / `alt-→` | Cycle sessions |
| `ctrl-q` | Quit (shuts down the server and all sessions; use `alt-d` to leave them running) |
| `esc` | Dismiss overlay |
| `alt-shift-PageUp/PageDown` | Scroll output (page) |
| `alt-shift-↑` / `alt-shift-↓` | Scroll output (line) |

All other input is passed through to the active session's PTY.

## Command Bar

Press `alt-c` to open. Available commands:

```
new claude [name]     Start a Claude session
new codex [name]      Start a Codex session
new shell [name]      Start a shell session
new <cmd> [name]      Start a single-word command as a session
new custom <cmd...>   Start a full command line (spaces, env prefixes) as a session
kill                  Kill the active session
kill <n>              Kill session by number
pause [n]             Pause a session's process (SIGSTOP) — keeps its context, frees CPU
resume [n]            Resume a paused session (SIGCONT)
council <file.toml>   Launch a multi-agent council
council status        Show council round / completion state
council stop          Detach the council router (sessions keep running)
restart [n]           Respawn a session with the same command, name, and cwd
profile save <name>   Save the current sessions and pipes as a startup profile
grant <n> <tier>      Set a session's IPC capabilities (operator|worker|council)
config path           Show the config file location
config edit           Open the config in $EDITOR (as a session)
config reload         Re-read linkshell.toml without restarting
pipe <src> <dst> [--extract=last-block|last-n=N|diff] [--summarize=N] [--on=ready|waiting|manual] [--prefix="..."]
                      Forward output from src to dst on state change
pipe fire [src] [dst] Manually fire a pipe with trigger=manual
unpipe <src> [dst]    Remove pipe(s) from src
pipes                 Inspect, pause, fire, or delete configured pipes
detach                Detach the client; the server and sessions keep running
quit                  Exit linkshell (shuts down the server and all sessions)
```

## New Session Dialog

Press `alt-n` for the interactive dialog. Use arrow keys or `1`–`4` to pick the session type, `tab` to move between fields, `enter` to create.

## Session States

| State | Meaning | Border |
|-------|---------|--------|
| STARTING | Process spawning | — |
| READY | Prompt detected, waiting for input | — |
| THINKING | AI model is processing | — |
| RUNNING | Active output streaming | — |
| WAITING | Agent asked you something, blocked | 🟡 yellow |
| ERROR | Error pattern detected or process crashed | 🔴 red flash |
| DEAD | Process exited | — |

## Status Panel

Each session gets one row:

```
1 🟠 →2   THINKING  1m 32s  │  ~450 tok  │  ~$0.02
```

`→2` means this session has an active pipe to session 2. The arrow goes bold for one second when the pipe fires. Token counts and cost come from the JSONL logs written by Claude and Codex — not from screen scraping. Shell and custom sessions show `—`.

## Pipes

Pipes forward a snapshot of one session's output to another when a trigger fires. They are edge-triggered on state change, not continuous.

```
pipe 1 2                          on READY, forward last code block
pipe 1 2 --on=waiting             fire when session 1 hits WAITING
pipe 1 2 --extract=last-n=20      last 20 lines instead of last block
pipe 1 2 --extract=diff           lines starting with + or -
pipe 1 2 --summarize=150          relay through Haiku first, max 150 tokens
pipe 1 2 --prefix="Review this:"  prepend text to the forwarded content
pipe fire 1 2                     manually fire a --on=manual pipe
unpipe 1                          remove all pipes from session 1
unpipe 1 2                        remove the specific 1→2 pipe
```

## Agent Integration

Every spawned session gets three environment variables set automatically:

```bash
LINKSHELL_SESSION_ID=3          # this session's id
LINKSHELL_SOCK=/run/user/1000/linkshell/12345.sock
LINKSHELL_TOKEN=<hex>           # capability token binding this session's rights
```

`linkshell-ctl` picks these up automatically (it presents the token in its
handshake, so a connection from inside a session carries that session's
capabilities — see below). Outside any session it falls back to the last
daemon socket recorded in `~/.config/linkshell/last_socket`, or `$LINKSHELL_SOCK`.

```bash
linkshell-ctl list                    # JSON snapshot of all sessions (incl. cwd)
linkshell-ctl state READY             # signal done; fires OnReady pipes
linkshell-ctl state THINKING          # signal working
linkshell-ctl output "step done"      # inject a line into this session's display
linkshell-ctl send [--wait] <name> <msg...>   # direct-message another agent
linkshell-ctl wait-ready <id> [--timeout=N]   # block until session <id> returns to READY
linkshell-ctl pipe list / add / remove / fire # manage pipes (operator capability)
linkshell-ctl new <kind> [name] [--cwd=PATH]  # start a session (operator capability)
linkshell-ctl input <id> <text...> [--wait]   # type into a session; --wait returns its answer
linkshell-ctl read <id> [n]                   # last n output lines of a session
linkshell-ctl chat <msg...>                   # post a line into the chat pane
linkshell-ctl kill <id> [reason...]           # request a kill; the user must /confirm-kill
```

### Capabilities

Every IPC connection is scoped by a capability set, resolved at handshake:

| Tier | Who gets it | Can do |
|------|-------------|--------|
| operator | the human (same-uid Unix peer without a token), shell sessions, headless registrations | everything, incl. `session_create`, `session_input_wait`, pipe management |
| worker | spawned Claude/Codex/custom sessions (via `LINKSHELL_TOKEN`) | report state/tokens/output, query, direct-message, fire pipes |
| council | council members | report their own state only |
| orchestrator | the resident CLI-class orchestrator session | same as operator (incl. `chat_post` and `session_kill_request`; kills still require human `/confirm-kill`) |

TCP connections must present a valid token; tokenless TCP is rejected.

### Claude Code hooks

Auto-signal state without changing your prompts:

```json
{
  "hooks": {
    "Stop": [{ "command": "linkshell-ctl state READY" }],
    "PreToolUse": [{ "command": "linkshell-ctl state THINKING" }]
  }
}
```

### Remote agents

With `--tcp`, remote agents connect over the network using the same typed
JSONL protocol as the Unix socket. Every message travels in an envelope —
`{"msg": {...}}`, plus an `"id"` on requests that expect a reply — and every
connection starts with a `hello`/`welcome` handshake. TCP requires a token
(mint one by spawning the agent locally, or register headlessly over Unix
first); same-uid Unix connections without a token get operator rights.

```python
import socket, json

s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.connect(("host", 7373))
f = s.makefile("rwb")

def send(msg, req_id=None):
    env = {"msg": msg} if req_id is None else {"id": req_id, "msg": msg}
    f.write(json.dumps(env).encode() + b"\n"); f.flush()

# Handshake — name registers a headless session slot (Unix); TCP needs token
send({"type": "hello", "protocol": 1, "token": TOKEN, "name": "remote-claude"})
welcome = json.loads(f.readline())["msg"]     # session_id, capabilities

# Signal state
send({"type": "state", "state": "THINKING"})

# Synchronous query (note the id)
send({"type": "query", "what": "sessions"}, req_id=1)
sessions = json.loads(f.readline())

# Receive pipe relay content
env = json.loads(f.readline())
if env["msg"]["type"] == "relay":
    process(env["msg"]["content"])
```

Message types: `hello`, `state`, `tokens`, `output`, `agent_send`, `broadcast`,
`fire_pipe`, `pipe_add`, `pipe_remove`, `session_create`, `session_input_wait`,
`query` — each gated by the connection's capabilities. Server→agent messages:
`welcome`, `relay`, `reply`, `error`.

## Built With

- [Ratatui](https://ratatui.rs) — TUI framework
- [pty-process](https://crates.io/crates/pty-process) — PTY subprocess management
- [Tokio](https://tokio.rs) — async runtime
- [crossterm](https://crates.io/crates/crossterm) — terminal backend
