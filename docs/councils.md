# Councils

A council is a declarative multi-agent topology defined in a TOML file: named
agents plus routes that relay output between them on state transitions
(`ready`/`waiting`), with `join = "all"` fan-in, extraction modes, round limits,
and an optional `done_signal` for early termination. See
[`examples/council.toml`](../examples/council.toml) for a fully commented
author/critic review loop.

Launch one at startup with `--council <file>` or at runtime from the command bar
(`alt-c`):

```
council <file.toml>   # spawn the agents and start routing
council status        # current round / completion state
council stop          # detach the router; sessions keep running
```

Council members are spawned with the minimal `SignalState` capability — they can
report their own state but cannot inject input, manage pipes, or create
sessions. Live progress (`round R/M`, done) is shown in the Status panel title.
