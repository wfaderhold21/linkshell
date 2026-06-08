# Router Agent

An internal agent that watches all active sessions and automatically creates pipes when it detects hand-off opportunities, without the user having to wire them manually.

---

## Purpose

Pipes are powerful but require the user to know in advance which sessions should talk to each other. The Router removes that friction: it observes session output as it arrives, classifies what each session just produced, and proposes or auto-creates a pipe to the most relevant destination session.

---

## Trigger

The Router runs as a background task spawned once when linkshell starts (or toggled via `:router on/off`). It receives a clone of the `AppEvent` channel and fires on every `SessionOutput` event.

It does **not** fire on every line. It batches: when a source session transitions to `Ready`, the Router collects the last N lines (configurable, default 20) and sends them to Haiku for classification.

---

## Classification Call

**Model:** `claude-haiku-4-5-20251001`  
**Max output tokens:** 150  
**Cost per fire:** ~$0.001

**Prompt:**

```
You are a routing agent for a terminal multiplexer. Given the last output of a coding session, decide whether it produced an artifact worth forwarding to another session.

Output JSON only, no prose:
{
  "artifact": "code_block" | "diff" | "error" | "question" | "none",
  "confidence": 0.0–1.0,
  "suggested_extract": "last-block" | "diff" | "last-n=10" | null,
  "reason": "<10 words>"
}

Session output:
<last N lines>
```

If `confidence < 0.6` or `artifact == "none"`, the Router takes no action.

---

## Destination Selection

After classifying the source, the Router scores each other active session as a destination using a simple rule table (no LLM call):

| Source artifact | Preferred dest kind | Fallback |
|-----------------|--------------------|----|
| `code_block`    | `codex` or `shell` | any non-source |
| `diff`          | `claude`           | any non-source |
| `error`         | `claude`           | any non-source |
| `question`      | `claude`           | any non-source |

Ties broken by recency (most recently active session wins). If no suitable destination exists, the Router does nothing.

---

## Action Modes

Controlled by `:router` command:

| Mode | Behavior |
|------|----------|
| `suggest` (default) | Appends a one-line notification to the status bar: `Router: pipe 1→3? [y/n]`. User confirms with `y` or dismisses with `n`. |
| `auto` | Creates the pipe immediately without asking. Skips if a pipe from that source already exists. |
| `off` | Router is disabled. |

Toggle: `:router suggest`, `:router auto`, `:router off`

---

## Pipe Parameters Created

The Router always creates a `Manual` trigger pipe (not `OnReady`) — the classification already waited for `Ready`. It fires once immediately via `fire_pipe`, then the pipe is removed (one-shot). This avoids accumulating stale pipes.

Extract mode comes from `suggested_extract` in the classification response.  
No prefix is set (Router doesn't editorialize).

---

## Data Structures

New fields on `App`:

```rust
pub router_mode: RouterMode,         // Suggest | Auto | Off
pub router_tx: Option<mpsc::Sender<RouterEvent>>,
pub pending_router_suggestion: Option<RouterSuggestion>,
```

```rust
pub enum RouterMode { Suggest, Auto, Off }

pub struct RouterSuggestion {
    pub source_id: usize,
    pub dest_id: usize,
    pub extract: ExtractMode,
    pub reason: String,          // from Haiku, shown in status bar
}

pub enum RouterEvent {
    SessionReady { session_id: usize, last_lines: Vec<String> },
    Shutdown,
}
```

---

## New File: `src/router.rs`

```rust
pub async fn run_router(
    mut rx: mpsc::Receiver<RouterEvent>,
    tx: mpsc::Sender<AppEvent>,
    mode: Arc<Mutex<RouterMode>>,
) { ... }
```

The task loop:
1. Receives `SessionReady { session_id, last_lines }`
2. Calls Haiku with classification prompt
3. Parses JSON response
4. If confidence ≥ 0.6, sends `AppEvent::RouterSuggestion { source_id, dest_id, extract, reason }` back to the main loop

The main loop handles `RouterSuggestion`:
- In `Suggest` mode: sets `app.pending_router_suggestion`, re-renders status bar
- In `Auto` mode: calls `fire_pipe` directly

---

## Status Bar Display

When `pending_router_suggestion` is set:

```
Router → pipe 1→3 (diff)  "contains a patch"  [y] accept  [n] skip
```

Rendered in the status panel below the session rows. Pressing `y` fires the pipe and clears the suggestion. Pressing `n` clears without firing. Either action also clears the suggestion after 30 seconds of inactivity.

---

## New `AppEvent` Variants

```rust
AppEvent::RouterSuggestion {
    source_id: usize,
    dest_id:   usize,
    extract:   ExtractMode,
    reason:    String,
},
```

---

## Implementation Order

1. Add `src/router.rs` with `run_router` task
2. Add `RouterMode`, `RouterSuggestion`, `RouterEvent` to `src/events.rs` / `src/app.rs`
3. Wire `SessionReady` events into `run_router` from `handle_session_output` state transitions
4. Add `RouterSuggestion` handler in main loop
5. Update status panel to show pending suggestion
6. Add `:router` command parser in `execute_command`
7. Spawn `run_router` in `main.rs`

---

## Open Questions

- Should the Router ever suggest pipes between two Claude sessions? (Currently yes, if one produced a diff.)
- Should `auto` mode respect an existing pipe on the source and skip rather than double-fire?
- Rate limit: should the Router ignore a source session that fired within the last 60 seconds to avoid chattering on fast-iterating sessions?
