# Giant architecture

Giant builds and caches a workspace by running a static graph of targets. The
engine is small and language-agnostic: it knows files, commands, and content
hashes, and little else. Everything specific - languages, tasks, terminals,
remote caches, config generation - lives outside the engine in separate
binaries.

This is the overview. The code under `crates/` is the detail, and the
guiding principles for working on it are in the contributor docs.

## A small engine and many porcelains

The core is an incremental engine over a content-addressed cache that speaks a
line protocol. A target is `inputs → command → outputs`. The engine loads a
graph of targets, computes a content hash for each, runs the command on a cache
miss, stores the outputs, and emits a stream of NDJSON events while it works.

Around that core, every user-facing capability is a separate binary - a
*porcelain* - found the way git finds `git-<name>`. Typing `giant <name>` runs
the built-in subcommand if there is one, otherwise it execs `giant-<name>` from
PATH. `giant build`, `giant test`, `giant tui`, `giant task`, `giant gen`,
`giant logs`, `giant explain`, and `giant graph` are all porcelains. They reach
the engine one of two ways: by linking it as a library and running builds in
process (the build family), or by speaking the NDJSON protocol to a
`giant session` subprocess (the read-only and UI porcelains). A web UI, an
editor plugin, or a CI annotator is just another consumer of the same protocol.

The point of this split is to keep the core small enough to read in one sitting,
and to keep every heavyweight concern - a TUI framework, a remote-cache client,
a task runner with service supervision - out of the engine binary. You install
the porcelains you use. Uninstall `giant-task` and the notion of a task is gone;
the engine never knew what one was.

## Targets, the graph, and the cache key

A target declares its file `inputs`, a shell `command`, and the `outputs` the
command produces. Dependencies between targets are explicit `deps`. The engine
builds a DAG, runs it in topological order up to a parallelism budget, and for
each target computes a cache key: a blake3 hash composed in labelled sections
from the command, the working directory, the environment, the content of every
input file, and the output hashes of its dependencies. A new kind of input adds
a new section to the key rather than overloading an existing one.

A cache hit restores the recorded outputs and replays the captured stdout and
stderr, so a cached build still shows the warnings the original produced. Two
targets feed downstream cache keys through their combined output hash, which
gives early cutoff: if a dependency rebuilds but its outputs are unchanged, its
dependents stay cached.

Some artifacts live outside the filesystem - a Docker image in a registry, an
object in S3. A target can declare an `exists` command that, when it succeeds,
marks the artifact present and skips the build. The command sees the cache key
in the environment, so it can name the remote artifact after Giant's own
identity.

## Config is static; generation is offline

The engine reads plain `giant.yaml` (or `giant.json`) and nothing else. There
is no discovery phase and no scripting language inside the engine. YAML is sugar
over one JSON schema; both parse to the same value tree, and that schema is the
contract between the engine and whatever produced the config.

Config is split across the tree. Every directory with a `giant.yaml` is a
package, and a target's identity is derived from where it lives:
`//<package>:<name>`. The engine scans the workspace, reads each package file,
and merges them into one graph. A name only has to be unique within its package.

The expressive part of configuration - build matrices, per-language targets,
anything repetitive - is a generator's job, run ahead of the build. This is the
same split as a configure step that emits a `build.ninja` for a fast, dumb
build step to execute: the slow, smart work happens once and offline; the engine
stays fast and free of logic.

## The build graph is explicit

The engine resolves `deps` as written and does no dependency inference of its
own. Output-to-producer resolution - "target B reads a file that target A
produces, so B depends on A" - runs as a link pass inside generation, which
fills in `deps` for generated targets. Hand-written targets declare their own.
The engine keeps only an O(n) check that no two targets claim the same output.

Keeping inference out of the engine is what lets generation and hand-written
config compose without the engine learning anything about either.

## Generation: an embedded Starlark host

`giant gen` runs the workspace's generators and writes their output as
`giant.<name>.yaml` files, one filename infix per generator. The blessed
generator embeds a Starlark interpreter: a `giant.star` describes how to turn
the tree into targets, calling a `target()` builtin that emits the typed wire
struct the engine reads. Language opinions - how to enumerate Go packages, how
to lay out a Docker build - live in Starlark, shipped as a standard library,
never compiled into the engine.

The standard library lives in its own repo (giantdotbuild/giant-std) so it can
move faster than the binaries. A workspace pins it with a `std:` block in the
root config (a tag or commit, no floating "latest"); `@std//` modules are
fetched once per pin into the cache dir and read from disk after that, so
generation stays offline past the first fetch. A `GIANT_STD` directory or a
vendored copy in the repo (`giant gen vendor`) overrides the pin entirely.

Emission is deterministic, so `giant gen --check` is a byte-diff gate: it
regenerates into a scratch directory and fails if the result differs from
what's committed, which keeps generated config from drifting from its source.

## Caching: a local store, an optional HTTP remote

The local cache is content-addressed: outputs and captured logs are stored as
blake3-named blobs, with an action-cache entry per cache key, and LRU eviction
when the cache grows past a configured size.

A remote cache is optional and feature-gated. It speaks the Bazel HTTP cache
protocol, which works against bazel-remote, an S3 bucket behind a small shim,
or any HTTP object store. The richer REAPI gRPC protocol was considered and set
aside: it is a large surface to implement and maintain for a build tool this
size, and the HTTP protocol covers the cache-sharing case that matters.

## Tasks live in a porcelain

The core has no `task` subcommand and no `tasks:` schema. `giant-task` owns all
of it: it reads the `tasks:` block, runs commands with their declared
arguments, builds any target dependencies first through the engine, and handles
the long-running and orchestration features (services with readiness probes,
`needs`, `finally`, shell completions). `giant <task>` falls through to it once
built-ins and other porcelains have had their turn, so a task feels like a
first-class command without the engine knowing tasks exist.

## Sandboxing and verify

Sandboxing is enforcement, applied by wrapping the command at execution time
under birdcage (Landlock and seccomp on Linux). Nothing is sandboxed unless the
run opts in with `giant build --sandbox`; the per-target `sandbox` field
(default `true`) only marks eligibility, so `sandbox: false` exempts a target
that cannot yet run confined. `network: true` grants a sandboxed target the
network. The wrapper is not part of the cache key, so a sandboxed run and a
plain run of the same target share cache entries.

`giant verify` is the audit. It builds every target sandboxed, with the cache
bypassed, inside a disposable git worktree of the committed state. Building in
the worktree means a destructive or under-declared command can only touch the
throwaway, so the audit can never damage the working tree - the failure mode
that retired the earlier live-tree approach. Outputs are discarded; the audit
only cares whether each target built without reaching for something it didn't
declare. When a sandboxed command fails, the failure is annotated with the
likely cause - an undeclared path, a denied network reach, or a sandbox that
could not start - rather than a bare exit code.

## What Giant leaves out

The engine has no service supervisor, no embedded scripting, no in-process TUI
or docs viewer, and no required daemon. It does not implement REAPI. Each of
those is either someone else's tool (process-compose for long-running services,
nix or mise for toolchains) or a porcelain on the other side of the protocol.
The engine's job is files, commands, and hashes; holding that line is what keeps
it small.
