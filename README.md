# ◈ linkshell

An orchestration workspace for AI coding agents. It starts where tmux does —
Claude, Codex, and shell sessions side by side in one terminal — and keeps
going: it knows what each agent is doing, lets them talk to each other, routes
output between them, and can run a resident agent that drives the whole board.

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

## Why linkshell

A multiplexer displays your sessions. Linkshell *coordinates* them. It starts as
a familiar tmux-style workspace — but tmux doesn't know your Claude session is
blocked waiting on you, that your Codex session just hit an error, or what any of
it is costing. Linkshell was built from the ground up around AI agents, so the
terminal grid is just the substrate. On top of it sits a control plane:

- **A resident orchestrator that runs the board.** Tell it "start a claude
  session in ~/proj and fix the parser bug" from chat — it spawns, prompts,
  waits, and pipes for you, and wakes proactively when something is blocked.
  [→ orchestrator](docs/orchestrator.md)
- **Agents that coordinate.** A typed IPC protocol with scoped capabilities lets
  sessions signal state, wait on each other, wire pipes, and spawn sessions —
  locally or over TCP. [Pipes](docs/pipes.md) route a snapshot from one agent to
  another on state change; [councils](docs/councils.md) wire whole author/critic
  topologies declaratively. [→ agent integration](docs/agent-integration.md)

And the awareness a generic multiplexer can't give you:

- **It knows what each agent is doing.** Live per-session state — READY,
  THINKING, RUNNING, WAITING, ERROR — inferred from PTY output and JSONL logs,
  with a yellow border when an agent is blocked on you and a red flash on error.
  [→ session states](docs/sessions.md#session-states)
- **It tracks real tokens and cost.** Read directly from the Claude/Codex JSONL
  logs (config-home aware), not screen-scraped — and subscription-aware, so
  Pro/Max shows real token counts and skips meaningless cost.
  [→ status panel](docs/panes-and-navigation.md#status-panel)
- **One chat pane drives everything.** Talk to any session, local LLM, or the
  orchestrator by name; answer permission prompts; run commands — without
  leaving the pane. [→ agent chat](docs/chat.md)

Plus the multiplexer fundamentals done right: detach/reattach with sessions that
survive, recursive tiled split panes, unified scrollback across full-screen TUIs
and shells, and mouse selection everywhere.

## Feature guides

| Guide | What's inside |
|-------|---------------|
| [Sessions](docs/sessions.md) | Starting sessions, detach/reattach, multiple linkshells, startup profiles, aliased Claude/Codex, local agents, session states |
| [Panes & navigation](docs/panes-and-navigation.md) | Split panes, scrollback, status panel, keybindings, command bar |
| [Pipes](docs/pipes.md) | Edge-triggered output forwarding between sessions |
| [Councils](docs/councils.md) | Declarative multi-agent topologies |
| [Agent chat](docs/chat.md) | The chat pane, local LLM agents, permission prompts |
| [Orchestrator](docs/orchestrator.md) | The resident agent: providers, skills, memory, runtime control |
| [Agent integration](docs/agent-integration.md) | `linkshell-ctl`, capabilities, Claude Code hooks, remote agents |
| [Configuration reference](docs/config-reference.md) | Full linkshell.toml reference |
| [Workflow recipes](docs/recipes.md) | End-to-end workflows |
| [Orchestrator memory](docs/orchestrator-memory.md) | Memory design |
| [Changelog](CHANGELOG.md) | Release notes |

## Install

```bash
cargo install linkshell        # installs linkshell + linkshell-ctl
```

Or grab a prebuilt binary for Linux (x86_64/aarch64) or macOS (Intel/Apple
Silicon) from the [releases page](https://github.com/wfaderhold21/linkshell/releases).

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
linkshell               # start a new session (Unix socket at $XDG_RUNTIME_DIR/linkshell/<pid>.sock)
linkshell ls            # list detached sessions
linkshell -r <id>       # reattach to a detached session
linkshell --tcp         # also open TCP agent listener on port 7373
linkshell --tcp 9000    # custom TCP port
linkshell --council examples/council.toml   # launch a multi-agent council
linkshell --profile ucc-dev                 # launch a named session profile
```

Then create your first session with `alt-n`. Linkshell runs as a client/server
pair, like tmux/screen: each `linkshell` starts a background server that owns its
sessions, and the foreground TUI is a client attached to it. `alt-d` detaches —
sessions keep running. See the [sessions guide](docs/sessions.md) for detach,
reattach, and running multiple independent linkshells.

If something looks wrong (missing logs, stale socket, nested multiplexer,
limited terminal colors), run `linkshell doctor` for a diagnostic report.

## Built With

- [Ratatui](https://ratatui.rs) — TUI framework
- [pty-process](https://crates.io/crates/pty-process) — PTY subprocess management
- [Tokio](https://tokio.rs) — async runtime
- [crossterm](https://crates.io/crates/crossterm) — terminal backend
