# Sessions

Linkshell runs up to 8 concurrent PTY sessions — Claude, Codex, local agents,
shells, or any custom command — as a tmux-style client/server pair. The server
owns the sessions and survives detach; the foreground TUI is just a client.

- [Starting & managing sessions](#starting--managing-sessions)
- [Detach, reattach & multiple linkshells](#detach-reattach--multiple-linkshells)
- [Startup profiles](#startup-profiles)
- [Aliased Claude / Codex sessions](#aliased-claude--codex-sessions)
- [Local agent sessions](#local-agent-sessions)
- [Session states](#session-states)

## Starting & managing sessions

Create your first session with `alt-n` (interactive dialog) or from the command
bar (`alt-c`):

```
new claude [name]     Start a Claude session
new codex [name]      Start a Codex session
new shell [name]      Start a shell session
new <cmd> [name]      Start a single-word command as a session
new custom <cmd...>   Start a full command line (spaces, env prefixes) as a session
kill [n]              Kill the active session (or session n)
pause [n]             Pause a session's process (SIGSTOP) — keeps context, frees CPU
resume [n]            Resume a paused session (SIGCONT)
restart [n]           Respawn a session with the same command, name, and cwd
```

In the `alt-n` dialog, use arrow keys or `1`–`4` to pick the session type,
`tab` to move between fields, `enter` to create.

Sessions are color-coded by type: 🟠 orange for Claude, 🔵 blue for Codex.

## Detach, reattach & multiple linkshells

Linkshell runs as a client/server pair, like tmux/screen: each `linkshell`
starts a background server that owns its sessions, and the foreground TUI is a
client attached to it. `alt-d` detaches — sessions keep running.

You can run **multiple independent linkshells** on one machine, screen-style:

```bash
linkshell                 # start a new detached server and attach
linkshell new work        # start a new one named "work"
linkshell ls              # list detached sessions (id, name, pid, status)
linkshell -r <id>         # reattach to a specific session by id
linkshell -r              # reattach when exactly one session is running
```

Each server has its own id, pid, and sockets; `linkshell ls` prunes any whose
process has died.

If something looks wrong (missing logs, stale socket, nested multiplexer,
limited terminal colors), run `linkshell doctor` for a diagnostic report.

## Startup profiles

Save a layout of sessions and pipes and relaunch it later:

```
profile save <name>          # from the command bar
linkshell --profile <name>   # relaunch it at startup
```

## Aliased Claude / Codex sessions

Sessions running Claude or Codex under a different config home are recognized
and get the full treatment — state inference patterns, the JSONL token/cost
watcher, and pricing — instead of being treated as generic custom commands.

Two ways to spell them:

**Inline env prefix** — just works, no config needed. The classifier sees
through leading `VAR=value` assignments, and the watcher reads the config home
from the command itself:

```
new custom CLAUDE_CONFIG_DIR=~/.claude-work claude
new custom CODEX_HOME=~/.codex-personal codex
```

(or enter the same command line in the `alt-n` dialog's Custom field)

**Config alias** — for wrapper scripts or shell aliases whose name doesn't
contain `claude`/`codex`. Map the command basename in `[sessions.aliases]`:

```toml
[sessions.aliases.claude-work]
kind = "claude"
config_dir = "~/.claude-work"     # exported as CLAUDE_CONFIG_DIR

[sessions.aliases.cx]
kind = "codex"
config_dir = "~/.codex-personal"  # exported as CODEX_HOME
```

When `config_dir` comes from an alias it is also injected into the session's
environment, so the CLI and the log watcher agree on where the config home is.
Precedence for the watcher: inline env prefix → alias `config_dir` →
`$CLAUDE_CONFIG_DIR`/`$CODEX_HOME` in linkshell's own environment → the default
`~/.claude` / `~/.codex`.

## Local agent sessions

Sessions running `opencode`, `omp` (oh-my-pi), `pi`, `aider`, `llama-cli`, or
`ollama` are recognized as local agents: they get agent-style state inference
(THINKING on spinners/working verbs, READY on idle prompts) and terminal-based
token scraping. Wrappers with other names can be mapped with `kind = "local"`
in `[sessions.aliases]`.

## Session states

States are inferred from PTY output and refined by JSONL log activity.

| State | Meaning | Border |
|-------|---------|--------|
| STARTING | Process spawning | — |
| READY | Prompt detected, waiting for input | — |
| THINKING | AI model is processing | — |
| RUNNING | Active output streaming | — |
| WAITING | Agent asked you something, blocked | 🟡 yellow |
| ERROR | Error pattern detected or process crashed | 🔴 red flash |
| DEAD | Process exited | — |

Desktop notifications for WAITING/ERROR are configurable via notify-send, OSC 9,
or bell. See the [configuration reference](config-reference.md).
