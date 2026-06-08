# ADR-0034 - Core shrinks to session + completions; commands become porcelains

- **Status**: Accepted
- **Date**: 2026-06-08
- **Builds on**: ADR-0003 (headless engine), ADR-0010 / ADR-0021 (porcelain
  dispatch), ADR-0032 (static graph), ADR-0033 (protocol query surface)

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

**Coupling: porcelains link the giant library, not the protocol.** The library
crate is the engine and a reusable workspace API (config scan, graph, cache,
selection, git, cache-key). First-party CLI porcelains link it directly and call
its public API - the giant-gen / giant-graph pattern - rather than spawning a
`session` subprocess. The NDJSON protocol remains for out-of-process and
interactive clients (the TUI, IDEs); a CLI command that just reads or runs once
has no reason to pay for a subprocess and a serialization round-trip.

This needs one small public entry point - **`prepare(config) -> Prepared
{ graph, cache, workspace_root, config }`** (today's internal `cli::prep`) -
alongside the already-public `cache` / `selection` / `git` / `executor`
cache-key functions. That is the porcelain contract.

**Crate layout:** one porcelain crate per command (matching the existing
porcelains), except `build` / `test` / `verify` share one crate - they share the
progress renderer and the in-process build adapter (`run_one_build`), which move
there. `test` is build with a test selection; `verify` is build --sandbox
--fresh.

## Phasing

- **Phase A (inspection, trivial):** `affected`, `clean`, `explain`, `logs`.
  Thin library clients. `clean` reads only `cache.dir`; `affected` needs the
  graph + git; `explain` / `logs` reuse `breakdown_for_target` and the cache
  reads. Each relocates the existing `cli/<cmd>.rs` into a porcelain crate.
- **Phase B (the renderer move):** `build`, `test`, `verify` into one
  `giant-build` crate; `renderer.rs` and `run_one_build` move with them. After
  this the binary has only `session` + `completions`.

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
