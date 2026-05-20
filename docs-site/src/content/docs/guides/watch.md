---
title: Watch mode
description: Continuous rebuilds with a debouncer that respects your selection.
---

`giant watch` runs an initial build, then keeps rebuilding affected
targets as files change. It's the everyday development loop.

## Basic usage

```bash
giant watch
```

This:

1. Runs an initial build of all non-test targets.
2. Starts watching the workspace for file changes.
3. On each batch of changes, computes which targets are affected and
   rebuilds them.
4. Loops until Ctrl-C.

## Selecting what to watch

The same selection language `giant build` uses applies:

```bash
giant watch go:bin:server         # one binary
giant watch 'go:**' '!go:test:*'  # everything Go, no tests
giant watch --tag dev             # dev-tagged targets
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
giant watch --quiet-ms 200 --max-delay-ms 1000
```

## What's excluded from the watch

By default Giant ignores:

- `.git/`
- `.giant/` (its own runtime state)
- The cache directory (whatever `cache.dir` resolves to)
- Every target's declared `outputs:` - Giant won't trigger rebuilds on
  files it produces itself

You don't need to write a `.giantignore`. Things you don't declare as
inputs don't trigger rebuilds anyway (a file change without an input
match means no target is affected → cycle skipped).

## Test loop

Pair with `giant test`:

```bash
# Terminal 1
giant watch

# Terminal 2
giant test --tag fast --watch    # if a --watch flag were added
```

For now, `giant test` doesn't have its own watch mode - but you can
run `giant watch` with a pattern matching just your tests:

```bash
giant watch 'go:test:*'
```

…or use `watchexec` to re-run `giant test` on a saved file. Both
work.

## Discovery re-runs

Watch re-prepares the graph on every cycle. If a structural input
(e.g. `import` lines in a Go file) changes, discovery re-runs and the
graph picks up newly emitted targets. New tests added during a watch
session are seen on the next cycle.

## Performance

The watcher uses `notify` (which uses inotify/FSEvents/ReadDirectoryChangesW
under the hood - kernel-level file notifications). Idle CPU usage is
near zero. The cycle work (re-prepare, affected computation, build)
dominates only when files actually change.

For a 10k-file workspace, the no-change side of a cycle is sub-50ms.

## Exit cleanly

Ctrl-C sends SIGINT. Giant cancels the in-flight build (if any), drains
the event queue, prints a "cancelled" note, and exits 0.
