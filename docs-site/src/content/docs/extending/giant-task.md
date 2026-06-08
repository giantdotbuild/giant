---
title: giant-task - the task runner
description: Run named commands with build deps, args, and pass-through.
---

`giant-task` is the official task-runner porcelain - a separate
binary that adds named commands ("tasks") on top of Giant's build
engine. Dispatched automatically via `giant task <name>` (Giant
itself doesn't ship a `task` subcommand; the dispatcher execs
`giant-task` on PATH per [ADR-0010](https://github.com/johnae/giant/blob/main/docs/adr/0010-tasks-live-in-porcelain.md)).

```console
$ giant task deploy
· building 2 dep(s)
  ✓ 0 built · 2 cached  in 1ms
▶ deploy to staging
deployed
```

The engine doesn't know what a task is - `giant-task` re-reads
`giant.yaml` with its own schema and ignores everything except the
`tasks:` block. If you don't install `giant-task`, the `tasks:`
field in your config is simply ignored. Other porcelains can claim
their own top-level keys the same way.

## Install

```bash
# From crates.io (once published)
cargo install giant-task

# From source
cargo install --path crates/giant-task --git https://github.com/johnae/giant
```

The binary just needs to be on PATH. Verify:

```console
$ giant-task --version
giant-task 0.1.0

$ giant task --version   # via the dispatcher
giant-task 0.1.0
```

## A first task

Add a `tasks:` block to your `giant.yaml`:

```yaml
workspace:
  name: my-monorepo

targets:
  - name: "server"
    inputs: ["cmd/server/**/*.go"]
    outputs: ["//bin/server"]
    cwd: "//"
    command: "go build -o bin/server ./cmd/server"

tasks:
  serve:
    command: "./bin/server"
    description: "Run the server locally"
    deps: ["//:server"]
```

`deps:` reference targets by their **label** - here `//:server`, the
`server` target in the root package. In a split repo you'd write the
full path, e.g. `deps: ["//crates/giant:giant"]`.

Then:

```console
$ giant task serve
· building 1 dep(s)
  ✓ 1 built · 0 cached  in 240ms
▶ Run the server locally
listening on :8080
```

`giant-task` first asks `giant build` to materialize the deps, then
runs the task's `command` via `sh -c` in the workspace root.

## The task schema

```yaml
tasks:
  <name>:
    command: "..."              # shell command or #! script body; optional
                                #   if `services:` is set (foreground supervise)
    description: "..."          # optional; shown in `giant task list`
    deps: ["..."]               # target labels (//pkg:name) to build before running
    needs: ["..."]              # other task names to run before command
    services: ["..."]           # service names to start before, stop after
    finally: ["..."]            # task names to run after command (always)
    args:                       # optional; ordered list, bound positionally
      - name: env
        default: "..."          # present => optional; absent => required
        choices: ["a", "b"]     # constrained set; default must be in choices
        description: "..."      # shown in completion + help
      - name: rest
        variadic: true          # trailing only; collects the rest into $@
    env:                        # extra env vars
      KEY: "value"
    cwd: "..."                  # workspace-relative; default = root
    timeout_secs: 300           # kill after N seconds; default = no timeout
    inputs: ["..."]             # optional; extra watch globs for files no
                                # target owns (consulted by --watch)
```

## Watch mode

`giant task <name> --watch` runs the task once, then re-runs it
whenever a relevant input changes. Ctrl-C exits.

This is dep-aware, and giant-task does no file watching itself. It
opens a `giant session` and subscribes to the task's dependencies with
`watch.subscribe { targets: deps, globs: inputs }`. The engine watches
the inputs of those `deps:` targets - and their transitive deps - plus
any path matching the task's `inputs:` globs, and notifies giant-task
(a `watch.changed` event) when one changes. So editing a file that a
dependency target consumes retriggers the task, even though the task
never named that file.

```console
$ giant task test:unit --watch
· initial run
…
· watching via engine - Ctrl-C to exit
· change detected, re-running
…
```

| Flag | Default | Description |
|---|---|---|
| `--watch` | off | Re-run on changes, watched by the engine. |

What gets watched:

- **`deps:`** - the engine expands these through the graph and watches
  every input they (transitively) depend on.
- **`inputs:`** - extra globs for files no target owns (e2e sources,
  fixtures).
- **Neither declared** - falls back to the whole workspace (minus
  `.git/`, the state dir, and the cache dir). Handy as a smoke loop but
  noisy; declare `deps:` / `inputs:` where you can.

Because the watching lives in core, a task and a `giant build --watch`
see the same change signal - one file-watching implementation, not two.

Task names follow the same rules as workspace names (alphanumeric,
hyphen, underscore; no leading digit). Any valid name is allowed - a task
named `build` or `test` is fine. Tasks are always invoked as `giant task
<name>`, so they never collide with a `giant` command like `giant build`
(which runs the build porcelain); the two namespaces are separate.

## The service schema

```yaml
services:
  <name>:
    command: "..."              # required; shell command (the daemon)
    description: "..."          # optional
    deps: ["..."]               # target labels (//pkg:name) to build before starting
    needs: ["..."]              # other services to bring up (ready) first
    ready:                      # optional readiness probe
      command: "..."            # shell snippet; exit 0 = ready
      period_secs: 1            # poll interval (default 1)
      timeout_secs: 30          # give up after this (default 30)
    env:                        # extra env vars
      KEY: "value"
    cwd: "..."                  # workspace-relative; default = root
```

A service is a long-lived process. When a task brings up services, the
supervisor starts them in **dependency order**: a service with `needs:`
waits for each dependency's `ready` probe to pass before it starts
(services with satisfied needs start concurrently). The transitive
`needs` closure is pulled in automatically, so listing `api` brings up
the `db` it needs. Cleanup is automatic: when the task exits (any reason
- success, failure, signal), services are sent SIGINT, then SIGTERM if
they don't exit in 2s, then SIGKILL after another 3s.

## Dev environments: a task that *is* its services

A task with `services:` and **no `command:`** supervises those services
in the foreground - the `giant dev` shape. It brings the stack up
dependency-ordered, streams their prefixed logs, and holds until Ctrl-C
(or until a service exits), then shuts everything down.

```yaml
services:
  db:
    command: "postgres -D ./data"
    ready: { command: "pg_isready" }
  api:
    command: "./bin/api"
    needs: ["db"]               # api starts once db is ready
  worker:
    command: "./bin/worker"
    needs: ["db"]

tasks:
  dev:
    services: ["api", "worker"]  # no command → foreground supervise
```

```console
$ giant dev
· starting services: db, api, worker
[db]     listening on 5432
[api]    serving on :8080
[worker] ready
^C
· interrupted
· stopping services: db, api, worker
```

This is the dev-loop slice of process-compose. It deliberately stops
there: no daemon/background mode, no process scaling, no restart
policies, no REST control. For those, use
[process-compose](https://github.com/F1bonacc1/process-compose).

## Lifecycle of one task

```
1. build deps          → giant build <labels>
2. start services      → dependency-ordered, each gated on its `ready` probe
3. run needs           → sequential, declared order
4. run command         → the task's own command (or supervise, if absent)
5. run finally         → sequential, declared order; ALWAYS runs
6. stop services       → SIGINT → SIGTERM → SIGKILL
```

If a step fails:

| Failure | Effect |
|---|---|
| `deps` build | stop. Nothing else runs. |
| `services` not ready in `timeout_secs` | stop already-started services, skip needs/command/finally. |
| `needs` task | skip `command`, still run `finally`, still stop services. |
| `command` non-zero exit | still run `finally`, still stop services. The exit code is what the task returns. |
| `finally` task | logged, doesn't change the task's exit code. |

### Signals

`finally` and service teardown run on **SIGINT or SIGTERM**, not just a
terminal Ctrl-C. When a task has services or a `finally`, giant-task
installs a handler: a signal - whether from Ctrl-C, `pkill`, `systemctl
stop`, or a parent supervisor - interrupts the running command (forwarded
to its process group), then the lifecycle falls through to `finally` and
stops services as usual. The task exits `130` (SIGINT) or `143` (SIGTERM).
Once teardown starts it runs to completion; a second signal won't cut it
short. A bare command with nothing to clean up keeps the default
behavior - the signal just kills it.

To make whole-subtree teardown work, a command with services or a
`finally` runs in its own process group. Interactive commands still work:
giant-task hands the terminal to that group while the command runs - the
way a shell does for a foreground job - so a `sudo` or `ssh` password
prompt reads the terminal normally, then giant-task takes it back. Off a
real terminal (CI, pipes) this is a no-op.

A worked example with all four hooks:

```yaml
services:
  db:
    command: "docker run --rm -p 5432:5432 postgres:16"
    ready:
      command: "pg_isready -h localhost -p 5432"

tasks:
  run-test:
    command: "go test ./..."
    deps: ["//:server"]                # build first
    services: ["db"]                   # spin up DB
    needs: ["migrate"]                 # run schema migrations first
    finally: ["wipe-test-data"]        # always clean up test rows

  migrate:
    command: "./bin/migrator up"
    services: ["db"]                   # migrate also needs the DB up

  wipe-test-data:
    command: "psql -c 'TRUNCATE …'"
    services: ["db"]
```

```console
$ giant task run-test
· starting services: db
· need: migrate
▶ migrate
[migrate] applied 4 migrations
▶ go test ./...
[run-test] ok      example/internal/store  0.123s
· finally: wipe-test-data
▶ wipe-test-data
· stopping services: db
```

## Arguments

`args:` is an ordered list. Values are bound **positionally** in
declaration order - `giant deploy prod` binds `prod` to the first arg.
An arg with no `default` is **required**; one with a `default` is
optional. A trailing arg may be `variadic: true` to collect the rest.

Each scalar arg is exported two ways before the command runs:
`GIANT_ARG_<NAME>` (uppercased, unambiguous) and a plain `$name`. A
variadic arg becomes the command's positional parameters (`$@`).

```yaml
tasks:
  deploy:
    command: "kubectl apply -f k8s/$env/ $@"
    args:
      - name: env
        choices: ["staging", "prod"]   # required (no default)
        description: "Target environment"
      - name: flags
        variadic: true                 # the rest → $@
```

```console
$ giant deploy prod --server-side
▶ deploy
deployed to prod with: --server-side

$ giant deploy            # missing required arg
giant-task: argument 'env': required (no value supplied and no default)

$ giant deploy nowhere
giant-task: argument 'env': value "nowhere" is not one of ["staging", "prod"]
```

You can also set an arg by name with `--arg name=value` (the scriptable
form); it conflicts with a positional for the same arg.

**Everything after the task name belongs to the task** - including
flag-like values, which bind to the variadic arg and reach the command
as `$@`, no `--` needed:

```console
$ giant test --release --nocapture     # forwarded to the test task's $@
```

Giant-task's own flags (`--watch`, `--config`, …) therefore come
*before* the task name (`giant task --watch deploy`), the same rule git
and cargo use.

## Per-task help

`giant <task> --help` prints that task's signature - its arguments,
which are required, their defaults and choices:

```console
$ giant deploy --help
deploy - deploy the app
  usage: giant deploy <env> [tag=latest]

    env  staging|prod  target environment
    tag  =latest
```

(`giant task --help`, with no task name, prints giant-task's own help.)

## Tasks in any language

If a task's `command` begins with a `#!` shebang line, the whole body is
written to a temp file and exec'd directly, so you can write a task in
any language. Declared args are in the environment; variadic/passthrough
values are the script's arguments.

```yaml
tasks:
  report:
    args: [{ name: since, default: "HEAD~20" }]
    command: |
      #!/usr/bin/env python3
      import os, subprocess
      since = os.environ["GIANT_ARG_SINCE"]
      print(subprocess.check_output(["git", "log", "--oneline", since]).decode())
```

A body without a shebang runs under `sh -c` as usual.

## Listing tasks

```console
$ giant task list             # or `giant-task --list`
tasks (my-monorepo)
  serve    Run the server locally
  deploy   Deploy to an environment
  test     Run the test suite
```

If you have no tasks declared, this prints a single dim "no tasks
defined" note instead.

## Dep-phase output

By default the dep build is collapsed into a one-line summary. A
50-target dependency pull doesn't fill the terminal before the
actual task command runs:

```console
$ giant task serve
· building 50 dep(s)
  ✓ 3 built · 47 cached  in 1.24s
▶ Run the server locally
listening on :8080
```

On failure, the failing target's stderr is replayed inline (capped
at 50 lines per target):

```console
$ giant task broken
· building 1 dep(s)

✗ //:bad
  going to fail
  this line too
  ✗ 1 failed · 0 built · 0 cached  in 2ms
giant-task: dependency build failed (exit code 1)
```

`--verbose` (`-v`) restores the full streamed `giant build`
output - useful when you want the per-target lines.

## Shell completions

`giant-task` has its own completion script generator, separate from
`giant`'s. Both binaries support bash, zsh, fish, PowerShell,
elvish, and nushell. Pipe the script into your shell's completion
directory:

```bash
# bash
giant-task --completions bash > ~/.local/share/bash-completion/completions/giant-task

# zsh
giant-task --completions zsh > "${fpath[1]}/_giant-task"

# fish
giant-task --completions fish > ~/.config/fish/completions/giant-task.fish

# nushell
giant-task --completions nushell >> ~/.config/nushell/completions.nu
```

Dynamic completion of task names works at TAB time - `giant-task`
reads the nearest `giant.yaml` and returns the matching tasks,
including their descriptions. Same idea for `giant` itself: target
labels from the merged `giant.yaml` package files.

## How it composes with the engine

The `giant-task` binary doesn't reach into Giant's internals. The
contract:

- **Build deps:** spawned as `giant build <labels…> --events ndjson`,
  the output parsed event-by-event so the porcelain can render a
  compact summary instead of streaming everything.
- **Config schema:** parsed by `giant-task` directly with its own
  narrow `TopLevel { workspace, tasks }` shape. Core's wider config
  parsing isn't involved.
- **Workspace root:** `giant-task` walks up from cwd looking for
  `giant.yaml` / `giant.json` (~30 LOC, deliberately not reaching
  into Giant's private modules).

If you want to write your own porcelain (`giant-deploy`,
`giant-bench`, anything), see [Porcelains](/extending/porcelains/)
for the dispatch mechanism and the [NDJSON event
protocol](/reference/events/) for the wire format.

## Environment variables

| Variable | Meaning |
|---|---|
| `GIANT_ARG_<NAME>` | Set per declared scalar arg (uppercased name). |
| `$<name>` | Plain-name binding for the same arg (lowercase as declared). |
| `GIANT_TASK_BUILD_BIN` | Override the `giant` binary used for `giant build` subprocess calls. Useful in tests; rarely needed otherwise. |
| `NO_COLOR` | Disable ANSI colors in `giant-task`'s own output (`giant build`'s output is also subject to this). |

## What giant-task DOESN'T do

A short list, deliberately:

- **No service restart policies.** If a service dies during your task,
  the task fails. (The right default for tests; if you want auto-restart
  during a dev loop, run `process-compose` directly.)
- **No service-to-service dependency ordering.** Express order by
  wrapping in a task that declares both services and the order it
  needs. Or use process-compose for nested service graphs.
- **No `after` DAG / log ring buffers / parallel `needs`.**
  `needs:` is sequential, `finally:` is sequential, and that's it.
- **No watch-mode service auto-restart on file changes.** Use
  `watchexec` or `process-compose`.
- **No nested tasks (`giant db migrate`).** Use flat names
  (`giant task db-migrate`) or namespace via prefix.
- **No `outputs:` / caching on tasks.** Tasks run every time. If you
  want caching, declare a build target with `outputs:` and an
  `exists:` check; `giant build` handles it cleanly.
- **No HTTP/TCP probe shortcuts.** `ready.command:` covers everything
  (`curl -fs http://...`, `nc -z host port`). HTTP/TCP sugar may
  arrive if heavy users ask.

Service supervision uses the [tokio-process-tools](https://docs.rs/tokio-process-tools)
crate for the cross-platform tricky parts (signal escalation, broadcast
output streams). The policy on top - schema, lifecycle, readiness
loop - is giant-task's own.
