---
title: Controlling Giant
description: Drive builds from your own code over the NDJSON protocol.
---

Giant's core is a build engine you control over a protocol. The CLI you
type is one client; a TUI, an IDE extension, a CI dashboard, a desktop app,
or a web backend are all equally valid clients. None of them link Giant's
code - they spawn a process and speak [NDJSON](/reference/events/).

There are two ways in.

## One-shot: read a build's events

The simplest integration. Run a build with `--events ndjson` and read the
stream off stdout. Stateless, no handshake, nothing to manage.

```bash
giant build //src/... --events ndjson \
  | jq -c 'select(.t == "target.finished") | {id, result, duration_ms}'
```

```json
{"id":"//src/auth:auth","result":"cache_hit","duration_ms":2}
{"id":"//src/core:core","result":"built","duration_ms":1240}
```

Good for a status line, a CI summary, a commit hook. Each line is one JSON
object; match on `t` and render. About 30 lines of code in any language.

## Session: a warm engine you command

For anything interactive or long-lived - rebuild on demand, watch a
selection, answer "why did this rebuild?" without re-reading config every
time - spawn **`giant session`**. The engine loads config once, emits its
catalog, then accepts commands on stdin and streams events on stdout, both
NDJSON.

The handshake on startup:

1. `engine.hello` - version, protocol, `capabilities`.
2. one `target.described` per target (the catalog).
3. `engine.ready` - now it will accept commands.

Then it's request/response, correlated by `command_id`: you send `{"c":
"build", "command_id": "c_1", ...}`, and the `build.*` / `target.*` events
that come back are the answer. A `command.accepted` / `command.rejected`
acknowledges each command.

### A minimal session client (Node)

```js
import { spawn } from "node:child_process";
import readline from "node:readline";

const giant = spawn("giant", ["session", "--events", "ndjson"], {
  stdio: ["pipe", "pipe", "inherit"],
});

const events = readline.createInterface({ input: giant.stdout });
const send = (cmd) => giant.stdin.write(JSON.stringify(cmd) + "\n");

let seq = 0;
const nextId = () => `c_${++seq}`;

for await (const line of events) {
  const ev = JSON.parse(line);
  switch (ev.t) {
    case "engine.ready":
      // Catalog is in; kick off a build.
      send({ c: "build", command_id: nextId(), targets: ["//src/..."] });
      break;
    case "target.finished":
      console.log(`${ev.result.padEnd(16)} ${ev.id}  ${ev.duration_ms}ms`);
      break;
    case "build.finished":
      send({ c: "shutdown", command_id: nextId() }); // closing stdin works too
      break;
  }
}
```

### The same shape in Python

```python
import json, subprocess

giant = subprocess.Popen(
    ["giant", "session", "--events", "ndjson"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
)

def send(cmd):
    giant.stdin.write(json.dumps(cmd) + "\n")
    giant.stdin.flush()

seq = 0
def next_id():
    global seq; seq += 1; return f"c_{seq}"

for line in giant.stdout:
    ev = json.loads(line)
    if ev["t"] == "engine.ready":
        send({"c": "build", "command_id": next_id(), "targets": ["//src/..."]})
    elif ev["t"] == "target.finished":
        print(f'{ev["result"]:<16} {ev["id"]}  {ev["duration_ms"]}ms')
    elif ev["t"] == "build.finished":
        send({"c": "shutdown", "command_id": next_id()})
```

## Asking without building

A session answers read-only queries - no build runs. Check
`engine.hello.capabilities` first, then send:

```jsonc
// "is this target cached, and at what key?"
{ "c": "query.status", "command_id": "q1", "targets": ["//src/core:core"] }

// "what feeds this target's cache key?" (structured `giant explain`)
{ "c": "query.explain", "command_id": "q2", "target": "//src/core:core" }

// "what did its last build print?"
{ "c": "logs.get", "command_id": "q3", "target": "//src/core:core" }
```

The replies (`query.status`, `query.explained`, `logs.line` + `logs.end`)
carry the same `command_id`. This is how the `giant-explain` and `giant-logs`
porcelains work - each is a thin client over these queries that shares the
CLI's code path.

## Live config and watch

A session watches `giant.yaml` and re-emits its catalog on edits, bracketed
by `catalog.invalidating` / `catalog.ready` - so a UI's target list stays
current without a restart. To follow a selection, send `watch.start` and
react to `watch.affected` + the build events each cycle; `watch.subscribe`
gives notify-only signals (no build) for dependency-aware tooling. See the
[command channel](/reference/events/#command-channel) for the full set.

## A web or desktop app

The pattern scales straight to a GUI:

```
browser ──ws──▶ your backend ──stdin/stdout (NDJSON)──▶ giant session
        ◀──ws──            ◀──────── events ──────────
```

Your backend owns one (or a pool of) `giant session` processes. It
translates UI actions into commands (`build`, `cancel`, `query.status`),
tags each with a `command_id`, and fans the event stream back to connected
clients over a websocket or IPC channel. The browser or desktop frontend
just renders events. Giant never needs to know a UI exists - it reads
commands and writes events.

Notes for a robust client:

- **Correlate by `command_id`.** Multiple commands can be in flight;
  match replies by the id you set.
- **Tolerate unknown event types.** Skip `t` values you don't handle; new
  ones are added without bumping the protocol.
- **Closing stdin is a clean shutdown.** The session drains in-flight work
  and exits. `{"c": "shutdown"}` does the same explicitly.
- **One stream, one writer.** Read stdout line by line; write whole JSON
  objects plus `\n` to stdin.

## Reference

- [Event protocol (NDJSON)](/reference/events/) - the full event and
  command catalogue.
- [`giant session`](/reference/cli/#giant-session) - the command itself.
- [Porcelains](/extending/porcelains/) - package a client as `giant-<name>`
  so it dispatches like a built-in.
