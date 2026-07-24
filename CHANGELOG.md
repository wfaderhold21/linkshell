# Changelog

## Unreleased

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

