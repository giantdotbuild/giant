# ADR-0034 - Core shrinks to session + completions; commands become porcelains

- **Status**: Accepted
- **Date**: 2026-06-08
- **Builds on**: ADR-0003 (headless engine), ADR-0010 / ADR-0021 (porcelain
  dispatch), ADR-0032 (static graph), ADR-0033 (protocol query surface)

> **Revised 2026-06-08**: the coupling rule below originally said "porcelains
> link the giant library" as a blanket default. That was too coarse. The
> refined rule sorts porcelains by *what data they need* into three categories -
> engine-computed state goes over the NDJSON protocol, static committed data is
> read directly, fs/config maintenance reads config - so lib-linking is the
> fallback, not the default. `explain` and `logs` are protocol clients, not
> lib-linked. See "Decision - coupling" and "Phasing" below.

## Context

The `giant` binary still bundles most user-facing commands: `build`, `test`,
`verify`, `affected`, `explain`, `logs`, `clean`, plus `session` and
`completions`. ADR-0003 made the engine headless; ADR-0021 added `giant-<name>`
PATH dispatch; ADR-0032 made the graph static; ADR-0033 added the protocol query
surface. Together those removed the last reasons most of these commands had to
live in core: `graph` already left (a porcelain reading static config), and
`explain` / `logs` became movable once `query.explain` / `logs.get` existed.

What actually has to be in the binary is small: the engine-as-a-server
(`session`) and the dispatcher's own completion (`completions`). Everything else
is a thin layer over the engine.

## Decision

**The giant binary's command surface shrinks to `session` + `completions` + the
`giant-<name>` PATH dispatch. Every other command becomes a porcelain.**

- **`session`** stays - it *is* the engine exposed over NDJSON (ADR-0003). It is
  the only necessary command.
- **`completions`** stays - it is the dispatcher's own concern. Static scripts
  cover the binary's surface; dynamic completion (`clap_complete::CompleteEnv` +
  `detect_porcelains`) already enumerates installed `giant-*` porcelains at
  TAB-time, so `giant <TAB>` lists them without regeneration, and each porcelain
  completes its own args.
- **`watch` is not a command** and never was: it is `build --watch` plus the
  protocol's `watch.start` / `watch.stop`. The vestigial `"watch"` entry in
  `BUILTIN_SUBCOMMANDS` is removed.

**Coupling: pick the interface by what data the porcelain needs.** Core does the
engine and emits NDJSON; it presents no UI, CLI or otherwise. Porcelains present.
That sorts them into three categories:

1. **Engine-computed state → the NDJSON protocol.** Cache status, the explain
   cache-key breakdown, captured logs, build progress - data the engine computes
   from the graph and cache. The porcelain spawns (or attaches to) a `session`
   and renders the events; it does not recompute. `explain` (over `query.explain`),
   `logs` (over `logs.get`), `build` / `test` (over `build` + the event stream),
   and the TUI all live here. This is the pure model and the one to reach for by
   default: the engine lives in exactly one place, the same protocol feeds the
   CLI and the TUI, and a future warm daemon (ADR-0003) answers without
   rebuilding the graph per invocation - something a lib-linked command can never
   do.
2. **Static committed data → read it directly.** ADR-0032 put the build graph on
   disk, so `graph` and `affected` (= static graph + a `git diff`) need neither
   the engine nor a subprocess. They use `prepare()` to load the graph and run.
3. **fs / config maintenance → read config.** `clean` wipes `cache.dir`; there is
   nothing to render, so there is no protocol round-trip to justify.

Lib-linking is the fallback for categories 2 and 3, not a blanket default. The
static/config porcelains use one small public entry point - **`prepare(config)
-> Prepared { graph, cache, workspace_root, config }`** (today's internal
`cli::prep`) - alongside the public `cache` / `selection` / `git` / `executor`
cache-key functions. The protocol porcelains use the wire types
(`commands::Command` / `events::Event`) plus a shared session-client helper that
spawns `giant session`, sends one correlated command, and collects the reply.

The wire types live in a standalone **`giant-protocol`** crate - `Command`,
`Event` (+ payloads), `TargetId`, and the `query_session` client - so the
protocol porcelains (`giant-explain`, `giant-logs`) depend on it alone and do
not compile the engine into their binary. The engine re-exports the crate's
modules (`giant::commands` / `giant::events` / `giant::TargetId`) so engine
internals and engine-linking porcelains are unaffected. The TUI is the one
"protocol" consumer that still links the engine - not for the wire types, but
because it reuses `giant::selection` for client-side target filtering; that is a
deliberate exception, not the wire-type coupling this crate removed.

**Crate layout:** one porcelain crate per command (matching the existing
porcelains), except `build` / `test` / `verify` share one crate - they share the
progress renderer and the in-process build adapter (`run_one_build`), which move
there. `test` is build with a test selection; `verify` is build --sandbox
--fresh.

## Phasing

- **Phase A (inspection):**
  - `affected` (category 2, static graph + git) and `clean` (category 3, config)
    relocate their `cli/<cmd>.rs` wholesale into porcelain crates. **Done.**
  - `explain` (category 1) becomes a protocol client over `query.explain`, and
    `logs` (category 1) over `logs.get`. Their `cli/<cmd>.rs` is deleted, not
    moved: the engine already computes the data in the session handlers. The
    `breakdown_for_target` / `walk_target` helpers stay in core (the session uses
    them) and move out of `cli/` into a library module. To keep full fidelity the
    protocol gains additive fields: `query.explained` carries the cache-hit detail
    (built-at, duration, exit, outputs, `outputs_content_hash`) and `logs.get`
    takes an optional `key` to inspect a specific historical AC entry.
- **Phase B (the renderer move): Done.** `build` / `test` / `verify` move into one
  `giant-build` crate (three bins sharing `BuildArgs` + a `run` fn). The renderer
  moves with them; the engine keeps the in-process build adapter and exposes it
  (`giant::run_one_build` / `run_watch_command` / `resolve_sandbox` + the prep
  helpers), so the porcelain links the engine and renders the event stream rather
  than recomputing - these are category 1, but a subprocess-protocol build is
  deferred (the in-process path avoids a subprocess for the most common command).
  `format_duration` (shared with the task renderer) drops into a tiny
  `giant::fmt`. After this the binary has only `session` + `completions`.

  Two follow-on changes fell out of the move: (1) dispatch now looks for
  `giant-<name>` *next to the giant binary* before PATH, so the co-installed
  suite (and a dev `target/<profile>` tree) resolves porcelains without PATH
  fiddling; (2) the giant-level `--fresh` / `--sandbox` global flags are removed -
  with build/test/verify gone they had no reader, and as `global` flags they
  swallowed a porcelain's own `--fresh` / `--sandbox` before dispatch. Clients
  pass `fresh` per build over the protocol; `--sandbox` is giant-build's flag.

## Consequences

- The binary's command surface collapses to two commands + dispatch. The
  porcelains compose against one stable library API.
- **Honest scope note:** this relocates the CLI veneer, it does not shrink the
  engine. The library (cache, executor, graph, scheduler) is the engine and
  stays; the "small core" budget was always about that engine, which is
  unaffected. What shrinks is the binary's surface and coupling, not the LOC of
  the substrate.
- The public `prepare()` + cache/selection/git/key functions become a contract
  to keep stable, like the protocol. They are versioned with the crate, so this
  is cheaper to evolve than the wire protocol.
- More crates, and the Phase B renderer move is the heavy lift. Phase A is
  mechanical relocation.
- `build`/`test`/`verify`/`affected`/`explain`/`logs`/`clean` only resolve once
  their porcelains are installed (the giant-suite flake package installs the
  whole set); a bare `giant` install gets `session` + `completions` and the
  dispatch hints.

## Relationship to prior decisions

Completes the trajectory of ADR-0003 (headless), ADR-0010/0021 (dispatch),
ADR-0032 (static graph), ADR-0033 (query surface). The protocol stays the
contract for out-of-process clients; the library API is the contract for
first-party porcelains.
