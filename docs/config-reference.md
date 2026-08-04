# Configuration reference

Linkshell reads TOML from the path reported by `config path`. Omitted values
use these defaults.

## `[general]`

`max_ipc_message_bytes = 0`, `scroll_buffer_lines = 2000`,
`tick_interval_ms = 100`, `ipc_state_override_timeout_secs = 60`,
`menu_key = "ctrl+space"`, `status_panel = "left"`, and
`status_panel_width = 28`.

`status_panel` is `"left"` (a permanent sidebar), `"bottom"` (the always-on
region below the output), `"overlay"` (alt-s only, claims no layout space) or
`"off"`. `status_panel_width` is the sidebar's width in columns, clamped to
16–60; below that width plus 60 columns of terminal the sidebar collapses to a
narrow rail. See [Status panel](panes-and-navigation.md#status-panel).

## `[theme]`

Every colour the UI draws with. `base` picks a palette; any individual field
overrides it with a `#rrggbb` hex string.

```toml
[theme]
base = "dark"          # "classic" | "dark" | "ansi16"
accent = "#5fb3d4"
```

- `classic` — the palette linkshell has always shipped.
- `dark` — the restyle palette: desaturated chrome, one accent colour used for
  focus and nothing else.
- `ansi16` — named ANSI colours only, so a 16- or 256-colour terminal renders
  *your* colour scheme instead of a quantized approximation of a truecolor one.

`base` unset auto-detects: `classic` when `COLORTERM` contains `truecolor` or
`24bit`, `ansi16` otherwise. `TERM` is not consulted — it reads
`xterm-256color` on nearly everything, truecolor-capable or not. Run
`linkshell doctor` to see which base resolved and why.

Overridable fields: `bg`, `surface`, `chrome`, `text`, `text_dim`,
`text_bright`, `accent`, `warn`, `err`, `ok`, `info`, `ctx`, `cost`, `pipe`,
`on_accent`, `sel_bg`, and the per-agent `kind_claude`, `kind_codex`,
`kind_opencode`, `kind_ohmypi`, `kind_aider`, `kind_shell`, `kind_custom`,
`kind_orch`. An unparseable value is reported on stderr and ignored.

## `[socket]`

`path = "/tmp/linkshell-{pid}.sock"`.

## `[sessions]`

`default_cwd` defaults to the process directory. `[sessions.commands]` uses
`claude = "claude"`, `codex = "codex"`, `opencode = "opencode"`,
`ohmypi = "omp"`, `aider = "aider"`, and an empty `shell` for `$SHELL`.
`[sessions.aliases.<name>]` accepts `kind` and optional `config_dir`.

## `[pipe.summarize]`

Defaults: `model = "claude-haiku-4-5-20251001"`, `max_tokens = 150`,
`cooldown_secs = 2`, and a terse extraction `system` prompt.

## `[pricing]`

`[pricing.claude.<model-prefix>]` and `[pricing.codex.<model-prefix>]` accept
`input`, `cache_write`, `cache_read`, and `output` rates. Longest prefix wins.

## `[keybindings]`

`[keybindings.vars]` defines chord fragments; `[keybindings.bind]` maps chords
to actions.

## `[notifications]`

Defaults: `enabled = true`, `on_states = ["waiting", "error"]`,
`method = "auto"`, `min_session_age_secs = 10`, and `debounce_secs = 30`.
Methods are `auto`, `osc9`, `notify-send`, `bell`, and `none`.

## `[chat]`

- `width_pct` (default `60`) — chat popup width as a percentage of the terminal width (20–95).
- `height_pct` (default `60`) — chat popup height as a percentage of the terminal height (20–95).

## `[orchestrator]` events

- `events` (default `["ready", "waiting", "error", "dead"]`) — session state changes forwarded to the orchestrator agent as `[linkshell event]` messages. `ready` fires when a session finishes (goes quiet); remove it if completion events are too chatty for your workflow.
- `event_cooldown_secs` (default `30`) — minimum seconds between events for the same (session, state) pair.

## `[orchestrator]` agent memory and skills

Both live under `~/.config/linkshell/` by default and are created
automatically the first time an orchestrator starts — no configuration
needed. Override the locations with `skills_dir` and `memory_file` (both
accept `~`).

- **Skills** (`~/.config/linkshell/skills/`) — one `.md` file per skill;
  the name + description go in the prompt, the body loads on demand via
  `use_skill` (API class) or by path (CLI class).
- **Memory** (`~/.config/linkshell/memory.md`) — durable notes injected
  into the orchestrator's prompt verbatim every turn. The agent appends
  dated bullets with its `remember` tool; you curate the file by hand.
  Keeping it concise is on you: past 8 KiB it is truncated in the prompt
  and the agent is told to ask you to prune. `remember` is in the default
  `auto_approve` set (it writes only to this file).

## `[orchestrator]` approval (propose mode)

- `approval` (default `"auto"`) — `"auto"` runs the orchestrator's tool calls
  immediately. `"propose"` holds gated tool calls as proposals: the chat pane
  shows `⏸ agent proposes send_input: session 2 ← "cargo test"` and the
  agent's turn blocks until you answer with `/approve` or `/deny [reason]`.
  A deny reason is returned to the model as the tool result, so it can
  course-correct within the same turn. From the model's perspective approval
  is just a slow tool — its context stays coherent, which matters for local
  models. While a proposal is pending the orchestrator processes nothing
  else; incoming events coalesce into its next turn.
- `auto_approve` (default `["list_sessions", "read_output", "use_skill"]`) —
  tools that skip the gate. The default set is read-only, so routine
  observation stays fluid and only session-mutating calls (`start_session`,
  `send_input`) interrupt you. `kill_session` always uses its own
  `/confirm-kill` flow and is never double-gated.
- `approval_timeout_secs` (default `600`) — an unanswered proposal resolves
  as denied ("no response from user") after this long, and the agent's turn
  continues; a proposal you never saw cannot wedge the orchestrator.

## `[orchestrator]` limits

- `max_tool_iterations` (default `12`) — tool-call loop budget per turn. When
  it runs out the agent gets one final tool-free response to wrap up, so the
  turn lands softly instead of being cut mid-plan. Raise it for local models
  that take smaller steps per call.
- `max_history_turns` (default `40`) — conversation turns kept in the agent's
  context before older ones are dropped.
- `max_tokens` (default `4096`) — per-response output token cap.
- `input_wait_timeout_secs` (default `180`) — how long a `send_input` tool
  call with `wait_ready` waits for the target session to finish before
  returning.

## `[[profiles]]`

Profiles contain `name`, `[[profiles.sessions]]`, and `[[profiles.pipes]]`.
Session fields are `kind`, `command`, `name`, `cwd`, and `group`; pipe fields
are `source`, `dest`, `trigger`, `extract`, and `prefix`.

## `[agents.<name>]`

Local LLMs require `endpoint` and `model`; `system` and `api_key` are optional.


- `tool_dedup_secs` (default `45`) — repeat-call suppression window for the orchestrator's tools. `0` disables.

## `[[personas]]`

Named behavioural presets layered over `[orchestrator]`. Every field is
optional; omitted fields inherit from `[orchestrator]`. Set
`[orchestrator].persona` to pick the one applied at startup, or switch at
runtime with `/persona <name>` (history is preserved).

Personas modulate autonomy and eagerness only. Repeat-call suppression,
tool-result elision stubs and `send_input` evidence are unconditional — a
persona cannot turn correctness off.

- `name` — the name used by `/persona`. Matching a builtin replaces it.
- `events`, `event_cooldown_secs` — which session transitions wake the agent, and how often.
- `approval`, `auto_approve` — propose-mode gating.
- `allowed_tools` — tools present in the schema at all. `list_sessions` is always kept.
- `max_tool_iterations`, `tool_dedup_secs`, `max_context_tokens`, `event_tail_lines`.
- `note` — appended to the system prompt under a `## Persona` heading.

Builtins: `assistant` (reactive, read-only, propose), `monitor` (watches and
reports, writes gated), `orchestrator` (acts autonomously, tightest dedup
window).

```toml
[orchestrator]
persona = "monitor"

[[personas]]
name = "monitor"
event_cooldown_secs = 90
note = "Be terse. One report per incident."
```
