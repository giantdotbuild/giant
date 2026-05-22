---
title: giant-tui - the interactive browser
description: A terminal UI for browsing the catalog, kicking off builds, and watching results.
---

`giant-tui` is an interactive terminal browser for your workspace.
Pick a selection with filters and tags, hit `b` or `Enter` to build,
press `w` to watch. It's a separate binary dispatched via
`giant tui` (Giant itself doesn't ship a `tui` subcommand; the
dispatcher execs `giant-tui` on PATH).

```console
$ giant tui
```

```
giant ─ my-monorepo                                42 targets
─ Browse ────────────────────────────────────────────────────────
  go:bin:server       ✓ idle
> go:bin:worker       · building
  go:test:auth        ✓ cached
  go:test:store       ✗ failed
  docker:api          · queued
─ Filters ───────────────────────────────────────────────────────
  /                   ─ open search
  t                   ─ tag picker
  T                   ─ toggle test-only
  Tab                 ─ cycle status filter
```

Under the hood it spawns one `giant session` subprocess and drives
it with NDJSON commands. The TUI itself does no building - the
engine does. See [TDD-0013](https://github.com/johnae/giant/blob/main/docs/tdd/0013-giant-tui.md)
for the design.

## Install

```bash
# From crates.io (once published)
cargo install giant-tui

# From source
cargo install --path crates/giant-tui --git https://github.com/johnae/giant
```

The binary just needs to be on PATH. Verify:

```console
$ giant-tui --version
giant-tui 0.1.0

$ giant tui --version   # via the dispatcher
giant-tui 0.1.0
```

## Pre-seed a selection

Patterns on the command line pre-fill the search filter:

```bash
giant tui 'go:**'           # opens with go targets pre-selected
giant tui 'go:test:*'        # narrowed further
```

Same selection grammar as `giant build` ([selection
language](/concepts/selection/)).

## Keys

### Browser (catalog view)

| Key | Action |
|---|---|
| `Enter` / `b` | Build the current selection |
| `w` | Watch the current selection |
| `/` | Open the search input. `Esc` cancels, `Enter` accepts. |
| `t` | Open the tag picker (multi-select include/exclude) |
| `T` | Toggle "test targets only" |
| `A` | Affected-mode filter - prompts for a git ref baseline; engine computes the set, keeps a file watcher pinned, and re-emits when the set changes. `A` again clears it. |
| `R` | Force a refresh of the affected set (re-subscribes to the same base) |
| `Tab` / `f` | Cycle the status filter (`all` → `failed` → `cached` → `built` → `all`) |
| `c` | Clear all filters |
| `j` / `k` / `↑` / `↓` | Move cursor |
| `g` / `G` | Jump to top / bottom |
| `PgUp` / `PgDn` | Page-scroll |
| `?` | Help overlay |
| `q` / `Ctrl-C` | Quit |

### During a build (or watch)

The build view has two panes - the target list on top, the log pane
on the bottom. `Tab` switches focus between them; the focused pane's
border is highlighted.

| Key | Action |
|---|---|
| `Tab` | Switch focus (target list ↔ log pane) |
| `j` / `k` / `g` / `G` / `PgUp` / `PgDn` | Scroll the focused pane |
| `Ctrl-↑` / `Ctrl-↓` | Shrink / grow the log pane |
| `/` | Substring-filter the log pane (case-insensitive). `Esc` clears, `Enter` commits. |
| `f` | Cycle status filter on the running list |
| `c` | Clear filters |
| `Esc` | Cancel the in-flight build (or `watch.stop` when watching) |
| `Ctrl-C` | Same as `Esc` while running; quits otherwise |
| `q` | Quit (sends shutdown) |
| `?` | Help overlay |

### After a build finishes

The result screen behaves like the running view, plus:

| Key | Action |
|---|---|
| `Esc` / `Enter` / `b` | Return to the browser |

### Tag picker modal

Multi-select with three states per tag: neutral, include, exclude.

| Key | Action |
|---|---|
| `j` / `k` | Move cursor |
| `Space` / `i` | Cycle the tag at cursor (neutral → include → exclude → neutral) |
| `c` | Clear all tag filters |
| `Esc` / `Enter` / `t` / `q` | Close the picker |

## What you'll see

The catalog list is sorted by ID and reflects three layers of
filtering: search substring, status, and tag include/exclude.
Test targets are hidden by default - press `T` to surface them
(matches `giant build`'s rule).

A status badge in the top-right shows the engine state:

- `idle` - engine ready, nothing running
- `building` - a one-shot build is in progress
- `WATCHING` - a watch session is active; file changes trigger
  rebuilds until you press `Esc` (or `Ctrl-C`)

The browser also shows a row of filter chips for whatever's
narrowing the catalog: the search query, included/excluded tags,
`tests-only`, and (if affected mode is on) an
`affected:<ref>` chip with the current count. The chip also surfaces
the refresh state - `affected:main…` while the engine is computing,
`affected:main (error: …)` if the git ref couldn't resolve.

During a build, the header shows a live summary:

```
giant - building 7/949 · 34 built · 120 cached · 1 failed · 12s
```

These counts update as `target.finished` events arrive - you don't
have to wait for the build to complete to see how things are
trending.

During a build, each target's stdout/stderr streams into a side
pane keyed to your cursor. Selecting a target shows that target's
log lines (or a "(cached)" placeholder if it was a cache hit
without replay).

## Watch mode in the TUI

Pressing `w` sends `watch.start` to the session with the current
selection. Subsequent file edits run rebuilds in place: the
`WATCHING` badge stays lit, and each affected target re-renders
its log. Press `Esc` (or `Ctrl-C`) once to send `watch.stop` and
return to the browser.

The TUI doesn't implement file watching itself - that's the
engine's job. The TUI is purely a viewer + command source.

## Piped / non-TTY usage

If stdout isn't a TTY (you piped output, redirected to a file,
ran inside CI), `giant-tui` falls back to invoking
`giant build` with the same patterns and exiting with its status.
This makes scripts like `giant tui 'go:**' | tee build.log`
do something sensible instead of dumping raw terminal escapes.

## How it composes with the engine

```
┌─────────────────┐       ┌─────────────────┐
│  giant-tui      │ stdin │  giant session  │
│  (renderer +    ├──────►│  (engine)       │
│   state machine)│       │                 │
│                 │ stdout│                 │
│                 │◄──────┤                 │
└─────────────────┘       └─────────────────┘
   ratatui + crossterm        loads config,
                              runs discovery,
                              executes builds
```

- **One session per TUI invocation.** Closing the TUI closes
  stdin, which the session treats as the shutdown signal.
- **Commands.** The TUI emits `build`, `watch.start`, `watch.stop`,
  `cancel`, `shutdown` (full list in
  [Command channel](/reference/events/#command-channel)).
- **Events.** The TUI consumes the entire [NDJSON event
  protocol](/reference/events/) - `target.started`, `target.log`,
  `target.finished`, `watch.batch`, etc.

If you want to write your own TUI / IDE plugin / dashboard,
spawn `giant session` the same way and speak the same protocol.
Nothing about `giant-tui` is privileged.

## Environment variables

| Variable | Meaning |
|---|---|
| `GIANT_TUI_BUILD_BIN` | Override the `giant` binary used to spawn the session subprocess. Useful in tests; rarely needed otherwise. |
| `NO_COLOR` | Respected - affects how the catalog and log panes render. |

## What giant-tui DOESN'T do

A short list, deliberately:

- **No mouse support.** Keyboard-only. Mice in terminals are a
  swamp of incompatible escape sequences.
- **No task running.** Tasks are `giant-task`'s job (see
  [giant-task](/extending/giant-task/)).
- **No remote / daemon mode.** One TUI, one session, one
  workspace.
- **No log search.** Scroll, but no `/` inside log panes yet.
  The catalog search (`/` in the browser) is the only search.
- **No multi-pane layout customization.** The browser → result
  split is fixed.
- **No reading config independently.** The catalog comes from
  `target.described` events emitted by the session at startup,
  not from the TUI re-parsing `giant.yaml`. If you change
  config, restart the TUI.

If you want any of these, the session protocol is open - write
your own porcelain. See [Porcelains](/extending/porcelains/).
