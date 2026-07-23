# Pipes

Pipes forward a snapshot of one session's output to another when a trigger
fires. They are **edge-triggered on state change, not continuous** — when a
source session hits a trigger state (`OnReady`, `OnWaiting`, or `Manual`),
linkshell extracts a snapshot and forwards it to the destination's PTY.

```
pipe 1 2                          on READY, forward last code block
pipe 1 2 --on=waiting             fire when session 1 hits WAITING
pipe 1 2 --extract=last-n=20      last 20 lines instead of last block
pipe 1 2 --extract=diff           lines starting with + or -
pipe 1 2 --summarize=150          relay through Haiku first, max 150 tokens
pipe 1 2 --prefix="Review this:"  prepend text to the forwarded content
pipe fire 1 2                     manually fire a --on=manual pipe
unpipe 1                          remove all pipes from session 1
unpipe 1 2                        remove the specific 1→2 pipe
pipes                             inspect, pause, fire, or delete configured pipes
```

Active pipes show as `→ N` in the status panel, bold for one tick when they
fire. The `--summarize` mode relays through Haiku (`claude-haiku-4-5-20251001`)
to compress the snapshot first.
