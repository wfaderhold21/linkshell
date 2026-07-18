# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build              # debug build
cargo build --release    # optimized build
cargo run                # run the TUI
cargo check              # compile check without building
cargo clippy             # lint
cargo fmt                # format
cargo test               # run tests
```

## Architecture

Linkshell is a terminal multiplexer TUI built for AI coding agents. It manages up to 8 concurrent PTY sessions (Claude, Codex, local agents, shells, or custom commands) with real-time state inference, token/cost tracking from JSONL logs, session-to-session pipes, multi-agent councils, an agent chat pane, and a resident orchestrator agent. It runs as a tmux-style client/server pair: the server owns sessions and survives detach (`alt-d`); `linkshell -r` reattaches.

### Module Overview

| File | Responsibility |
|------|---------------|
| `main.rs` | Client/server launch, async event loop, terminal init/cleanup, key routing, `--tcp`/`--council`/`--profile`/`--server`/`-r` flags, `doctor` subcommand |
| `reattach.rs` | Detach/reattach machinery: relay client, `SwappableWriter` backend redirection, reattach info file |
| `app.rs` | App state, session lifecycle, all event handlers, command bar + palette, profiles, chat pane state |
| `session.rs` | Session model: PTY, vt100 screen, state, token stats, `BaseKind` identity resolution |
| `events.rs` | `AppEvent` enum shared between tasks and the main loop |
| `ui.rs` | Ratatui rendering — output pane(s), splits, session bar, status panel, chat pane, overlays |
| `patterns.rs` | Pattern matching for session state inference (dispatched on `BaseKind`) and token/cost parsing |
| `pipe.rs` | Pipe definitions (`Pipe`, `ExtractMode`, `PipeTrigger`), extraction logic, trigger evaluation, Haiku summarizer |
| `ipc.rs` | Unix socket + optional TCP listener, handshake, capability enforcement |
| `protocol.rs` | Typed wire protocol: `Envelope`/`Message`, error codes, per-message capability requirements |
| `auth.rs` | Capability tiers (operator / worker / council / orchestrator) and token minting |
| `council.rs` | council.toml parsing and the multi-agent routing engine (`CouncilRouter`) |
| `orchestrator/` | Resident orchestrator agent: `mod.rs` task/tools/prompts/memory, `skills.rs` on-demand markdown skills, `anthropic.rs` + `openai.rs` tool-use loops. CLI-class providers instead run as a session with `orchestrator_caps()` driving `linkshell-ctl` |
| `agent_llm.rs` | Chat-addressable local LLMs: any OpenAI-compatible endpoint under `[agents.*]` |
| `claude_log.rs` | Watch `$CLAUDE_CONFIG_DIR/projects` JSONL for cumulative token/cost stats |
| `codex_log.rs` | Watch `$CODEX_HOME/sessions` rollout JSONL for token/context stats |
| `opencode_log.rs` | Watch the OpenCode SQLite DB for token/cost stats |
| `ctx_probe.rs` | Probe local model backends (llama.cpp, LM Studio) for context window size |
| `notify.rs` | Desktop notifications (notify-send, OSC 9, bell) for WAITING/ERROR |
| `doctor.rs` | `linkshell doctor` — environment/config diagnostics |
| `config.rs` | linkshell.toml: commands, aliases, pricing, socket, agents, orchestrator, profiles, keybindings |
| `keybindings.rs` | Configurable key chord parsing |
| `bin/ctl.rs` | `linkshell-ctl` — CLI client speaking the typed protocol |

### Data Flow

Background tasks communicate via `tokio::mpsc` to the main loop:

1. **Input reader** — keyboard/mouse → `Key`/`Mouse` events
2. **Tick generator** — 500ms → `Tick` event (timeout-based state transitions)
3. **PTY reader** (one per session) — raw bytes → `SessionBytes` + `SessionOutput`/`SessionCurrentLine`
4. **Log watchers** — Claude/Codex/OpenCode logs → token/cost events
5. **IPC listener** — `linkshell-ctl` / remote agents → state, input, pipe, chat messages
6. **Orchestrator task** (API class) — tool-use loop ↔ main loop via events

The main loop calls `handle_event()` then re-renders with `ui::draw()`.

### Output Dual-Path Design

PTY output takes two separate paths intentionally:
- `SessionBytes` → `session.process_bytes()` → `vt100::Parser` screen buffer (for display)
- `SessionOutput` (complete lines) → `PatternMatcher` (for state inference and token parsing)

This prevents escape sequences from corrupting state inference logic.

### Session States

`Starting → Ready ↔ Thinking/Running ↔ Waiting/Error → Dead`

States are inferred from output patterns in `patterns.rs` (dispatched on `BaseKind`), overridden by IPC `state` messages, and refined by JSONL log activity. A 2-second timeout with no output while `Running` or `Thinking` reverts to `Ready`.

### Pipes

Pipes are **edge-triggered on state change, not continuous** (`src/pipe.rs`). When a source session hits a trigger state (`OnReady`, `OnWaiting`, or `Manual`), linkshell extracts a snapshot (`LastBlock`, `LastN(n)`, `Diff`, or `Summarize(n)` via Haiku, model `claude-haiku-4-5-20251001`) and forwards it to the destination's PTY through `AppEvent::PipeRelay`. Trigger checks run in `check_pipes` after state updates in output handling. Active pipes show as `→ N` in the status panel, bold for one tick when they fire.

### Capabilities

Every IPC connection is scoped at handshake (`auth.rs`): **operator** (human, shell sessions — everything), **worker** (spawned AI sessions — report state/tokens, query, message, fire pipes), **council** (report own state only), **orchestrator** (operator-tier plus `chat_post`; kills still need human `/confirm-kill`). Sessions get `LINKSHELL_SESSION_ID`, `LINKSHELL_SOCK`, `LINKSHELL_TOKEN` at spawn. TCP requires a valid token.

### Orchestrator

Two provider classes (`[orchestrator]` in linkshell.toml):
- **API class** (`anthropic`, `openai`, `lmstudio`): in-process tool-use loop with tools for sessions, pipes, chat, `use_skill`, and `remember`.
- **CLI class** (`claude`, `codex`, `opencode`, `omp`): the CLI runs as a (by default hidden) session with orchestrator capabilities, driving linkshell via `linkshell-ctl`.

Skills are `*.md` files in `~/.config/linkshell/skills/` (name + description in prompt, body loaded on demand). Persistent memory lives in `~/.config/linkshell/memory.md` (`memory_file`), injected each turn, appended via the `remember` tool, truncated in-prompt at 8 KiB. See `docs/orchestrator-memory.md`.

### UI Layout

Vertical panes: main output (optionally split into two session panes), session bar, status panel, and an optional chat pane (`alt-t`, dockable with `alt-g`). Overlays: NewSession dialog, command bar/palette (`alt-c`), pipes overlay, help (`alt-h`).

Keybindings and command-bar commands are user-facing — keep the README's Keybindings and Command Bar sections as the source of truth and update them when defaults in `keybindings.rs` or the parser in `app.rs::execute_command` change.

## Documentation

- `README.md` — user-facing feature docs; keep in sync with behavior changes
- `docs/config-reference.md` — full linkshell.toml reference
- `docs/recipes.md` — workflow recipes
- `docs/orchestrator-memory.md` — orchestrator memory design
- `CHANGELOG.md` — add entries under `## Unreleased` for user-visible changes
