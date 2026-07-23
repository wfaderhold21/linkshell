# Panes, Navigation & Keybindings

- [Split panes](#split-panes)
- [Scrollback](#scrollback)
- [Status panel](#status-panel)
- [Keybindings](#keybindings)
- [Command bar](#command-bar)

## Split panes

Split any pane side by side (`alt-\`) or top/bottom (`alt--`), repeatedly and in
any direction, for arbitrary tiled layouts. The session bar is centered and
reflows based on how many sessions are open.

| Key | Action |
|-----|--------|
| `alt-\` | Split focused pane side by side |
| `alt--` | Split focused pane top/bottom |
| `alt-w` | Close focused pane (sibling reclaims the space) |
| `alt-r` | Rotate the focused pane's split direction |
| `alt-o` | Focus next pane |

## Scrollback

`alt-shift-PageUp/PageDown` (and `alt-shift-↑/↓`) scroll every session type the
same way. Shells use the terminal's native scrollback; full-screen TUIs (claude,
codex, opencode) scroll through linkshell's captured line history, shown dimmed.
The view holds position while new output streams in — typing returns you to the
live tail.

Mouse text selection works everywhere: drag to select, auto-copies to clipboard.

## Status panel

Each session gets one row:

```
1 🟠 →2   THINKING  1m 32s  │  ~450 tok  │  ~$0.02
```

`→2` means this session has an active pipe to session 2. The arrow goes bold for
one second when the pipe fires. Token counts and cost come from the JSONL logs
written by Claude and Codex — not from screen scraping. Shell and custom
sessions show `—`. On Pro/Max subscriptions, linkshell detects the subscription
and shows real token counts while skipping meaningless cost.

## Keybindings

| Key | Action |
|-----|--------|
| `alt-n` | New session dialog |
| `alt-c` | Open command bar |
| `alt-t` | Toggle agent chat pane |
| `alt-h` | Toggle help |
| `alt-x` | Kill active session |
| `alt-d` | Detach (sessions keep running) |
| `alt-\` | Split focused pane side by side |
| `alt--` | Split focused pane top/bottom |
| `alt-w` | Close focused pane (sibling reclaims the space) |
| `alt-r` | Rotate the focused pane's split direction |
| `alt-o` | Focus next pane |
| `alt-b` | Toggle broadcast input to all sessions |
| `alt-g` | Dock the chat pane |
| `alt-1` … `alt-8` | Switch to session by number |
| `alt-←` / `alt-→` | Cycle sessions |
| `ctrl-q` | Quit (shuts down the server and all sessions; use `alt-d` to leave them running) |
| `esc` | Dismiss overlay |
| `alt-shift-PageUp/PageDown` | Scroll output (page) |
| `alt-shift-↑` / `alt-shift-↓` | Scroll output (line) |

All other input is passed through to the active session's PTY. Keybindings are
configurable — see the [configuration reference](config-reference.md).

## Command bar

Press `alt-c` to open. Available commands:

```
new claude [name]     Start a Claude session
new codex [name]      Start a Codex session
new shell [name]      Start a shell session
new <cmd> [name]      Start a single-word command as a session
new custom <cmd...>   Start a full command line (spaces, env prefixes) as a session
kill                  Kill the active session
kill <n>              Kill session by number
pause [n]             Pause a session's process (SIGSTOP) — keeps its context, frees CPU
resume [n]            Resume a paused session (SIGCONT)
council <file.toml>   Launch a multi-agent council
council status        Show council round / completion state
council stop          Detach the council router (sessions keep running)
restart [n]           Respawn a session with the same command, name, and cwd
profile save <name>   Save the current sessions and pipes as a startup profile
grant <n> <tier>      Set a session's IPC capabilities (operator|worker|council)
config path           Show the config file location
config edit           Open the config in $EDITOR (as a session)
config reload         Re-read linkshell.toml without restarting
pipe <src> <dst> [--extract=last-block|last-n=N|diff] [--summarize=N] [--on=ready|waiting|manual] [--prefix="..."]
                      Forward output from src to dst on state change
pipe fire [src] [dst] Manually fire a pipe with trigger=manual
unpipe <src> [dst]    Remove pipe(s) from src
pipes                 Inspect, pause, fire, or delete configured pipes
detach                Detach the client; the server and sessions keep running
quit                  Exit linkshell (shuts down the server and all sessions)
```
