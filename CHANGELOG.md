# Changelog

## Unreleased

- Fixed 100% CPU usage caused by full-screen agent TUIs (notably OpenCode) that repaint continuously: session output now triggers a redraw only when a visible session's screen actually changes, and the partial-line heartbeat only when a session's inferred state changes.
- Added startup profiles and live profile saving.
- Added split panes with independent focus and PTY sizing.
- Added a fuzzy command palette and session-name completion.
- Added pipe topology summaries and an interactive `pipes` overlay.
- Added WAITING previews and debounced desktop notifications.
- Added diagnostics, recipes, and configuration documentation.

