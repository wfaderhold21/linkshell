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
│ 1 🟠 THINKING  1m 32s  │ ~450 tok  │ ~$0.02                    │
│ 2 🔵 READY     0m 08s  │ ~1.2k tok │ ~$0.05                    │
│ 3    RUNNING   0m 45s  │ —         │ —                         │
└─────────────────────────────────────────────────────────────────┘
```

## Why

tmux doesn't know your Claude session is blocked waiting on you. It doesn't know your Codex session just hit an error. It can't tell you token usage or cost at a glance. Linkshell does.

## Features

- **Up to 8 sessions** — Claude, Codex, shell, or any custom command
- **Live session state** — READY, THINKING, RUNNING, WAITING, ERROR inferred from PTY output
- **WAITING alerts** — yellow border when an agent is blocked on your input
- **ERROR alerts** — red flashing border on failure
- **Token & cost tracking** — parsed from Claude output, displayed per session
- **Centered session bar** — slots reflow based on how many sessions are open
- **Full PTY passthrough** — your keystrokes go straight to the active session
- **Color coded by type** — 🟠 orange for Claude, 🔵 blue for Codex

## Install

```bash
git clone https://github.com/wfaderhold21/linkshell
cd linkshell
cargo build --release
cp target/release/linkshell ~/.local/bin/
```

Requires Rust 1.80+.

## Usage

Launch it from your project directory:

```bash
linkshell
```

Then create your first session with `alt-n`.

## Keybindings

| Key | Action |
|-----|--------|
| `alt-n` | New session dialog |
| `alt-c` | Open command bar |
| `alt-x` | Kill active session |
| `alt-1` … `alt-8` | Switch to session by number |
| `alt-←` / `alt-→` | Cycle sessions |
| `ctrl-q` | Quit |
| `esc` | Dismiss overlay |

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
1 🟠 THINKING  1m 32s  │  ~450 tok  │  ~$0.02
```

Token counts and cost are scraped from Claude's output. Shell and custom sessions show `—` for those fields.

## Built With

- [Ratatui](https://ratatui.rs) — TUI framework
- [pty-process](https://crates.io/crates/pty-process) — PTY subprocess management
- [Tokio](https://tokio.rs) — async runtime
- [crossterm](https://crates.io/crates/crossterm) — terminal backend
