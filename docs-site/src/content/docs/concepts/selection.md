---
title: Selection language
description: Picking targets via labels, globs, exclusions, tags, and modes.
---

Every subcommand that operates on targets (`build`, `test`, `watch`,
`affected`) takes the same selection arguments. One language, one
matcher, used everywhere.

Targets are identified by their path-derived label, `//<package>:<name>`,
where the package is the directory holding the target's `giant.yaml`. See
[Packages](/concepts/packages/) for how labels are derived.

## Empty: everything

```bash
giant build                    # all non-test targets
giant test                     # all test targets
giant build --watch            # all non-test targets, continuously
giant test --watch             # all test targets, continuously (TDD loop)
giant build --with-tests --watch  # everything, test + non-test, continuously
```

## Labels and patterns

A label or pattern picks targets out of the package tree.

| Pattern | Matches |
|---|---|
| `//src/go/server:server` | one target by full label |
| `//src/go/server` | shorthand for the same - name defaults to the last path segment |
| `//src/go:*` | every target in exactly that package (no subpackages) |
| `//src/go/...` | every target at or under `src/go`, crossing subpackages |
| `//...` | every target in the whole workspace |

`*` matches a single path or name segment; it does not cross `/` or `:`.
`...` is the only construct that crosses package boundaries - it descends
recursively from its anchor.

```bash
giant build //cmd/server:server     # one target
giant build //cmd/server            # same, shorthand
giant build //src/go:*              # every target in the src/go package
giant build //src/go/...            # src/go and everything beneath it
giant build //...                   # the whole workspace
```

A literal label that doesn't exist is an error - and Giant suggests the
closest match:

```console
$ giant build //go/bin:srvr
no target matches "//go/bin:srvr"
  did you mean //go/bin:server?
```

Glob misses (patterns containing `*` or `...`) are silent - no targets,
no error. Only literal labels error out.

```console
$ giant build //rust/...
· no targets to build       # no targets under rust/; not a typo, just empty
```

## Exclusion

Prefix `!` to remove matches. Shell-special, so quote it:

```bash
giant build '//src/go/...' '!//src/go/internal/...'
giant build '!//src/go/...'        # everything except src/go and below
```

If only excludes are given, the implicit include is `//...` -
everything, minus your excludes.

## Tags

Language and kind live in `tags:`, alongside any free-form labels:

```yaml
- name: "server"
  tags: ["lang=go", "kind=bin", "release", "linux"]
  ...
```

Filter with `--tag` (include) and `--no-tag` (exclude). Multiple `--tag`
flags **union** - a target passes if it carries any of them. `--no-tag`
drops a target that carries any excluded tag, and composes with `--tag`.
Each flag takes one whole tag; there's no comma syntax:

```bash
giant build --tag kind=bin                   # only binaries
giant build --tag kind=bin --no-tag flaky    # binaries AND NOT flaky
giant build --tag lang=go --tag lang=rust    # Go OR Rust (union)
```

Tags compose with patterns:

```bash
giant build '//src/...' --tag release    # release-tagged AND under src/
```

## Test mode

`giant build` excludes `test: true` targets by default. `giant test`
selects only test targets:

```bash
giant build                   # 12 production targets
giant test                    # 47 test targets
giant test '//src/go/...'     # only Go tests under src/go
giant test //cmd/server:server   # error - server isn't a test
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
giant build --affected --base main '//src/go/...' --no-tag flaky
```

Means: targets that (a) match `//src/go/...`, (b) are not flaky, AND (c)
were affected by file changes since main.

## Re-running failures

`failed-last` re-selects the targets that failed in the most recent
build:

```bash
giant build failed-last       # retry just what broke
```

`failed-last` is a whole-selection token: it must be the
only argument, and tag filters don't apply to it (it replays exactly the
recorded set). It does still intersect with `--affected`, so
`giant build failed-last --affected --base main` retries only the failures
that touch changed files.

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
