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

## `[[profiles]]`

Profiles contain `name`, `[[profiles.sessions]]`, and `[[profiles.pipes]]`.
Session fields are `kind`, `command`, `name`, `cwd`, and `group`; pipe fields
are `source`, `dest`, `trigger`, `extract`, and `prefix`.

## `[agents.<name>]`

Local LLMs require `endpoint` and `model`; `system` and `api_key` are optional.

