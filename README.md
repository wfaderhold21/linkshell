# ◈ linkshell

A terminal multiplexer built for AI coding agents. Run Claude, Codex, and shell sessions side by side — and actually know what each one is doing.

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
- **Live session state** — READY, THINKING, RUNNING, WAITING, ERROR inferred from PTY output and JSONL logs
- **Accurate token & cost tracking** — read directly from `~/.claude` and `~/.codex` JSONL logs, not screen-scraped
- **Pro/Max subscription aware** — detects subscription automatically; shows real token counts, skips meaningless cost
- **Session pipes** — forward output between sessions on state change; flash indicator when a pipe fires
- **Agent communication** — sessions receive `LINKSHELL_SESSION_ID` and `LINKSHELL_SOCK` at spawn; use `linkshell-ctl` to signal state or trigger pipes from within a session
- **Remote agent support** — opt-in TCP listener for agents on other machines; same JSON protocol as the Unix socket
- **WAITING alerts** — yellow border when an agent is blocked on your input
- **ERROR alerts** — red flashing border on failure
- **Mouse text selection** — drag to select, auto-copies to clipboard
- **Centered session bar** — slots reflow based on how many sessions are open
- **Full PTY passthrough** — your keystrokes go straight to the active session
- **Color coded by type** — 🟠 orange for Claude, 🔵 blue for Codex

## Install

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
linkshell               # Unix socket only (/tmp/linkshell.sock)
linkshell --tcp         # also open TCP agent listener on port 7373
linkshell --tcp 9000    # custom TCP port
```

Then create your first session with `alt-n`.

## Keybindings

| Key | Action |
|-----|--------|
| `alt-n` | New session dialog |
| `alt-c` | Open command bar |
| `alt-h` | Toggle help |
| `alt-x` | Kill active session |
| `alt-1` … `alt-8` | Switch to session by number |
| `alt-←` / `alt-→` | Cycle sessions |
| `ctrl-q` | Quit |
| `esc` | Dismiss overlay |
| `PageUp` / `PageDown` | Scroll output (20 lines) |
| `Shift-↑` / `Shift-↓` | Scroll output (3 lines) |

All other input is passed through to the active session's PTY.

## Command Bar

Press `alt-c` to open. Available commands:

```
new claude [name]     Start a Claude session
new codex [name]      Start a Codex session
new shell [name]      Start a shell session
new <cmd> [name]      Start any command as a session
kill                  Kill the active session
kill <n>              Kill session by number
pipe <src> <dst> [--extract=last-block|last-n=N|diff] [--summarize=N] [--on=ready|waiting|manual] [--prefix="..."]
                      Forward output from src to dst on state change
pipe fire [src] [dst] Manually fire a pipe with trigger=manual
unpipe <src> [dst]    Remove pipe(s) from src
quit                  Exit linkshell
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

Every spawned session gets two environment variables set automatically:

```bash
LINKSHELL_SESSION_ID=3          # this session's slot number
LINKSHELL_SOCK=/tmp/linkshell.sock
```

Use `linkshell-ctl` to signal linkshell from within a session:

```bash
linkshell-ctl state READY        # signal done; fires OnReady pipes
linkshell-ctl state THINKING     # signal working
linkshell-ctl pipe fire          # fire manual pipes from this session
linkshell-ctl pipe fire 3 5      # fire a specific pipe
linkshell-ctl output "step done" # inject a line into this session's display
```

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

With `--tcp`, remote agents connect over the network using the same JSONL protocol as the Unix socket:

```python
import socket, json

s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.connect(("host", 7373))

# Register to get a session slot
s.send(json.dumps({"type": "register", "name": "remote-claude"}).encode() + b"\n")
resp = json.loads(s.recv(1024))
session_id = resp["session_id"]

# Signal state
s.send(json.dumps({"type": "state", "state": "THINKING"}).encode() + b"\n")

# Receive pipe relay content
msg = json.loads(s.recv(65536))
if msg["type"] == "relay":
    process(msg["content"])
```

Supported message types: `register`, `state`, `tokens`, `output`, `fire_pipe`, `session_create`, `session_input_wait`.

## Built With

- [Ratatui](https://ratatui.rs) — TUI framework
- [pty-process](https://crates.io/crates/pty-process) — PTY subprocess management
- [Tokio](https://tokio.rs) — async runtime
- [crossterm](https://crates.io/crates/crossterm) — terminal backend
