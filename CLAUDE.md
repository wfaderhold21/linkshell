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

Linkshell is a terminal multiplexer TUI built for AI coding agents. It manages up to 8 concurrent PTY sessions (Claude, Codex, shell, or custom commands) with real-time state inference, token tracking, cost estimation, and session-to-session pipes.

### Module Overview

| File | Responsibility |
|------|---------------|
| `main.rs` | Async event loop, terminal init/cleanup, key routing, `--tcp`/`--council` flags |
| `app.rs` | App state, session lifecycle, all event handlers, council launch, command bar |
| `session.rs` | Session model: PTY, vt100 screen, state, token stats, `BaseKind` identity resolution |
| `events.rs` | Event enum shared between tasks and the main loop |
| `ui.rs` | Ratatui rendering — output pane, session bar, status panel, overlays |
| `patterns.rs` | Pattern matching for session state inference (dispatched on `BaseKind`) and token/cost parsing |
| `pipe.rs` | Pipe definitions, extraction logic, trigger evaluation, Haiku summarizer |
| `ipc.rs` | Unix socket + optional TCP listener, handshake, capability enforcement |
| `protocol.rs` | Typed wire protocol: `Envelope`/`Message`, error codes, per-message capability requirements |
| `auth.rs` | Capability tiers (operator / worker / council) and token minting |
| `council.rs` | council.toml parsing and the multi-agent routing engine (`CouncilRouter`) |
| `claude_log.rs` | Watch `$CLAUDE_CONFIG_DIR/projects` JSONL for cumulative token/cost stats |
| `codex_log.rs` | Watch `$CODEX_HOME/sessions` rollout JSONL for token/context stats |
| `config.rs` | linkshell.toml: commands, aliases, pricing, socket, keybindings |
| `keybindings.rs` | Configurable key chord parsing |
| `bin/ctl.rs` | `linkshell-ctl` — CLI client speaking the typed protocol |

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
3. **Status panel** — elapsed time, token counts, cost per session; `→ N` when a pipe is active

Overlays: NewSession dialog, CommandBar (`:` prefix), Help (`?`).

### Key Bindings (from README)

| Key | Action |
|-----|--------|
| `Alt+N` | New session dialog |
| `Alt+1`–`8` | Switch to session |
| `Alt+Left/Right` | Previous/next session |
| `Alt+X` | Kill active session |
| `:` | Open command bar |
| `Alt+H` | Toggle help |
| `Ctrl+Q` | Quit |
| Mouse drag | Select text (auto-copies to clipboard) |

### Command Bar Commands

`new <claude|codex|shell|cmd>`, `kill <id>`, `quit`

`pipe 1 2`, `pipe 1 2 --extract=last-n=15`, `pipe 1 2 --extract=diff`, `pipe 1 2 --summarize=150`, `pipe 1 2 --on=waiting`, `pipe 1 2 --prefix="Review this:"`, `unpipe 1`, `unpipe 1 2`

### Token/Cost Parsing

`patterns.rs` extracts cost (`~$1.23`) and token counts (`12,345 input`, `3.4k out`) from Claude output lines. Falls back to cost estimation at $3/MTok input, $15/MTok output when only counts are available.

---

## Pipe System

Pipes are **edge-triggered on state change, not continuous**. When a source session hits a trigger state (e.g. READY), linkshell extracts a formatted snapshot of its output and forwards it to a destination session. You extract the artifact, not the process — this keeps token usage low.

### Data Structures (`src/pipe.rs`)

```rust
#[derive(Debug, Clone)]
pub enum ExtractMode {
    LastBlock,           // last ```...``` fenced code block
    LastN(usize),        // last N lines
    Diff,                // lines starting with + or -
    Summarize(u32),      // relay through Haiku first, max N tokens
}

#[derive(Debug, Clone)]
pub enum PipeTrigger {
    OnReady,             // fires when source hits READY state
    OnWaiting,           // fires when source is blocked on user input
    Manual,              // only fires on explicit user command
}

#[derive(Debug, Clone)]
pub struct Pipe {
    pub source: usize,
    pub dest: usize,
    pub trigger: PipeTrigger,
    pub extract: ExtractMode,
    pub prefix: Option<String>,
    pub active: bool,
}
```

Add `pipes: Vec<Pipe>` to `App` in `src/app.rs`.

### Trigger Logic

Call `check_pipes` inside `handle_session_output` after state is updated. Collect triggered pipes, extract content, spawn `fire_pipe` tasks that send `AppEvent::PipeRelay { dest_id, message }` back through the event channel.

### Extraction Modes

| Mode | Token cost | When to use |
|------|-----------|-------------|
| `last-block` | Low | Implementer → reviewer |
| `last-n=N` | Low, bounded | Quick status relay |
| `diff` | Very low | Code review |
| `summarize=N` | Small API call + N output tokens | Noisy output, large context |

`Summarize` relays through **Haiku** — not Sonnet. Fast, cheap, fractions of a cent per relay. Use model ID `claude-haiku-4-5-20251001`.

### New AppEvent Variants (`src/events.rs`)

```rust
pub enum AppEvent {
    // ... existing variants
    PipeRelay { dest_id: usize, message: String },
    IpcStateOverride { session_id: usize, state: SessionState },
    IpcTokenUpdate   { session_id: usize, input: u64, output: u64 },
}
```

`PipeRelay` is handled in `main.rs` by writing the message bytes to the dest session's PTY writer.

### Status Panel

Active pipes show as `→ N` in the source session's status row. When a pipe fires, briefly highlight the arrow (bold/flash for one tick).

```
1 🟠 READY   → 2   last-block   1m 32s  │  ~450 tok  │  ~$0.02
```

### Implementation Order

1. Add `src/pipe.rs` with `Pipe`, `ExtractMode`, `PipeTrigger`
2. Add `pipes: Vec<Pipe>` to `App`
3. Add `PipeRelay` to `AppEvent`
4. Wire `check_pipes` into `handle_session_output`
5. Add `pipe` / `unpipe` to command bar parser in `execute_command`
6. Add `reqwest` to `Cargo.toml` for the summarizer call
7. Update status panel to show `→ N` for active pipes
8. Add `src/ipc.rs` for orchestrator integration (see below)

---

## Orchestrator Integration

### Option A — Observe Only (Zero Modification)

Run an existing orchestrator in a shell session. Linkshell infers state from output patterns. No changes to the orchestrator.

**Start here for council and foundry-x.**

### Option B — IPC Socket (`src/ipc.rs`)

Linkshell opens `/tmp/linkshell.sock` as a Unix domain socket. The orchestrator sends structured JSON messages to update session state, inject output lines, or report tokens.

**Linkshell side:**

```rust
pub async fn start_ipc_listener(tx: mpsc::Sender<AppEvent>, session_id: usize) {
    let path = "/tmp/linkshell.sock";
    let _ = std::fs::remove_file(path);
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    loop {
        if let Ok((stream, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(handle_ipc_connection(stream, tx, session_id));
        }
    }
}
```

Messages: `{"type":"state","state":"THINKING","detail":"..."}`, `{"type":"tokens","input":4200,"output":890}`, `{"type":"output","line":"..."}`, `{"type":"state","state":"READY"}`.

**Python shim (fire-and-forget, add to any orchestrator):**

```python
import socket, json

def linkshell_status(msg: dict):
    try:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.connect("/tmp/linkshell.sock")
        s.send(json.dumps(msg).encode() + b"\n")
        s.close()
    except Exception:
        pass  # linkshell not running, silently ignore
```

---

## Council Integration (`~/council`)

Council runs 4 sequential phases: investigate (Claude + Codex in parallel) → stage (deterministic merge) → debate (turn-by-turn) → resolve. Each phase outputs markdown + JSON to a run directory. Agents are invoked as subprocesses.

### Observe Mode (start here)

```bash
# Session 1: council run
linkshell new shell council
# in that shell:
python3 -m council issue --repo ~/myproject --task "fix the foo bug"
```

Linkshell reads stdout, infers RUNNING/READY from patterns. You watch both investigate agents fire and the debate rounds scroll by.

### Pipe Patterns for Council

**Watch parallel investigation:** Pipe both agent logs into a shared reviewer session (requires splitting agent output into separate shell sessions — redirect each agent's log with `tail -f`).

**Relay debate conclusion to resolver:** When the debate shell hits READY (last turn complete), pipe its last block to a session reading the resolve prompt.

```
pipe 2 4 --extract=last-block --prefix="Debate concluded. Now resolve:"
```

**Devil's advocate:** Pipe Claude's investigation conclusion to a Codex session to argue against it.

```
pipe 1 2 --summarize=100 --prefix="Argue against this design decision:"
```

### IPC Shim for Council

Add `linkshell_status()` calls to `phases.py`:

```python
# in run_investigate():
linkshell_status({"type": "state", "state": "THINKING", "detail": "investigating"})
# after both agents return:
linkshell_status({"type": "state", "state": "READY"})
linkshell_status({"type": "tokens", "input": total_input, "output": total_output})

# in run_debate() per round:
linkshell_status({"type": "output", "line": f"Debate round {round_num}: {attacker} attacks"})

# after convergence check:
if converged:
    linkshell_status({"type": "state", "state": "READY"})
```

This lets pipe triggers fire on phase boundaries, not just on process exit.

---

## Foundry-X Integration (`~/foundry-x-push`)

Foundry-X is a FSM-driven code-generation pipeline: `PRD_OPEN → SPEC_READY → CODE_READY → TEST_READY → EXECUTED → VALIDATED → (human gate) → MERGED`. Agents are injected callables; artifacts are stored as SHA256 blobs in a content-addressed registry. State is persisted to SQLite.

### Observe Mode (start here)

```bash
linkshell new shell foundry
# in that shell:
python3 scripts/run_pipeline.py --prd prd.md --library ucc
```

State transitions, retry counts, and compilation errors stream to stdout. Linkshell shows RUNNING during code-gen and compilation, WAITING at the human approval gate.

### Pipe Patterns for Foundry-X

**Route compilation errors to a debug agent:** When the test session hits WAITING (compilation failed, waiting for retry), pipe the last N lines to a Claude session for analysis.

```
pipe 1 2 --on=waiting --extract=last-n=30 --prefix="Fix the compilation error:"
```

**Human approval workflow:** When the pipeline reaches VALIDATED (human gate), pipe the diff to a review session.

```
pipe 1 3 --extract=diff --prefix="Review this patch before approval:"
```

**Parallel model comparison:** Spin up two pipeline sessions with different `--model` flags, pipe both results to a comparator session.

```
pipe 1 5 --extract=last-block --prefix="Pipeline A output:"
pipe 2 5 --extract=last-block --prefix="Pipeline B output:"
```

### IPC Shim for Foundry-X

Add `linkshell_status()` calls to `core/orchestrator.py` in `_drive()` at each state transition:

```python
# after each FSM transition:
linkshell_status({"type": "state",  "state": new_state.name})
linkshell_status({"type": "output", "line": f"→ {prev_state.name} → {new_state.name} (attempt {run.attempt})"})

# on retry:
linkshell_status({"type": "output", "line": f"Retry {run.attempt}/{MAX_RETRIES}: {error_summary}"})

# at VALIDATED (human gate):
linkshell_status({"type": "state", "state": "WAITING", "detail": "awaiting human approval"})
```

This maps FSM states directly to linkshell session states, so pipe triggers fire at the right moments — especially `OnWaiting` at the approval gate.

The `--transport` flag in `scripts/run_pipeline.py` already abstracts the LLM provider. A `linkshell` transport could write prompts into a linkshell session and read responses back via the IPC socket — enabling human-in-the-loop editing mid-pipeline.
