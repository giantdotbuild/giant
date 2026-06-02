---
title: Watch mode
description: Continuous rebuilds with a debouncer that respects your selection.
---

Watch is the `--watch` flag on `build` and `test`; there is no separate
subcommand. It runs an initial build, then keeps rebuilding the affected
targets as files change. It's the everyday development loop.

## Basic usage

```bash
giant build --watch
```

This:

1. Runs an initial build of all non-test targets.
2. Starts watching the workspace for file changes.
3. On each batch of changes, computes which targets are affected and
   rebuilds them.
4. Loops until Ctrl-C.

Because `--watch` is a flag, it composes with the full `build`/`test`
selection and every other flag.

## Selecting what to watch

The same selection language `giant build` uses applies:

```bash
giant build //cmd/server:server --watch  # one binary
giant build //src/... --tag lang=go --watch  # everything Go under src/
giant build --tag dev --watch            # dev-tagged targets
```

Watch enforces the selection on every cycle. Editing a file that only
affects targets *outside* the selection produces:

```
· no targets affected
```

- no rebuild, even though a file changed.

## The debouncer

Editor saves usually arrive in bursts (your editor writes the file,
its `.swp`, the lockfile, etc.). The watcher coalesces these into
batches:

- **Quiet window** (default 100 ms): a batch flushes after this long
  with no new events.
- **Max delay** (default 500 ms): a batch flushes after this long even
  if events keep streaming.

Both are tunable:

```bash
giant build --watch --quiet-ms 200 --max-delay-ms 1000
```

## What's excluded from the watch

By default Giant ignores:

- `.git/`
- The state directory (`state.dir`, default `.giant/` - its own runtime
  state)
- The cache directory (whatever `cache.dir` resolves to)
- Every target's declared `outputs:` - Giant won't trigger rebuilds on
  files it produces itself

You don't need to write a `.giantignore`. Things you don't declare as
inputs don't trigger rebuilds anyway (a file change without an input
match means no target is affected → the cycle is a no-op).

## Test-driven feedback loop

```bash
giant test --watch
```

This is the TDD shape: edit a source file, watch re-runs the affected
test targets, see results, repeat. It's `build --watch` restricted to
`test: true` targets - same selection semantics as `giant test`, same
debouncer, same affected-detection.

Mix with patterns and tags to scope further:

```bash
giant test //internal/auth:test --watch  # only auth tests
giant test --tag fast --watch            # only fast-tagged tests
giant test //internal/... --no-tag integration --watch
```

If you want everything - tests AND production targets - together:

```bash
giant build --with-tests --watch
```

## The graph is fixed for the watch

Watch prepares the graph from config once, then rebuilds the affected
subset on the same graph each cycle. Editing a `giant.yaml` (any
package's) mid-watch is **not** picked up - restart the watch to reload
it. (The long-lived
engine session reloads config on its own; the one-shot `--watch` keeps
it simple.)

## Performance

The watcher uses `notify` (inotify / FSEvents / ReadDirectoryChangesW -
kernel-level file notifications). Idle CPU usage is near zero. The cycle
work (affected computation, build) dominates only when files actually
change.

## Exit cleanly

Ctrl-C sends SIGINT. Giant cancels the in-flight build (if any), drains
the event queue, prints a "cancelled" note, and exits 0.

## Related: `giant-task --watch`

`build --watch` rebuilds *targets*. To re-run a *task* (lint, fmt,
deploy, an ad-hoc script) on file changes, use the task-runner's own
`--watch`:

```bash
giant-task --watch test:unit
```

Declare `inputs:` on the task to narrow what triggers a re-run (its
`deps:` are followed automatically). See
[`giant-task`](/extending/giant-task/#watch-mode).
