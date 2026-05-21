---
title: Selection language
description: Picking targets via globs, exclusions, tags, and modes.
---

Every subcommand that operates on targets (`build`, `test`, `watch`,
`affected`) takes the same selection arguments. One language, one
matcher, used everywhere.

## Empty: everything

```bash
giant build         # all non-test targets
giant test          # all test targets
giant watch         # all non-test targets, continuously
giant watch --test  # all test targets, continuously (TDD loop)
giant watch --all   # everything, test + non-test, continuously
```

## Exact IDs

```bash
giant build go:bin:server
giant build go:bin:server docker:api
```

A literal id that doesn't exist is an error. Catches typos:

```console
$ giant build go:bin:srvr
no target matches "go:bin:srvr"
```

## Globs

Patterns match against the full target id. The separator is `:`.

| Pattern | Matches |
|---|---|
| `*` | any chars except `:` |
| `**` | any chars including `:` |

```bash
giant build 'go:bin:*'      # go:bin:server, go:bin:client
giant build 'go:**'         # everything starting with go:
giant build '**:test:*'     # all test targets across all languages
```

Glob misses are silent - no targets, no error. Only literal-id misses
error out.

```console
$ giant build 'rust:**'
· no targets to build       # no rust targets exist; not a typo, just empty
```

## Exclusion

Prefix `!` to remove matches. Shell-special, so quote it:

```bash
giant build 'go:**' '!go:test:*'
giant build '!go:**'        # everything except go:
```

If only excludes are given, the implicit include is `**` - everything,
minus your excludes.

## Tags

Mark targets with `tags: [...]` in `giant.yaml`:

```yaml
- id: "go:bin:server"
  tags: ["release", "linux"]
  ...
```

Filter with `--tag` (include) and `--no-tag` (exclude):

```bash
giant build --tag release            # only release-tagged
giant build --tag release --no-tag flaky    # release AND NOT flaky
giant build --tag linux --tag macos  # linux OR macos (union)
```

Tags compose with patterns:

```bash
giant build 'go:**' --tag release    # release-tagged AND go:**
```

## Test mode

`giant build` excludes `test: true` targets by default. `giant test`
selects only test targets:

```bash
giant build           # 12 production targets
giant test            # 47 test targets
giant test 'go:**'    # only Go tests
giant test go:bin:server   # error - bin:server isn't a test
```

## Affected

Restrict the selection to what changed since a baseline:

```bash
giant build --affected --base main         # diff against main
giant build --affected --base HEAD         # since last commit (working tree)
giant build --affected --file src/main.go  # explicit file list (for CI)
```

`--affected` composes with everything else:

```bash
giant build --affected --base main 'go:**' --no-tag flaky
```

Means: targets that (a) match `go:**`, (b) are not flaky, AND (c)
were affected by file changes since main.

## How it composes

Order of operations:

1. Compute the set of eligible targets - apply test mode, then tag
   filters.
2. Apply pattern selection (positional includes / exclusions) to the
   eligible set.
3. If `--affected` is given, intersect with the affected set.

Same pipeline in `build`, `test`, `watch`, and `affected`. Same matcher.
A future TUI porcelain will drive it via the [NDJSON event
protocol](/reference/events/) - `selection.list_tags`, `selection.resolve`
- so the UI gets the same answers as the CLI.
