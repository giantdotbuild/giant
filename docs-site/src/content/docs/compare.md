---
title: How Giant compares
description: Where Giant sits next to make, just, Bazel, and Buck2.
---

Build tooling spans a wide range, from a 30-line `Makefile` to a
company-wide Bazel deployment. Giant aims at a specific spot in that range:
**the content-addressed cache and correct dependency graph of the big
tools, with the footprint of a small one, wrapping the commands you already
run instead of replacing them.** Here's how it lines up against the tools
people usually weigh it against.

## make

`make` is everywhere, needs no install, and for a small project it's hard
to beat. The friction shows up as a repo grows:

- **It keys on timestamps.** `make` rebuilds when a file's mtime is newer, so
  a `git checkout`, a `touch`, or a no-op edit triggers work that changed
  nothing. Giant keys on content hashes, so the same bytes hit the same cache
  entry across machines and checkouts.
- **No cross-language correctness.** Getting a Makefile to track
  dependencies correctly across, say, protobuf to Go to a Docker image is
  possible but manual and fragile. Giant's graph links targets by their
  declared inputs and outputs.
- **No remote cache.** Sharing build results across a team or CI means
  bolting something on. Giant speaks the Bazel HTTP cache protocol out of
  the box.

Giant doesn't replace `make` for a tiny project. It's for the point where a
Makefile's caching and correctness start costing you.

## just

`just` runs *tasks* - `just test`, `just deploy` - a nicer `make` for the
command-running job. It has no dependency graph, no incremental cache, no
notion of inputs and outputs, and doesn't try to; that's a different job from
a build system, and `just` does it well.

Giant draws the same line, on purpose. Named commands are the
[`giant-task`](/extending/giant-task/) porcelain - the part that overlaps
with `just`. The engine underneath, the caching and the graph, is the part
`just` doesn't have and doesn't try to. If you want "just, plus a real
build cache underneath your tasks," that's Giant with `giant-task`
installed.

## Bazel

Bazel is the reference design for hermetic, content-addressed, remotely
cached builds at scale, and Giant borrows its best ideas - the cache model,
the remote protocol (Giant is wire-compatible with bazel-remote), the
"static config, generated offline" split (Giant's generators are to
`giant.yaml` what Gazelle is to `BUILD` files). Where they part ways:

- **Bazel wants to own your build.** You express builds as rules in
  Starlark and adopt a rule ecosystem per language. Giant wraps the
  commands you already run - `go build`, `cargo build`, `docker build` -
  declared as `inputs → command → outputs` in plain YAML. No rule rewrite.
- **Size and ceremony.** Bazel is a large system with a daemon, a steep
  learning curve, and real adoption cost. Giant is one small binary, no
  daemon, readable in an afternoon.
- **Hermeticity.** Bazel's sandboxing and hermeticity are deeper and more
  battle-tested. Giant trusts your declared inputs and outputs by default,
  with an opt-in sandbox (`giant verify`) that runs targets isolated and
  flags undeclared reads. For many monorepos that trade - less enforcement,
  far less setup - is the right one; if you need Bazel-grade hermeticity at
  scale, Bazel earns its weight.

Short version: if you're adopting Bazel for the cache and the graph but not
the ecosystem, Giant gets you most of that benefit for a fraction of the
investment. If you need the full hermetic-rules ecosystem, Bazel is Bazel.

## Buck2

Buck2 (Meta's successor to Buck) is in Bazel's family: Starlark, a rule
ecosystem, a persistent daemon, and excellent performance at very large
scale. The contrast with Giant is the same as with Bazel - power and
ecosystem and a daemon versus a small, daemon-less engine that wraps your
existing commands - with the added note that Buck2's speed comes partly
from that always-on daemon, which is exactly the piece Giant chooses not to
have. Giant gets a warm engine only when you ask for one, via `giant
session`.

## At a glance

| | make | just | Bazel / Buck2 | Giant |
| --- | --- | --- | --- | --- |
| Content-addressed cache | ✗ (mtime) | ✗ | ✓ | ✓ |
| Remote / shared cache | ✗ | ✗ | ✓ | ✓ (Bazel HTTP) |
| Cross-language dep graph | manual | ✗ | ✓ | ✓ |
| Wraps your existing commands | ✓ | ✓ | rewrite as rules | ✓ |
| Own build language | ✗ | ✗ | Starlark | ✗ (YAML; Starlark only to *generate* config) |
| Daemon | ✗ | ✗ | ✓ | ✗ (opt-in `session`) |
| Deep hermeticity / sandbox | ✗ | ✗ | ✓ | opt-in (`giant verify`) |
| Footprint | tiny | tiny | large | tiny |

A row being blank isn't a verdict - `just` not having a build cache is the
point of `just`. The table is for placing Giant: it sits with the big tools
on caching and graph correctness, and with the small ones on footprint and
"wrap what you already run."

## What a feature table misses

Two things don't fit in a row:

- **The core is a protocol.** Builds run over [NDJSON](/reference/events/), so
  a TUI, an IDE, or a web app can control Giant through the same interface the
  CLI uses, without linking any of its code. See [Controlling
  Giant](/guides/controlling-giant/).
- **Capability ships as porcelains.** Tasks, the TUI, config generation are
  each a [porcelain](/extending/porcelains/) you install or skip. Uninstall the
  task runner and `giant task` stops resolving; the engine has no concept of a
  task to begin with. The toolset grows while the core holds still.

## Where Giant comes from

Giant condenses years of working in and on large repos: internal build
tools, the bespoke setups that grow up around them, and the lessons each
one charged for. That's why the design settled quickly - the expensive
mistakes were already paid for.

Bazel shaped it most. Its core ideas are the right ones (the section above
lists what Giant borrows), and it is engineered for planet-scale repos;
below that scale you carry the weight without collecting the payoff. Giant
keeps the ideas at a size that fits the repos most teams actually have.

Why YAML? Years in the Kubernetes world made it an easy choice. It has
warts, but everyone reads it, every language emits it, and a build graph
should be data you can diff in code review. When a repo outgrows
hand-written config, [generation](/guides/generating-config/) writes the
same YAML offline, so the syntax stays boring either way.
