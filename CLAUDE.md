# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
cargo build              # debug build
cargo build --release    # optimized build
cargo run                # run the TUI
cargo run --release      # run optimized
cargo check              # compile check without building
cargo clippy             # lint
cargo fmt                # format
cargo test               # run tests
```

## Architecture

Linkshell is a terminal multiplexer TUI built for AI coding agents. It manages up to 8 concurrent PTY sessions (Claude, Codex, shell, or custom commands) with real-time state inference, token tracking, and cost estimation.

### Module Overview

| File | Responsibility |
|------|---------------|
| `main.rs` | Async event loop, terminal init/cleanup, key routing |
| `app.rs` | App state, session lifecycle, all event handlers |
| `session.rs` | Session model: PTY, vt100 screen, state, token stats |
| `events.rs` | Event enum shared between tasks and the main loop |
| `ui.rs` | Ratatui rendering — output pane, session bar, status panel, overlays |
| `patterns.rs` | Pattern matching for session state inference and token/cost parsing |

### Data Flow

Three background tasks communicate via `tokio::mpsc` to the main loop:

1. **Input reader** — keyboard/mouse → `Key`/`Mouse` events
2. **Tick generator** — 500ms → `Tick` event (timeout-based state transitions)
3. **PTY reader** (one per session) — raw bytes → `SessionBytes` + `SessionOutput`/`SessionCurrentLine`

The main loop calls `handle_event()` then re-renders with `ui::draw()`.

### Output Dual-Path Design

PTY output takes two separate paths intentionally:
- `SessionBytes` → `session.process_bytes()` → `vt100::Parser` screen buffer (for display)
- `SessionOutput` (complete lines) → `PatternMatcher` (for state inference and token parsing)

This prevents escape sequences from corrupting state inference logic.

### Session States

`Starting → Ready ↔ Thinking/Running ↔ Waiting/Error → Dead`

States are inferred from output patterns in `patterns.rs`. A 2-second timeout with no output while `Running` or `Thinking` reverts to `Ready`.

### UI Layout

Three vertical panes:
1. **Main output** — active session's vt100 screen (cell-by-cell color preservation)
2. **Session bar** — tabbed slots with colored state dots per session kind
3. **Status panel** — elapsed time, token counts, cost per session

Overlays: NewSession dialog, CommandBar (`:` prefix), Help (`?`).

### Key Bindings (from README)

| Key | Action |
|-----|--------|
| `Alt+N` | New session dialog |
| `Alt+1`–`8` | Switch to session |
| `Alt+Left/Right` | Previous/next session |
| `Alt+X` | Kill active session |
| `:` | Open command bar |
| `?` | Toggle help |
| `Ctrl+Q` | Quit |
| Mouse drag | Select text (auto-copies to clipboard) |

### Command Bar Commands

`new <claude|codex|shell|cmd>`, `kill <id>`, `switch <id>`, `quit`

### Token/Cost Parsing

`patterns.rs` extracts cost (`~$1.23`) and token counts (`12,345 input`, `3.4k out`) from Claude output lines. Falls back to cost estimation at $3/MTok input, $15/MTok output when only counts are available.
