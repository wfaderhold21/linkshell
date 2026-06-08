# Error Triage Agent

An internal agent that monitors session output for errors, stack traces, and compilation failures, then opens a repair session automatically with the error as context.

---

## Purpose

When a session hits an error — a Rust compile failure, Python traceback, test failure, or shell error — the user's next action is almost always to copy the error and paste it into Claude. The Error Triage agent does this automatically: it detects the error, extracts the relevant portion, and either opens a new session with it or routes it to an existing Claude session.

---

## Trigger

The agent hooks into the existing `PatternMatcher` pipeline in `handle_session_output`. It does **not** make an LLM call to detect errors — that's handled locally via regex patterns to keep latency near zero.

An error is detected when **any** of the following patterns match a line:

```
error[E\d+]:          # Rust compiler error
thread '.*' panicked  # Rust panic
Traceback (most recent call last):   # Python traceback
FAILED                # pytest / cargo test output
Error:                # generic (only if session kind != shell, to reduce noise)
✗ | ×                 # common test runner fail symbols
make: *** [           # Makefile error
```

After detection, the agent waits for the session to return to `Ready` or `Waiting` (the error output is complete) before acting. This uses the existing state transition in `handle_session_output` — no new polling needed.

---

## Error Extraction

Once the session reaches `Ready`/`Waiting` after an error was flagged, the agent extracts the error block from `session.output_lines`.

Extraction strategy (in order of preference):

1. **Fenced block** — if the error is surrounded by a terminal separator line (e.g. `─────` or `=====`), extract between those
2. **Last N lines** — fall back to last 30 lines of `output_lines`
3. **Traceback slice** — for Python, everything from the `Traceback` line to the last `Error:` line

The extracted block is trimmed to 2000 characters max to avoid oversized prompts.

---

## Classification Call (Optional)

If the error type couldn't be determined locally, a Haiku call classifies it and generates a short repair hint:

**Model:** `claude-haiku-4-5-20251001`  
**Max output tokens:** 100  
**Cost per fire:** ~$0.0005  
**Only called when:** local regex matched but error type is ambiguous

**Prompt:**

```
Classify this error in 5 words or less and give a one-line repair hint.
Output JSON only: {"type": "<error type>", "hint": "<repair hint>"}

<extracted error block>
```

If the error type is clear from regex (e.g. `error[E0502]` → "Rust borrow checker"), skip the Haiku call entirely.

---

## Action Modes

Controlled by `:triage` command:

| Mode | Behavior |
|------|----------|
| `notify` (default) | Shows an overlay notification with the error type and a `[r]` keybind to open a repair session |
| `auto` | Immediately opens or routes to a Claude session with the error |
| `off` | Agent disabled |

Toggle: `:triage notify`, `:triage auto`, `:triage off`

---

## Repair Session Behavior

When the user accepts (or `auto` mode fires):

1. **If a Claude session already exists and is `Ready`:** route the error to it via `fire_pipe` (no new session created)
2. **If no Claude session is ready:** spawn a new Claude session, wait for `Ready`, then send the repair prompt

**Repair prompt template:**

```
<optional hint from Haiku, e.g. "Rust lifetime error">

The following error occurred in session <N> (<session name>):

<extracted error block>

Please diagnose and fix it.
```

The prefix is configurable via `:triage prefix "..."`.

---

## Data Structures

New fields on `App`:

```rust
pub triage_mode: TriageMode,
pub triage_state: TriageState,
```

```rust
pub enum TriageMode { Notify, Auto, Off }

pub enum TriageState {
    Idle,
    Watching { session_id: usize, error_hint: Option<String> },
    Pending  { session_id: usize, extracted: String, error_type: String },
}
```

State machine:
- `Idle` → `Watching` when error pattern matches during `Running`/`Thinking`
- `Watching` → `Pending` when session reaches `Ready`/`Waiting`
- `Pending` → `Idle` after action is taken (or dismissed)

---

## New File: `src/triage.rs`

```rust
pub struct ErrorDetector {
    patterns: Vec<Regex>,
}

impl ErrorDetector {
    pub fn new() -> Self { ... }
    pub fn check(&self, line: &str, kind: &SessionKind) -> bool { ... }
}

pub fn extract_error_block(lines: &VecDeque<String>) -> String { ... }

pub async fn classify_error(block: &str) -> Option<(String, String)> { ... }
// returns Option<(error_type, hint)>
```

`ErrorDetector::check` is called from `handle_session_output` in `app.rs`, same location as `PatternMatcher`.

---

## Overlay Display

In `notify` mode, when `triage_state == Pending`, an overlay bar appears above the session bar:

```
╔ Error Triage ══════════════════════════════════════════════════════╗
║ Session 2 (codex): Rust borrow checker error                       ║
║ [r] open repair session   [v] view extracted block   [esc] dismiss ║
╚════════════════════════════════════════════════════════════════════╝
```

Rendered in `ui.rs` as a new overlay layer, similar to the existing `NewSession` dialog. The `[v]` view opens a scrollable popup showing the extracted block before committing.

---

## New `AppEvent` Variants

```rust
AppEvent::TriageDetected {
    session_id: usize,
    extracted:  String,
    error_type: String,
    hint:       Option<String>,
},
```

---

## Key Bindings

New bindings active only when triage overlay is visible:

| Key | Action |
|-----|--------|
| `r` | Open / route to repair session |
| `v` | View extracted block popup |
| `Esc` | Dismiss triage overlay |

---

## Implementation Order

1. Add `src/triage.rs` with `ErrorDetector` and `extract_error_block`
2. Add `TriageMode`, `TriageState` to `src/app.rs`
3. Call `ErrorDetector::check` in `handle_session_output` after pattern matching
4. Drive `triage_state` machine in `handle_session_output` state transitions
5. Add `TriageDetected` handler in main loop — spawns Haiku call if needed
6. Render triage overlay in `ui.rs`
7. Add `r`/`v`/`Esc` key handlers under triage overlay mode
8. Add `:triage` command parser in `execute_command`

---

## Open Questions

- Should the agent suppress repeat fires for the same session within a cooldown window (e.g. 30 seconds)? Avoids spamming when a session retries in a loop.
- Should `auto` mode be gated behind a session cost threshold? (Don't auto-open Claude sessions if spend is already high.)
- For Python tracebacks, should the full traceback always be included or just the final exception line + 5 lines of context?
