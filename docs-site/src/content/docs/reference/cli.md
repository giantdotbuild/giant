---
title: CLI reference
description: Every subcommand and flag.
---

```
giant [GLOBAL FLAGS] <subcommand> [ARGS]
```

## Global flags

`--fresh` and `--log` are the true global flags - they work before or
after the subcommand. `--config` is not global: it must appear *before*
the subcommand (e.g. `giant --config path/giant.yaml build`).

| Flag | Description |
|---|---|
| `--config <path>` | Path to `giant.yaml` / `giant.json`. Defaults to walking up from cwd. Must precede the subcommand. |
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
| `--with-tests` | off | Include `test: true` targets in the selection. |
| `--watch` | off | After the initial build, rebuild the affected subset when files change. Ctrl-C to exit. |
| `--quiet-ms <n>` | 100 | (with `--watch`) Flush a change batch this long after the last event. |
| `--max-delay-ms <n>` | 500 | (with `--watch`) Flush a batch this long after the first event. |

Exit code: `0` on success, non-zero if any target failed. (No banner
on failure - the summary block already names what failed.)

`--watch` composes with the selection and every flag above:
`giant build go:bin:* --watch`, `giant build --with-tests --watch`
(watch everything). It prepares the graph once and rebuilds only the
affected subset each cycle; a `giant.yaml` edit mid-watch isn't picked
up - restart to reload config.

## `giant test`

Run test targets. Same flags as `giant build`; the only difference is
that the selection is restricted to `test: true` targets.

```
giant test [PATTERNS...]
```

Passing a non-test exact id (e.g. `giant test go:bin:server`) errors -
catches the obvious typo.

## Watching

There is no `watch` subcommand. Watch is the `--watch` flag on `build`
and `test`, so it composes with their selection and flags:

```
giant build go:bin:* --watch      # rebuild these on change
giant test go:* --watch           # the TDD loop
giant build --with-tests --watch  # watch everything
```

See [`giant build`](#giant-build) above for the watch flags
(`--watch`, `--quiet-ms`, `--max-delay-ms`).

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
`giant-<name>` binary on PATH, then the configurable dispatch routing
table. The table's default route sends anything else to `giant-task`, so
a bare task name just works - and everything after the name is passed
through untouched (no `--` needed).

```
giant tui                # → giant-tui binary on PATH
giant task deploy        # → giant-task with args [deploy]
giant deploy prod        # → routes to giant-task: run the `deploy` task with arg `prod`
giant deploy --help      # → giant-task prints the deploy task's signature
```

The catch-all is configurable per workspace - route namespaces to your
own porcelains via the `dispatch:` section (see
[config reference](/reference/config/#dispatch)). Core itself never
parses tasks; it only routes. See [Porcelains](/extending/porcelains/)
for how to build one.
