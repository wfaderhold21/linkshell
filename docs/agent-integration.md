# Agent Integration

Sessions can drive linkshell and talk to each other over a typed IPC protocol.
Every connection is scoped by a capability set so agents get exactly the rights
they need — no more.

- [linkshell-ctl](#linkshell-ctl)
- [Capabilities](#capabilities)
- [Claude Code hooks](#claude-code-hooks)
- [Remote agents](#remote-agents)

## linkshell-ctl

Every spawned session gets three environment variables set automatically:

```bash
LINKSHELL_SESSION_ID=3          # this session's id
LINKSHELL_SOCK=/run/user/1000/linkshell/12345.sock
LINKSHELL_TOKEN=<hex>           # capability token binding this session's rights
```

`linkshell-ctl` picks these up automatically (it presents the token in its
handshake, so a connection from inside a session carries that session's
capabilities). Outside any session it falls back to the last daemon socket
recorded in `~/.config/linkshell/last_socket`, or `$LINKSHELL_SOCK`.

```bash
linkshell-ctl list                    # JSON snapshot of all sessions (incl. cwd)
linkshell-ctl state READY             # signal done; fires OnReady pipes
linkshell-ctl state THINKING          # signal working
linkshell-ctl output "step done"      # inject a line into this session's display
linkshell-ctl send [--wait] <name> <msg...>   # direct-message another agent
linkshell-ctl wait-ready <id> [--timeout=N]   # block until session <id> returns to READY
linkshell-ctl pipe list / add / remove / fire # manage pipes (operator capability)
linkshell-ctl new <kind> [name] [--cwd=PATH]  # start a session (operator capability)
linkshell-ctl input <id> <text...> [--wait]   # type into a session; --wait returns its answer
linkshell-ctl read <id> [n]                   # last n output lines of a session
linkshell-ctl chat <msg...>                   # post a line into the chat pane
linkshell-ctl kill <id> [reason...]           # request a kill; the user must /confirm-kill
```

## Capabilities

Every IPC connection is scoped by a capability set, resolved at handshake:

| Tier | Who gets it | Can do |
|------|-------------|--------|
| operator | the human (same-uid Unix peer without a token), shell sessions, headless registrations | everything, incl. `session_create`, `session_input_wait`, pipe management |
| worker | spawned Claude/Codex/custom sessions (via `LINKSHELL_TOKEN`) | report state/tokens/output, query, direct-message, fire pipes |
| council | council members | report their own state only |
| orchestrator | the resident CLI-class orchestrator session | same as operator (incl. `chat_post` and `session_kill_request`; kills still require human `/confirm-kill`) |

TCP connections must present a valid token; tokenless TCP is rejected.

## Claude Code hooks

Auto-signal state without changing your prompts:

```json
{
  "hooks": {
    "Stop": [{ "command": "linkshell-ctl state READY" }],
    "PreToolUse": [{ "command": "linkshell-ctl state THINKING" }]
  }
}
```

## Remote agents

With `--tcp`, remote agents connect over the network using the same typed JSONL
protocol as the Unix socket. Every message travels in an envelope —
`{"msg": {...}}`, plus an `"id"` on requests that expect a reply — and every
connection starts with a `hello`/`welcome` handshake. TCP requires a token
(mint one by spawning the agent locally, or register headlessly over Unix
first); same-uid Unix connections without a token get operator rights.

```python
import socket, json

s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
s.connect(("host", 7373))
f = s.makefile("rwb")

def send(msg, req_id=None):
    env = {"msg": msg} if req_id is None else {"id": req_id, "msg": msg}
    f.write(json.dumps(env).encode() + b"\n"); f.flush()

# Handshake — name registers a headless session slot (Unix); TCP needs token
send({"type": "hello", "protocol": 1, "token": TOKEN, "name": "remote-claude"})
welcome = json.loads(f.readline())["msg"]     # session_id, capabilities

# Signal state
send({"type": "state", "state": "THINKING"})

# Synchronous query (note the id)
send({"type": "query", "what": "sessions"}, req_id=1)
sessions = json.loads(f.readline())

# Receive pipe relay content
env = json.loads(f.readline())
if env["msg"]["type"] == "relay":
    process(env["msg"]["content"])
```

Message types: `hello`, `state`, `tokens`, `output`, `agent_send`, `broadcast`,
`fire_pipe`, `pipe_add`, `pipe_remove`, `session_create`, `session_input_wait`,
`query` — each gated by the connection's capabilities. Server→agent messages:
`welcome`, `relay`, `reply`, `error`.
