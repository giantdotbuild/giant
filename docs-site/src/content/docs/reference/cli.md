---
title: CLI reference
description: Every subcommand and flag.
---

```
giant <subcommand> [ARGS]
```

## Core flags

The `giant` binary is a thin dispatcher. Its built-ins (`session`,
`completions`) take two flags of its own, *before* the subcommand:

| Flag | Description |
|---|---|
| `--config <path>` | Path to `giant.yaml` / `giant.json` for the core. Defaults to walking up from cwd. Must precede the subcommand. |
| `--log <filter>` | Log filter (RUST_LOG syntax). Default: `error`. |

Everything else is a [porcelain](/extending/porcelains/) with its own
flags, passed *after* the subcommand - including `--config`, `--fresh`,
and `--sandbox` on the build family. So it's `giant build --fresh`, not
`giant --fresh build`.

## `giant build`

Build targets.

```
giant build [PATTERNS...]
```

`PATTERNS` are label selectors (see the [selection
language](/concepts/selection/) for the full grammar):

| Pattern | Selects |
|---|---|
| `//src/go/server:server` | one exact target |
| `//src/go/server` | shorthand for the target whose name matches the last path segment |
| `//src/go:*` | every target in the `//src/go` package (one segment, no descent) |
| `//src/go/...` | every target at or under `//src/go`, crossing package boundaries |

`*` matches within a single path segment; `...` is the recursive wildcard
that crosses packages. A literal label that matches nothing is an error,
and Giant suggests the closest existing label:

```console
$ giant build //src/go/server:sever
no target matches "//src/go/server:sever" - did you mean "//src/go/server:server"?
```

| Flag | Default | Description |
|---|---|---|
| `--config <path>` | - | Path to `giant.yaml` / `giant.json`. Walks up from cwd by default. |
| `--fresh` | off | Bypass the cache - rebuild every selected target. |
| `--sandbox` | off | Run eligible targets through the `giant-sandbox` helper (Linux only; see [verify](#giant-verify)). |
| `-j, --jobs <n>` | num CPUs | Number of parallel jobs. |
| `--events <fmt>` | - | Emit structured events. `ndjson` is the only format in v1. |
| `--affected` | off | Restrict to targets affected by changes. Requires `--base` or `--file`. |
| `--base <ref>` | - | Git ref baseline for `--affected`. |
| `--file <path>` | - | Explicit changed-file list. Repeatable. Overrides `--base`. |
| `-q, --quiet` | off | Print only failures + summary. |
| `--color <when>` | `auto` | `auto`, `always`, `never`. Honors `NO_COLOR`. |
| `--tag <tag>` | - | Include only targets carrying this tag. Repeatable. A bare value (`--tag release`) matches the tag; `key=value` (`--tag kind=bin`) matches a role tag. Multiple `--tag` flags **union** - a target passes if it carries any of them. One whole tag per flag; no comma syntax. |
| `--no-tag <tag>` | - | Exclude targets carrying this tag. Repeatable. Same value syntax as `--tag`. |
| `--show-toolchains` | off | Show `toolchain`-tagged targets, folded out by default. |
| `failed-last` | off | Re-run only the targets that failed in the last build. Used as a positional selector: `giant build failed-last`. |
| `--with-tests` | off | Include `test: true` targets in the selection. |
| `--watch` | off | After the initial build, rebuild the affected subset when files change. Ctrl-C to exit. |
| `--quiet-ms <n>` | 100 | (with `--watch`) Flush a change batch this long after the last event. |
| `--max-delay-ms <n>` | 500 | (with `--watch`) Flush a batch this long after the first event. |

Exit code: `0` on success, non-zero if any target failed. (No banner
on failure - the summary block already names what failed.)

`--watch` composes with the selection and every flag above:
`giant build //src/go:* --watch`, `giant build --with-tests --watch`
(watch everything). It prepares the graph once and rebuilds only the
affected subset each cycle; a `giant.yaml` edit mid-watch isn't picked
up - restart to reload config.

## `giant test`

Run test targets. Same flags as `giant build`; the only difference is
that the selection is restricted to `test: true` targets.

```
giant test [PATTERNS...]
```

Passing a non-test exact label (e.g. `giant test //src/go/server:server`)
errors - catches the obvious typo.

## `giant verify`

The hermeticity audit. `verify` is `build` with the sandbox and a fresh
build forced on, over every target (tests included): each target runs
isolated, the cache is bypassed so everything actually runs, and a target
that reads an undeclared file, depends on a scrubbed env var, or reaches
the network fails. Same selection and output flags as `build`.

```
giant verify [PATTERNS...]
```

Linux only (it uses the `giant-sandbox` helper). Use it in CI to catch
under-declared inputs before they cause a phantom cache hit.

## Watching

There is no `watch` subcommand. Watch is the `--watch` flag on `build`
and `test`, so it composes with their selection and flags:

```
giant build //src/go:* --watch    # rebuild this package on change
giant test //src/go/... --watch   # the TDD loop
giant build --with-tests --watch  # watch everything
```

See [`giant build`](#giant-build) above for the watch flags
(`--watch`, `--quiet-ms`, `--max-delay-ms`).

## `giant affected`

List targets that would rebuild given a set of changed files. Doesn't
run anything. Output: one label per line, sorted, on stdout.

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

| Flag | Description |
|---|---|
| `-r, --reverse` | Show downstream consumers instead of upstream dependencies. |

## `giant explain`

Show what feeds a target's cache key. The first thing to reach for
when "why did this rebuild?" comes up.

```
giant explain <TARGET> [--diff <OTHER_TARGET>]
```

Output covers: the cache key itself, the command, cwd, env vars, file
inputs (with their content hashes), and dep output hashes.

`--diff <other-target>` swaps the breakdown for a side-by-side
comparison: only the fields that differ between the two targets are
printed. Useful for "why does `//src/go/server:server` have a different
key than `//src/go/server:server-debug`?" and similar.

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
| `--merged` | Route stderr into stdout too. By default the streams are split: stdout goes to stdout, stderr to stderr. |

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
| `[PATTERNS...]` | Label patterns. Same selection language as `giant build` - exact labels, package/recursive patterns (`//src/go:*`, `//src/go/...`), exclusions (`!//src/go/...`). |

With no patterns or `--older-than`, the entire cache is cleared (the
historical behavior). Combine both for surgical eviction:

```bash
giant clean '//src/go/...' --older-than 14d -y
```

For automatic LRU eviction (which happens after every build when
configured), see the `cache.max_size_gb` setting in
[giant.yaml reference](/reference/config/).

## `giant session`

Persistent engine over stdio. Loads config once, then reads NDJSON
commands on stdin and emits NDJSON events on stdout.
The protocol porcelains (the TUI in particular) drive against. Refuses
to run with stdout on a TTY - pipe it.

```
giant session --events ndjson <commands.jsonl >events.jsonl
```

| Flag | Default | Description |
|---|---|---|
| `--events <fmt>` | `ndjson` | Only `ndjson` today; flag shape matches `giant build`. |

Commands accepted on stdin: `build`, `cancel`, `watch.start`,
`watch.stop`, `watch.subscribe`, `watch.unsubscribe`,
`affected.subscribe`, `affected.unsubscribe`, `config.reload`,
`shutdown`. See [Event protocol - Command channel](/reference/events/#command-channel)
for the full wire format and ack semantics.

The session also reloads on its own when `giant.yaml` / `giant.json`
changes - it re-reads the config and re-emits the catalog
(`catalog.invalidating` → `catalog.ready`), so a `giant tui` reflects
config edits without a restart. `config.reload` forces the same thing.

## Porcelain dispatch

`giant <name>` resolves in order: a built-in subcommand, then a
`giant-<name>` binary (beside the giant binary, then on PATH). Everything
after the name is passed through untouched (no `--` needed). An unknown name
is an error (with a hint to try `giant task <name>`), so a typo fails loudly
instead of running as something unexpected.

```
giant tui                # → giant-tui binary
giant task deploy        # → giant-task with args [deploy] (run the `deploy` task)
giant deploy prod        # → error: no such subcommand 'deploy' (use `giant task deploy`)
```

Tasks live behind the explicit `giant task` front door (the `giant-task`
porcelain); core itself never parses tasks. See
[Porcelains](/extending/porcelains/) for how to build one.
