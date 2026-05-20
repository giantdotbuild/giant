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
  - id: "go:bin:server"
    inputs: ["cmd/server/**/*.go"]
    outputs: ["bin/server"]
    command: "go build -o bin/server ./cmd/server"

tasks:
  serve:
    command: "./bin/server"
    description: "Run the server locally"
    deps: ["go:bin:server"]
```

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
    command: "..."              # required; shell command
    description: "..."          # optional; shown in `giant task list`
    deps: ["..."]               # target IDs to build before running
    needs: ["..."]              # other task names to run before command
    services: ["..."]           # service names to start before, stop after
    finally: ["..."]            # task names to run after command (always)
    args:                       # optional named arguments
      <key>:
        default: "..."          # value when --arg isn't passed
        choices: ["a", "b"]     # constrained set; default must be in choices
        description: "..."      # shown in completion + help
    env:                        # extra env vars
      KEY: "value"
    cwd: "..."                  # workspace-relative; default = root
    timeout_secs: 300           # kill after N seconds; default = no timeout
```

Task names follow the same rules as workspace names (alphanumeric,
hyphen, underscore; no leading digit). Names that would shadow a
built-in giant subcommand (`build`, `test`, `watch`, `affected`,
`graph`, `clean`, `explain`, `help`) are rejected at config load.

## The service schema

```yaml
services:
  <name>:
    command: "..."              # required; shell command (the daemon)
    description: "..."          # optional
    deps: ["..."]               # target IDs to build before starting
    ready:                      # optional readiness probe
      command: "..."            # shell snippet; exit 0 = ready
      period_secs: 1            # poll interval (default 1)
      timeout_secs: 30          # give up after this (default 30)
    env:                        # extra env vars
      KEY: "value"
    cwd: "..."                  # workspace-relative; default = root
```

A service is a long-lived process started for the duration of a task
that lists it under `services:`. Cleanup is automatic: when the task
exits (any reason - success, failure, signal), services are sent
SIGINT, then SIGTERM if they don't exit in 2s, then SIGKILL after
another 3s.

## Lifecycle of one task

```
1. build deps          → giant build <ids>
2. start services      → spawn each (parallel), wait for each `ready`
3. run needs           → sequential, declared order
4. run command         → the task's own command
5. run finally         → sequential, declared order; ALWAYS runs
6. stop services       → parallel; SIGINT → SIGTERM → SIGKILL
```

If a step fails:

| Failure | Effect |
|---|---|
| `deps` build | stop. Nothing else runs. |
| `services` not ready in `timeout_secs` | stop already-started services, skip needs/command/finally. |
| `needs` task | skip `command`, still run `finally`, still stop services. |
| `command` non-zero exit | still run `finally`, still stop services. The exit code is what the task returns. |
| `finally` task | logged, doesn't change the task's exit code. |

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
    deps: ["go:bin:server"]            # build first
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

Named args are bound at the command line via `--arg key=value`
(repeatable). Each declared arg is exported as a `GIANT_ARG_<NAME>`
environment variable before `sh -c` runs:

```yaml
tasks:
  deploy:
    command: "kubectl apply -f k8s/$GIANT_ARG_ENV/"
    args:
      env:
        default: "staging"
        choices: ["staging", "prod"]
        description: "Target environment"
```

```console
$ giant task deploy
▶ deploy
deployed to staging

$ giant task deploy --arg env=prod
▶ deploy
deployed to prod

$ giant task deploy --arg env=anywhere
giant-task: argument 'env': value "anywhere" is not one of ["staging", "prod"]
```

If a declared arg has no `default` and the user doesn't supply one,
`giant-task` errors before doing any work.

## Pass-through args

Everything after `--` is appended to the task command's positional
arguments inside `sh -c`:

```yaml
tasks:
  test:
    command: 'cargo test "$@"'
```

```console
$ giant task test -- --release --nocapture
```

`"$@"` inside the shell command expands to `--release --nocapture`.
Useful for forwarding raw flags to whatever the task wraps.

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

✗ fail:bad
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
IDs from `giant.yaml`'s `targets:`/`include:` plus anything
discovery wrote to disk on a previous build.

## How it composes with the engine

The `giant-task` binary doesn't reach into Giant's internals. The
contract:

- **Build deps:** spawned as `giant build <ids…> --events ndjson`,
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
| `GIANT_ARG_<NAME>` | Set per declared task arg before `sh -c`. |
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
