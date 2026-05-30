---
title: CLI reference
description: Every subcommand and flag.
---

```
giant [GLOBAL FLAGS] <subcommand> [ARGS]
```

## Global flags

| Flag | Description |
|---|---|
| `--config <path>` | Path to `giant.yaml` / `giant.json`. Defaults to walking up from cwd. |
| `--fresh` | Force a fresh build - bypass cache. |
| `--log <filter>` | Log filter (RUST_LOG syntax). Default: `error`. |

## `giant build`

Build targets.

```
giant build [PATTERNS...]
```

| Flag | Default | Description |
|---|---|---|
| `-j, --jobs <n>` | num CPUs | Number of parallel jobs. |
| `--events <fmt>` | - | Emit structured events. `ndjson` is the only format in v1. |
| `--affected` | off | Restrict to targets affected by changes. Requires `--base` or `--file`. |
| `--base <ref>` | - | Git ref baseline for `--affected`. |
| `--file <path>` | - | Explicit changed-file list. Repeatable. Overrides `--base`. |
| `-q, --quiet` | off | Print only failures + summary. |
| `--color <when>` | `auto` | `auto`, `always`, `never`. Honors `NO_COLOR`. |
| `--tag <tag>` | - | Include only targets carrying this tag. Repeatable (union). |
| `--no-tag <tag>` | - | Exclude targets carrying this tag. Repeatable. |
| `--show-toolchains` | off | Show `toolchain`-tagged targets, folded out by default. |

Exit code: `0` on success, non-zero if any target failed. (No banner
on failure - the summary block already names what failed.)

## `giant test`

Run test targets. Same flags as `giant build`; the only difference is
that the selection is restricted to `test: true` targets.

```
giant test [PATTERNS...]
```

Passing a non-test exact id (e.g. `giant test go:bin:server`) errors -
catches the obvious typo.

## `giant watch`

Run an initial build, then continuously rebuild affected targets when
files change. Ctrl-C to exit.

```
giant watch [PATTERNS...]
```

| Flag | Default | Description |
|---|---|---|
| `-j, --jobs <n>` | num CPUs | Parallel jobs per rebuild. |
| `--quiet-ms <n>` | 100 | Flush a batch this long after the last event. |
| `--max-delay-ms <n>` | 500 | Flush a batch this long after the first event. |
| `-q, --quiet` | off | Print only failures + summary per cycle. |
| `--color <when>` | `auto` | See `build`. |
| `--tag <tag>` | - | See `build`. |
| `--no-tag <tag>` | - | See `build`. |
| `--test` | off | Watch test targets only (TDD loop). Mutually exclusive with `--all`. |
| `--all` | off | Watch every target - tests and non-tests. |
| `--show-toolchains` | off | Show `toolchain`-tagged targets, folded out by default. |

## `giant affected`

List targets that would rebuild given a set of changed files. Doesn't
run anything. Output: one ID per line, sorted, on stdout.

```
giant affected [--base <ref> | --file <path>...] [PATTERNS...]
```

| Flag | Description |
|---|---|
| `--base <ref>` | Git ref baseline. |
| `--file <path>` | Explicit changed-file list. Repeatable. Overrides `--base`. |
| `--tag <tag>` | Filter by tag (include). |
| `--no-tag <tag>` | Filter by tag (exclude). |
| `--tests-only` | Restrict to test targets. |
| `--with-tests` | Include test targets alongside non-test. |

Empty output is exit 0 - most CI scripts want this.

## `giant graph`

Show the dependency graph.

```
giant graph [TARGET]
```

With no argument, lists every target. With a target, shows its
transitive dep tree.

## `giant explain`

Show what feeds a target's cache key. The first thing to reach for
when "why did this rebuild?" comes up.

```
giant explain <TARGET> [--diff <OTHER_TARGET>]
```

Output covers: the cache key itself, the command, cwd, env vars, file
inputs (with their content hashes), structural inputs (with their
fingerprints), and dep output hashes.

`--diff <other-target>` swaps the breakdown for a side-by-side
comparison: only the fields that differ between the two targets are
printed. Useful for "why does `bin:server` have a different key than
`bin:server-debug`?" and similar.

## `giant logs`

Replay a target's captured stdout/stderr from the cache.

```
giant logs <TARGET> [--key <hex>] [--stdout-only | --stderr-only | --merged]
```

| Flag | Description |
|---|---|
| `--key <hex>` | Look up by an explicit cache key. Defaults to the current key (what a fresh build would compute). |
| `--stdout-only` | Print stdout only. |
| `--stderr-only` | Print stderr only. |
| `--merged` | Interleave stdout and stderr (default behavior). |

Errors out if the action-cache entry has no captured logs (a target
that ran with log capture disabled, or a cold target that's never
been built).

## `giant clean`

Clear the local cache. By default interactive: shows a summary, asks
for confirmation, then wipes the cache directory.

```
giant clean [-y] [--dry-run] [--older-than <duration>] [PATTERNS...]
```

| Flag | Description |
|---|---|
| `-y, --yes` | Skip the confirmation prompt. |
| `--dry-run` | Print what would be deleted; touch nothing. |
| `--older-than <duration>` | Only clean entries older than this (`30d`, `12h`, `15m`, `45s`). |
| `[PATTERNS...]` | Target-id patterns. Same selection language as `giant build` - exact ids, globs (`go:*`, `**:test:*`), exclusions (`!go:test:*`). |

With no patterns or `--older-than`, the entire cache is cleared (the
historical behavior). Combine both for surgical eviction:

```bash
giant clean 'go:*' --older-than 14d -y
```

For automatic LRU eviction (which happens after every build when
configured), see the `cache.max_size_gb` setting in
[giant.yaml reference](/reference/config/).

## `giant session`

Persistent engine over stdio. Loads config once, runs discovery once,
then reads NDJSON commands on stdin and emits NDJSON events on stdout.
The protocol porcelains (the TUI in particular) drive against. Refuses
to run with stdout on a TTY - pipe it.

```
giant session --events ndjson <commands.jsonl >events.jsonl
```

| Flag | Default | Description |
|---|---|---|
| `--events <fmt>` | `ndjson` | Only `ndjson` today; flag shape matches `giant build`. |

Commands accepted on stdin: `build`, `cancel`, `watch.start`,
`watch.stop`, `affected.subscribe`, `affected.unsubscribe`,
`shutdown`. See [Event protocol - Command channel](/reference/events/#command-channel)
for the full wire format and ack semantics.

## Porcelain dispatch

Unknown subcommands fall through to `giant-<name>` on PATH. If the
binary exists, it's exec'd; otherwise you get a helpful error.

```
giant task deploy        # → exec's giant-task with args [deploy]
giant tui                # → exec's giant-tui
giant nope               # error: no such subcommand, no giant-nope on PATH
```

See [Porcelains](/extending/porcelains/) for how to build one.
