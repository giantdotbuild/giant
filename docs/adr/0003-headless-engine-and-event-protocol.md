# ADR-0003 - Headless engine with NDJSON event protocol

- **Status**: Accepted
- **Date**: 2026-05-19

## Context

A build tool with a built-in TUI couples the core binary to a heavy UI
stack. CI invocations carry it. Container images carry it. Users who
want a different visualization (IDE plugin, web dashboard, custom CI
overlay) have no clean way in - they shell out to `giant build` and
parse human-readable output, which means every layout tweak risks
breaking integrations.

We want the engine to be the engine: a single static binary that
does builds and produces machine-readable output. Anything visual
sits on top.

## Decision

The engine is headless. It emits a stream of structured events;
external consumers render them however they want.

Three execution modes the same binary supports:

| Mode | Output |
| --- | --- |
| `giant build` | Built-in tty progress renderer (cursor-up tricks, ~400 LOC). |
| `giant build --events ndjson` | NDJSON event stream on stdout. |
| `giant serve` | Unix socket: bidirectional event stream + command channel. |

Visual consumers (`giant-tui`, `giant-web`, IDE plugins) are separate
binaries that connect to the socket or spawn `giant build --events ndjson`
and parse stdout. They are not part of the core distribution.

The event protocol is the contract. ~12 event types, ~6 commands.
Versioned. Documented separately.

## Consequences

### Enables

- Core binary stays free of ratatui, crossterm-heavy widgets, syntect,
  termimad, animation frameworks, ansi-to-tui, embedded docs assets.
  Tens of dependencies don't need to be in the build at all.
- CI images can install just `giant` without UI cruft.
- Multiple UIs can coexist (terminal + browser + IDE) on the same
  build, observing the same event stream.
- Test harness has a clean way to assert on engine behavior: read
  events, match expected sequence.
- Protocol versioning lets the engine and UIs evolve independently.

### Costs

- The built-in tty renderer is still ~400 LOC we own. Not zero.
- Two-process setup for the dashboard experience. Users running
  `giant build` see plain output unless they also have `giant-tui`
  installed.
- Protocol is a stable surface we have to evolve carefully.
- Daemon mode (`giant serve`) adds lifecycle concerns: PID files,
  graceful restart, config reload, version mismatch with clients.

### What we're committing to maintaining

- The NDJSON event schema with backward-compatible evolution (additive
  fields OK; renames need versioned events).
- The Unix socket protocol for `giant serve` (same schema, plus the
  command channel).
- A minimum tty renderer that's actually pleasant to use without a TUI.

## Alternatives considered

### Bundle a TUI in the engine binary

Smallest user-facing change. Worst engine bloat. The big concern is
that "everything in one binary" turns the engine into a rendering
host; every new core feature wants a tab. Rejected.

### Library-only engine + separate `giant-cli` and `giant-tui` binaries

Engine as a Rust library, all UI binaries link against it.

Rejected: forces users to install multiple binaries even for the basic
"build something" case. Single static binary is part of giant's pitch.

### gRPC instead of NDJSON

Protobuf schemas, generated clients, versioned via proto.

Rejected: heavy dependency stack (prost + tonic + ~50 transitive deps),
no human-readable debugging, can't pipe through `jq`. NDJSON is
debugger-friendly and parseable by every language without codegen.

### Build Event Stream (Bazel BES) compatibility

Match Bazel's BES schema so existing BES consumers work.

Rejected: BES is large, evolves with Bazel, and ties us to choices that
don't fit our model (e.g. action vs target distinction). We can support
a BES exporter later if anyone asks for it.

## Open issues

- **Daemon mode timing.** When does `giant serve` actually ship?
  Leaning: not until at least one multi-client porcelain (e.g. an
  IDE plugin alongside the TUI) genuinely needs it. The subprocess +
  NDJSON transport covers single-client cases.

## References

- TDD-0004 - Event protocol
