# Changelog

## Unreleased

- Capability tokens are no longer minted from a silently-failed CSPRNG read. If `/dev/urandom` could not be opened or read the error was discarded and the buffer kept its zero initializer, so every token became 32 zeros — a predictable credential for reattach and for TCP agents. `mint_token` now propagates the failure, which surfaces as a session-spawn error rather than a weak token. Most likely to have bitten agents run inside a container or bubblewrap sandbox with no `/dev` bound.
- A panic no longer leaves the terminal unusable. The relay client installs a panic hook that leaves the alternate screen, disables raw mode and mouse reporting, and pops kitty flags before the panic report prints, so the message lands on the normal screen instead of a shell with no echo. `SIGTERM`/`SIGHUP` are now handled the same way as a detach rather than killing the client mid-alternate-screen.
- Fixed a crash when the command bar's slash-command popup was open on a short terminal: the popup claimed one row per match (up to 8) without checking how many rows were left above the bar, underflowing the row calculation and panicking inside ratatui's buffer indexing. The popup now yields rows to the bar, and terminals below 20x12 render a "terminal too small" placeholder instead of attempting a layout the solver can't satisfy.
- Fixed the client hanging after detach until an extra keypress, which was then swallowed. The stdin reader runs as a blocking task that can't be cancelled, and dropping the tokio runtime waits for it; the client now exits directly once the terminal is restored.
- Fixed `ctrl`/`alt` chords being typed as literal characters in the chat pane, command bar, new-session dialog, and settings editor — crossterm reports `ctrl-c` as `Char('c')` with a modifier set, so it inserted a `c`. Shifted characters are still text. Search mode already had this guard.
- Tokenless IPC clients are now rejected on platforms without `SO_PEERCRED` (macOS, BSD) instead of being granted operator capabilities. The "same-uid peer is the operator" shortcut is only sound where the kernel can attest the peer's uid; elsewhere a connection is anonymous and must present a token, as TCP already did. Relatedly, the default socket path now falls back to `TMPDIR` off Linux rather than `/run/user/<uid>`, which doesn't exist there.
- A pipe's `Summarize` relay no longer stalls indefinitely against an unresponsive endpoint; the request now carries a 60s timeout, matching the bounds the orchestrator paths already set.
- Fixed Escape keypresses being swallowed when followed quickly by another key (crossterm merges them into Alt+char): the ESC prefix is now forwarded to the PTY. Most visible in vim, where Esc then `:wq` typed fast left the session in insert mode with `:wq` inserted into the buffer.
- `PageUp`/`PageDown` now scroll linkshell's captured scrollback in claude/codex panes (matching the mouse wheel) instead of being sent to the TUI, which ignored them.
- Fixed the whole UI shaking when a codex session flapped in and out of WAITING: the status panel's waiting-preview row now shrinks with a few seconds of hysteresis, breaking the resize→repaint→state-flap feedback loop.
- Fixed missing token/context stats for resumed codex sessions (`codex resume` or the in-TUI picker): the rollout watcher now also picks up a pre-existing rollout file that starts being written after the session spawns.
- Chat pane input: `Home`/`End`/`Delete` now work; `Up`/`Down` recall previously sent messages (draft preserved); typing `/` as the first character opens a filtering command popup (`Up`/`Down` select, `Tab` completes).
- Added recursive split panes: any pane can be split side by side (`alt-\`) or top/bottom (`alt--`), repeatedly and in any direction, for arbitrary tiled layouts. `alt-w` closes the focused pane (its sibling reclaims the space), `alt-r` rotates a split, `alt-o` cycles focus. Replaces the previous two-pane toggle; keybinding actions are now `split_pane_right`, `split_pane_down`, `close_pane`, `rotate_split`, `focus_next_pane`.
- Added screen-style multi-session support: each `linkshell` starts its own detached server, `linkshell ls` lists live sessions (id, name, pid, status), and `linkshell -r <id>` reattaches to a specific one. `linkshell new [name]` names a session; `linkshell -r` with no id attaches the sole running session.
- Fixed 100% CPU usage caused by full-screen agent TUIs (notably OpenCode) that repaint continuously: session output now triggers a redraw only when a visible session's screen actually changes, and the partial-line heartbeat only when a session's inferred state changes.
- Added startup profiles and live profile saving.
- Added split panes with independent focus and PTY sizing.
- Added a fuzzy command palette and session-name completion.
- Added pipe topology summaries and an interactive `pipes` overlay.
- Added WAITING previews and debounced desktop notifications.
- Added diagnostics, recipes, and configuration documentation.

