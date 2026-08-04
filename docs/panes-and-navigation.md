# Panes, Navigation & Keybindings

- [Split panes](#split-panes)
- [Tab strip](#tab-strip)
- [Footer](#footer)
- [Scrollback](#scrollback)
- [Status panel](#status-panel)
- [Keybindings](#keybindings)
- [Command bar](#command-bar)

## Split panes

Split any pane side by side (`alt-\`) or top/bottom (`alt--`), repeatedly and in
any direction, for arbitrary tiled layouts.

Panes have no boxes. Each carries a title line and a one-column bar down its
left edge, and that bar does double duty — `▎` in the accent colour marks the
focused pane, `│` in chrome separates a pane from its neighbour — so a split
needs neither per-pane borders nor a separate divider:

```
▎ ● alpha · ~/src/linkshell                    │ ○ beta · /tmp
▎ $ cargo test                                 │ $
```

A filled dot and a bright name mean focused; hollow and dim mean not. A pane
scrolled off the live tail says so in its title (`↑12`). The docked chat and
planning panes wear the same chrome; the floating chat overlay keeps its box,
because a floating thing needs an edge to read as floating.

| Key | Action |
|-----|--------|
| `alt-\` | Split focused pane side by side |
| `alt--` | Split focused pane top/bottom |
| `alt-w` | Close focused pane (sibling reclaims the space) |
| `alt-r` | Rotate the focused pane's split direction |
| `alt-o` | Focus next pane |

## Tab strip

One row above the output pane names every visible session:

```
 1 alpha  2 beta!  3 gamma✕
```

The active tab is highlighted; a tab underlined is showing in some other split
pane. The suffix glyph is the session's state — `!` for WAITING, `✕` for ERROR
or a dead session, `⏸` for paused, nothing otherwise — so "which agent wants
me" is legible without the status panel open. Click a tab to focus it.

As the terminal narrows the strip drops names before it drops tabs: first the
inactive tabs fall back to bare indices, then all of them do. Every session
keeps a tab.

## Footer

The bottom row reports the focused session:

```
 claude · opus-4-8   18.0k/180k   ⣾ THINKING   0:42   $0.31   alt-h help
```

The name is coloured by state — accent while working, amber on WAITING, red
on ERROR — and a dead session offers `alt-r restart`. The context counter
turns amber past 80% of the window; when the window size is unknown (a local
model that was never probed) it renders a bare count with no denominator,
because there is no percentage to colour against. A paused or dead session
greys the whole row: its numbers are frozen.

Fields are dropped as the terminal narrows rather than wrapping, which would
cost a row of output. The hint goes first, then cost, then elapsed; the
session's name and state are the last to go.

## Scrollback

`alt-shift-PageUp/PageDown` (and `alt-shift-↑/↓`) scroll every session type the
same way. Shells use the terminal's native scrollback; full-screen TUIs (claude,
codex, opencode) scroll through linkshell's captured line history, shown dimmed.
The view holds position while new output streams in — typing returns you to the
live tail.

Mouse text selection works everywhere: drag to select, auto-copies to clipboard.

## Status panel

A permanent sidebar down the left, on by default. `alt-s` tucks it away and
brings it back.

```
 Status                    │▎ ● alpha · ~/src/linkshell
 ● alpha            1m32s  │▎ $ cargo test
   READY       18.0k/180k  │▎
   opus-4-8         $0.31  │▎
                           │▎
 ! beta               12s  │▎
   WAITING           2.1k  │▎
   sonnet-5         $0.04  │▎
                           │
 2 sess            $0.35   │
```

Each session is a stacked block: name and elapsed, state and context, then
model and cost. A session with no model (a shell) shows its working directory
instead — the thing that actually distinguishes it from the shell in the next
pane. Clicking a block focuses that session.

The sidebar costs **columns instead of rows**, which is the better trade on a
wide terminal, and it has the full column height — so unlike the bottom
region it doesn't run out of room at four sessions.

### When the terminal is narrow

Below `status_panel_width + 60` columns the sidebar collapses to a
three-column rail rather than starving the output pane:

```
●1 ▎ ● alpha · ~/src/linkshell
!2 ▎ $ cargo test
✕3 ▎
```

State still reaches you — which agent needs you is the sidebar's real job, and
a glyph plus an index still does it.

### Placement

```toml
[general]
status_panel = "left"       # "left" (default) | "bottom" | "overlay" | "off"
status_panel_width = 28     # columns for the sidebar
```

- **`left`** — the sidebar above.
- **`bottom`** — the always-on region below the output, in two-line rows.
  Its height is capped at a third of the terminal, so past four sessions it
  drops the detail line to keep every session visible.
- **`overlay`** — not docked; `alt-s` opens it centered over the output, which
  costs no layout space and resizes no PTY.
- **`off`** — never shown, and `alt-s` does nothing.

In `left` and `bottom`, `alt-s` hides and restores the panel. Note that this
resizes the sessions' PTYs, as any docked panel must; `overlay` is the
placement that never does.

Token counts and cost come from the JSONL logs written by Claude and Codex —
not from screen scraping. Shell and custom sessions show `—`. On Pro/Max
subscriptions, linkshell detects the subscription and shows real token counts
while skipping meaningless cost.

## Keybindings

| Key | Action |
|-----|--------|
| `alt-n` | New session dialog |
| `alt-c` | Open command bar |
| `alt-t` | Toggle agent chat pane |
| `alt-s` | Show/hide the status panel |
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
| `alt-p` | Toggle the planning pane |
| `alt-shift-p` | Planning pane over the whole output region (docks it first if closed) |
| `ctrl-space` | Toggle the menu bar |
| `alt-1` … `alt-8` | Switch to session by number |
| `alt-←` / `alt-→` | Cycle sessions |
| `ctrl-q` | Quit (shuts down the server and all sessions; use `alt-d` to leave them running) |
| `esc` | Dismiss overlay |
| `alt-shift-PageUp/PageDown` | Scroll output (page) |
| `alt-shift-↑` / `alt-shift-↓` | Scroll output (line) |

All other input is passed through to the active session's PTY. Keybindings are
configurable — see the [configuration reference](config-reference.md).

### Menu bar keys

`ctrl-space` opens a menu bar across the top (Sessions, View, Pipes,
Orchestrator, Agenda, Help). It is the runtime settings surface: the
Orchestrator section cycles persona, provider, model, context budget, approval
mode and the wake-on-event toggles, and the Agenda section opens the planning
backend/model picker, all without editing config.toml.

| Key | Action |
|-----|--------|
| `←` / `→` | Move between sections |
| `↓` | Drop into the section, then move down its items |
| `↑` | Move up; from the first item, back out to the section row |
| `Enter` | Activate the item. Value rows (model, context, toggles) cycle in place and leave the menu open |
| a letter | Jump to the section whose title starts with it |
| `ctrl-space` / `esc` | Close |

Rows whose action is unavailable are greyed out and do nothing rather than
closing the menu — the orchestrator lifecycle rows are greyed while it is not
running, and "Planning Model" is greyed when there is no endpoint to pick from.

The planning backend list does not have to be written out twice: every
`[agents.NAME]` endpoint, and an API-class `[orchestrator]`, are offered as
planning backends automatically under their own names. A
`[planning.backends.NAME]` entry with the same name shadows the derived one,
so declaring a backend explicitly is how you give it a different model or
context budget than the agent it came from. Derived backends are never written
back into config.toml.

### Planning pane keys

`alt-p` docks the planning pane into a split leaf; `alt-shift-p` gives it the
whole output region, docking it first if it was closed. A plan is a document,
and a third of a split is not enough room to hold one in your head. The session
bar and status panel stay visible, so the sessions you are planning against are
still on screen with their state and token counts updating — only their output
is covered, and their PTYs keep the size they last laid out at. `esc` steps
back to the split, and a second `esc` moves focus on.

While the pane is focused these keys apply instead of being passed to a PTY:

| Key | Action |
|-----|--------|
| `Enter` | Send the input as a planning turn |
| `alt-Enter` | Insert a newline (a planning message is usually a paragraph) |
| `Tab` | Move focus between the thread list and the transcript |
| `ctrl-b` | Collapse/expand the thread list |
| `alt-m` | Open the backend/model picker |
| `ctrl-k` | Commit the thread to a plan revision |
| `alt-i` | Hand the committed plan to a session as work |
| `↑` / `↓` | Thread list: change selection. Transcript: scroll |
| `PageUp` / `PageDown` | Scroll the transcript |
| `Enter` (list focus) | Open the selected thread |
| `n` (list focus) | New thread — browse for its scope root |
| `d` (list focus) | Delete the selected thread (confirms first) |
| `esc` | Leave fullscreen, else return focus to the other pane |

The backend picker is `alt-m` rather than `ctrl-m` because terminals encode
`ctrl-m` as carriage return, making it indistinguishable from `Enter`.

It has two levels. The first lists backends — endpoints. On opening, every
self-hosted endpoint is asked what it is currently serving (`GET /v1/models`,
2s timeout, cached), and `Enter` or `→` on one descends into that answer to
pick a model; `←` backs out, `r` re-probes. Hosted catalogues
(`api.openai.com`, Anthropic) are not asked — they list hundreds of models and
are not what changes under you — so `Enter` there selects the endpoint on its
configured model, as does an endpoint that did not respond. A model picked
this way applies to the next turn and is recorded on each message, so a thread
shows where it switched models.

`alt-i` hands the thread's latest committed plan to a session you pick. What
is sent is a path plus a staleness warning, not the conversation: an
implementation session may run sandboxed, and a read-only bind mount of one
file is simpler to arrange than replaying a thread. Staleness is recomputed at
handoff, not reused from commit time, so a plan grounded in files that have
since moved on says so. The brief queues if the target session is mid-turn.

The status row's context meter shows two numbers: `~868/131k (peak 5.4k)`. The
first is the thread transcript plus your draft — what the *next* turn starts
from. The peak is the largest request the last turn actually built, including
the file contents it read. They diverge sharply, because tool results are
consumed within a turn and never stored in the thread, so reading a codebase
moves the peak and barely touches the transcript. The peak is not restored when
a thread is reopened; it describes a turn, not a thread.

When a thread exceeds the selected backend's context budget the pane shows a
prompt offering `[c]` compact, `[b]` switch backend, or `[Esc]` dismiss. This
is deliberately not automatic: compacting silently would drop the early turns,
which in a planning thread are usually the premises everything else rests on.

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
