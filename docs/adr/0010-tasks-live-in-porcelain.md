# ADR-0010 - Tasks live in porcelain binaries, not in core

- **Status**: Accepted
- **Date**: 2026-05-20
- **Supersedes**: [ADR-0005](0005-tasks-stripped-to-bone.md)

## Context

A fully-featured task subsystem (services with `after` DAGs, ready
probes, log modes, restart-on-rebuild, nested task hierarchies) is
genuinely useful - the integration-test scenario "start a database,
run tests, stop services" is a real workflow. The cost is large
surface area in the build engine, and users without tasks reasonably
ask "why is this in the build tool?"

ADR-0005 attempted to address this by stripping tasks down to bone -
keep `command` + `deps` + `args`, drop everything else. That cuts
weight but doesn't address the bigger question: should tasks be a
*first-class concept in the build tool* at all?

Four candidate shapes surfaced:

| | Surface in core | UX | Discoverability |
|---|---|---|---|
| A. Full tasks in core | huge | great | great (`giant <task> --help`) |
| B. No tasks; use Just/make alongside | zero | two tools, fragmented | bad |
| C. Tasks ARE targets, `giant <id>` shortcut (Buck2 model) | ~30 LOC | ugly: `giant build deploy -- prod`, no typed args | poor |
| D. Porcelain layer: separate `giant-task` binary | zero in core, opt-in | clean, owns its own UX | good if installed |

A keeps growing. B loses the task→target dep integration that makes
giant tasks useful. C copies Buck2's UX problems. D is the model
git, cargo, and kubectl have used successfully for years.

## Decision

Adopt the porcelain pattern.

- **Core `giant` has zero task surface.** No `tasks:` field in
  `giant.yaml`. No service supervision. No typed task args. No
  task hierarchies. No `--watch`-style task flags beyond what
  `giant watch` already provides for targets.
- **Optional `giant-<name>` binaries on PATH extend giant.**
  `giant <name>` dispatches to `giant-<name>` if `<name>` is not a
  built-in subcommand. Same model as git, cargo, kubectl, jj.
- **Communication between porcelain and core uses the NDJSON
  protocol** (TDD-0004) over one of three transports chosen
  per-porcelain:
  - **Subprocess + stdout** for one-shot consumers (`giant-task`
    spawning `giant build X --events ndjson`).
  - **Subprocess + stdin/stdout** for long-lived single-client tools
    (`giant-watch-tui` controlling a watch session via piped
    commands).
  - **Unix socket via `giant serve`** for multi-client tools
    (`giant-web` serving multiple browser sessions over one shared
    workspace).

The protocol is the same across transports. Porcelains pick the
transport that matches their needs.

## Consequences

### Enables

- Users who don't want tasks install just `giant`. Their footprint
  is the minimum.
- Users who do install `giant-task` (or whatever porcelain fits).
- Community-built porcelains: `giant-deploy`, `giant-bench`,
  `giant-fmt`, anything else. None require core changes.
- Porcelain bugs don't affect builds; build bugs don't break
  porcelains.
- Different teams can ship different porcelains opinionated for
  their workflows without coordinating with us.
- `giant <name>` feels monolithic to users even when the
  implementation is two processes.

### Costs

- Documentation surface grows: two tools to teach when porcelain is
  in play.
- "Where do tasks go?" is a new question for users - has to be
  answered in the README.
- Discoverability for the porcelain-not-installed case requires a
  helpful "no such subcommand" error.
- Some shared code (config parsing) requires the porcelain to
  either re-implement it or depend on giant as a Rust library.

### What we're committing to maintaining

- Subcommand dispatch in core: any unrecognised subcommand →
  `giant-<name>` on PATH → exec. Clear error when not found.
- The NDJSON event + command protocol (TDD-0004) is the stable
  contract between core and porcelains. Breaking changes require a
  protocol version bump.
- Both subprocess and (future) socket transports speak the same
  protocol.

## Alternatives considered

### A. Tasks in core

Rejected: large subsystem of a build tool whose pitch is "small,
modular, easy to grasp." The ergonomics are good, but the cost is the
largest single contributor to engine surface area. Not worth it.

### C. Tasks = targets with `giant <id>` shortcut (Buck2)

Rejected: ugly UX. No typed args, no per-task `--help`, the `build`
word shows up in unrelated commands. Buck2 users put up with it
because Buck2 has features that earn forgiveness; giant doesn't have
the headroom to spend on UX.

### B. Don't ship task support at all

Rejected: users would install Just / make alongside, then maintain
parallel dependency lists (one in `giant.yaml`, one in `Justfile`).
Friction without payoff.

### Single porcelain shipped with core via a feature flag

Rejected: keeps the surface area in one repo. The point of porcelain
is that the binary is *optional* and *opt-in*. With a feature flag,
users still build it by default and it ends up in their install
unless they remember to opt out.

### Plugin DLLs / dynamic loading

Rejected outright: ABI versioning, security surface (loaded code
runs in our process), platform-specific dynamic loading
(`dlopen`/Windows DLLs/macOS dylibs). Subprocess-based plugins are
strictly simpler and easier to reason about.

## Migration

ADR-0005 said "stripped tasks live in core." This ADR says "no tasks
in core; they live in porcelain." `giant-task` becomes its own crate
in `crates/giant-task/`.

## References

- [git's subcommand dispatch](https://github.com/git/git)
- cargo plugins (cargo-edit, cargo-watch, cargo-nextest, etc.)
- kubectl krew plugin system
- jj's subcommand dispatch
- [TDD-0004 - Event protocol](../tdd/0004-event-protocol.md)
- [ADR-0005 - Tasks stripped to bone (superseded)](0005-tasks-stripped-to-bone.md)
- [ADR-0020 - giant-task workhorse charter](0020-giant-task-workhorse-charter.md) (defines this porcelain's scope)
