# Configuration reference

Linkshell reads TOML from the path reported by `config path`. Omitted values
use these defaults.

## `[general]`

`max_ipc_message_bytes = 0`, `scroll_buffer_lines = 2000`,
`tick_interval_ms = 100`, `ipc_state_override_timeout_secs = 60`, and
`menu_key = "ctrl+space"`.

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

## `[[profiles]]`

Profiles contain `name`, `[[profiles.sessions]]`, and `[[profiles.pipes]]`.
Session fields are `kind`, `command`, `name`, `cwd`, and `group`; pipe fields
are `source`, `dest`, `trigger`, `extract`, and `prefix`.

## `[agents.<name>]`

Local LLMs require `endpoint` and `model`; `system` and `api_key` are optional.

