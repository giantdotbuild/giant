# ADR-0029 - The blessed generator is an embedded Starlark host

- **Status**: Proposed
- **Date**: 2026-06-05
- **Deciders**: Mr 9k

## Context

ADR-0027 shipped
reference generators (`giant-gen-go`, `giant-gen-docker`) written in the
language of the ecosystem they serve, a per-language helper library, and a
single workspace-root `giant-gen.yaml` carrying declarative policy. It drew a
firm line in §6: config is data, programmability is a generator, and giant does
**not** bundle a config language, because doing so would create two ways to be
programmable (a config DSL *and* write-a-generator) and "two half-mechanisms
are worse than one whole one."

That line was right, and this ADR does not cross it. What forces a revisit is
not the principle but where a data-only policy file leads under real build
needs.

A declarative policy schema grows a knob per case it cannot otherwise express,
and the needed cases pile up quickly: skip a package; emit the binary but not
its test; give DB-backed tests different deps and caching than pure-unit tests;
drive a cross-build matrix of `os/arch` pairs with named platform sets; let a
cross artifact match an existing filename convention via an output template. The
trajectory is the problem, not any single knob: each one re-implements, one YAML
field at a time, a conditional, a loop, a lookup, or a template that a
programming language already has. That is the classic path where config slowly
becomes a bad programming language. Two problem classes make the ceiling
concrete:

- **The floor with sharp edges.** Even "ordinary" build graphs need computed,
  not configured, decisions: mixed output-naming conventions in one repo (a
  dotted runtime-contract path alongside a dashed one), ad-hoc per-package cross
  shapes (one target host-built plus a cross variant, another cross-only), a
  constant dep set spread across every target, and a path-predicate that
  separates DB-backed tests (never cached, extra deps) from cached pure-unit
  tests. Each of these had already cost, or would cost, a knob.
- **The ceiling.** cgo cross-compilation is the hard tail: per-arch sysroots,
  cross-compiler target selection, native-versus-cross flag branching,
  library-group to include-directory maps, auxiliary C-dependency fetch targets
  that a binary then depends on, and hierarchical per-directory metadata that
  merges down the tree. This is all conditionals, arch-to-string maps,
  group-to-directory maps, string assembly, and emitting several related targets
  from one logical unit. No fixed knob set models it; expressing it
  declaratively would need a dozen more knobs we would never finish adding.

ADR-0027 §6 already named the correct escape hatch: "the moment a rule would
need real logic, that is the signal to write a custom generator." The
trajectory above is that signal, arriving for the common case rather than the
long tail. The thing ADR-0027 left unbuilt is the *blessed* on-ramp for it. Its
answer was per-language SDKs (write a compiled generator against the helper
library), but in practice customization takes the path of least resistance, and
that path was adding YAML knobs, not forking a compiled generator. A blessed
generator that is itself programmable in a familiar language is the on-ramp that
makes "write logic" cheaper than "add a knob."

The lines ADR-0024 and ADR-0027 drew still hold and are not reopened: the
engine knows nothing about generation, it reads only static `giant.*.yaml`, and
all dynamism is offline and above the waist. The generator runs at `giant gen`
time and emits static, checked-in, drift-gated YAML; the engine never evaluates
a language at load. Dynamism in the generator is unbounded and always was; the
line we do not cross is dynamism the *engine* evaluates.

## Decision

### 1. The blessed generator is an embedded dynamic-language host, not a knob set

Giant ships one general-purpose generator whose logic is **user-authored in a
dynamic language**, so loops, conditionals, lookups, and templates are ordinary
code rather than schema fields. Authoring source is `giant.star` at the
workspace root (plus `load()`-ed library files); the emitted `giant.*.yaml` are
the output, still checked in and `--check`-gated. Every would-be knob becomes
plain code, and the cgo-cross logic becomes a library function. We stop minting
knobs because there is no longer a knob schema to extend: users define their own
"knobs" as functions.

This is the **resolution of ADR-0027 §6, not its reversal.** ADR-0027 rejected a
config language *inside `giant-gen.yaml`* because it would sit alongside
write-a-generator as a second mechanism. This ADR has no `giant-gen.yaml` DSL
and no second mechanism: there is one mechanism, the generator, and that
generator is programmable. "One whole mechanism" is honored exactly. The price,
named plainly, is that the dynamic surface (`ws.exec`, `ws.glob`, `target()`,
the bundled stdlib) becomes a **versioned contract** users write against, where
the knob set was a closed schema we could reshape freely. A small stable API
beats an ever-growing knob list, but it is an API, and the TDD must say what is
stable and what may change.

### 2. The language is Starlark

The dynamic language is Starlark, embedded via `starlark-rust` (the Meta
implementation that powers Buck2). The argument, since this is the decision a
reviewer will question:

**A filter rules out most of the field before taste enters.** The strongest
reason to host the generator in-process at all (decision 4) is sharing giant's
own wire type, so there is one schema definition rather than two that can
diverge. That benefit only accrues to a language giant can embed in its own Rust
process. It rules *in* Starlark (`starlark-rust`), Jsonnet (`jrsonnet`), and the
Rust-native scripting languages (Lua via `mlua`, Rhai). It rules *out* an
external JavaScript/Python/TypeScript SDK and a bash script, which get no
shared-schema benefit because they cannot reference the Rust wire type.

**The deciding tension is output-shape versus input-shape.** The output is YAML,
which pulls toward Jsonnet, a language purpose-built to template JSON/YAML. But
the entire reason this generator exists is imperative I/O: run `go list`, scan
the tree, exec subprocesses, read per-directory metadata. That is exactly what
Jsonnet is designed to *forbid*; exposing `exec`/`glob` to it fights the
language's purity model. Starlark also forbids I/O by default, but its model is
"the host exposes builtins, the script calls them and loops over the results,"
which fits "call `go_packages()`, branch on what you find, emit targets"
naturally. The YAML-output pull is weaker than it first looks, because the
script does not hand-write the data; it computes the data from live facts, and
computing-from-live-facts is the Starlark shape.

**Determinism by construction is the property `giant gen --check` needs most.**
Starlark has no unbounded `while`, bounded recursion, and no ambient
nondeterminism; the host is the only source of impurity, so determinism is
enforced where we control it. JavaScript is actively hostile here (`Date`,
`Math.random`, iteration-order surprises, async, unbounded loops that can hang
generation), which is disqualifying for a tool whose output must be
reproducible and diff-gated.

**Familiarity in this exact niche.** Starlark is the lingua franca of build
authoring (Bazel, Buck2); shops in every source language already author builds
in it. It is Python-shaped, so "familiar to most programmers" holds without an
external Python runtime. This is also the property that answers ADR-0027 §3's
concern (below): Starlark is a neutral authoring language, not the engine's
implementation language, so choosing it does not force a Go shop to customize in
Rust.

**Embedding is solved.** `starlark-rust` is battle-tested at Buck2 scale, which
de-risks generation at large-monorepo scale, and the result is a single static
binary with no runtime to provision (unlike a Python/TS SDK, which reintroduces
the runtime-provisioning problem ADR-0028's ambient-environment model just
removed).

Two honest caveats recorded so the ADR is not overselling. First, Starlark is
dynamically typed, so a misspelled field is a *runtime* error at the host's
`target()` boundary, not a compile error; the shared-schema win is "one schema
definition, validated at one boundary," not "typos fail to compile." Second,
Jsonnet has a genuine pull as a YAML/JSON templating language (see
Alternatives); we weigh that against a permanent I/O-model mismatch and choose
Starlark regardless.

### 3. Amending ADR-0027 §3: the blessed path is neutral-language, SDKs stay additive

ADR-0027 §3 argued a generator should be written in its ecosystem's language so
a Go shop extends `giant-gen-go` in Go. This ADR amends that for the blessed
generator: a Go shop drives Go-target generation by writing **Starlark**, not
Go. The cost is real and stated: the ecosystem-native authoring property is no
longer the blessed path. Three things make the trade worth it. The concern
ADR-0027 raised was forcing users into the *engine's* language (Rust); Starlark
is not Rust, it is a purpose-built neutral authoring language that Go, Java, C++,
Python, and Rust shops all already use for builds, so the concern does not bind.
The per-language SDK alternative is N libraries that must each track the schema,
the exact maintenance surface consolidation removes. And the evidence is that
ecosystem-native extension did not happen in practice (customization arrived as
YAML knobs, not as forked Go), so the property being traded away was largely
theoretical. The language SDKs are **not foreclosed**: the contract is the
shared wire type, anything that emits it is a valid generator, and a compiled
Go/Rust generator against `giant-schema` remains supported. It is simply no
longer the blessed on-ramp.

### 4. Host the Starlark interpreter in Rust, in this workspace, embedded in `giant gen`

The host is built in Rust and lives in the giant workspace, embedded directly in
the `giant gen` runner (`crates/giant-gen`) rather than as a separate binary.
`giant gen` finds `giant.star` by convention at the root and runs it; it can
still invoke external generator commands, so the "generators are external tools"
path is preserved and the Starlark host is just the built-in one. A
`generate:`-in-root escape (a list of generator commands in root `giant.yaml`)
covers repos that want multiple or external generators; the default entry runs
the built-in host on `giant.star`.

Co-locating the host here does **not** violate ADR-0024's "the engine has no
part in generation," because that boundary is a property of the dependency
graph, not of repo walls. The rule is one-way: `giant-schema <- engine` and
`giant-schema <- host`, with the engine binary never depending on the host crate
and still reading only static JSON. Classification keeps this consistent rather
than ad-hoc: the host is generation *infrastructure* (a peer to the `giant-gen`
runner), so it lives here and counts as porcelain, never engine code; the
generators it runs (`go.star`, `docker.star`, cgo-cross logic, user scripts)
are *content* and live with each workspace.

### 5. Prerequisite: carve out a `giant-schema` crate holding the wire `Target`

Sharing one schema requires one type. Today `TargetSpec`
(`crates/giant/src/model.rs`) conflates three concerns: serde-visible wire
fields (several `pub(crate)`, so an external crate cannot even set them),
loader-resolved fields (`#[serde(skip)]`, e.g. `id`, resolved output paths), and
runtime-only fields (`inferred_deps`, `prune_dirs`). An external generator cannot
target this type. The dependency closure of the wire fields is clean
(`serde` + `glob` + `std`, no `cache`/`executor`/`graph`), so the carve-out does
not drag the engine along, but the type must be split first.

We extract a new workspace-internal crate `giant-schema` (deps `serde` + `glob`)
holding a pure `WireTarget` whose public fields are exactly the serialized form,
plus `Input`, `GlobPattern`, and the `{ targets: [...] }` document wrapper. The
loader (`crates/giant/src/config.rs`) deserializes `WireTarget` and constructs
the internal resolved `TargetSpec` from it; the internal type keeps its resolved
and runtime fields and sheds its serde annotations. Both engine and host depend
on `giant-schema`; neither depends on the other. This is healthy on its own: it
makes ADR-0007's "JSON is the contract" a first-class typed artifact rather than
serde annotations smeared across a hybrid struct, aligned to TDD-0001. Round-trip
must stay byte-identical (watch `remote_cache`'s `default_true`, the
`outputs`/`cwd` renames), proven by tests.

### 6. `giant-gen.yaml` goes away; the host owns all I/O for determinism

The policy file's reason to exist was the knobs; with logic in `giant.star`, it
is removed. Root `giant.yaml` still carries workspace config (cache, remote,
routing, tasks/services) and hand-written root targets (toolchain identity,
codegen, DB-migration targets); `giant.star` plus its `load()`-ed libraries is
the generator program; the generated `giant.*.yaml` is the checked-in static
output. The host owns every impure call (`exec`, `glob`, `read`, `go list`), so
normalization (stable iteration order, sorted package and platform loops,
snapshotted exec output) lives in one place and `giant gen --check` is reliable.

## Consequences

### Enables

- Every would-be knob (skip a package, build-only, per-rule output paths,
  platform matrices, glob-scoped test rules) becomes ordinary code, and the
  cgo-cross matrix becomes a library function. No new schema fields, ever, for
  cases like these.
- One blessed on-ramp for programmable generation, single-binary, with no
  runtime to provision and no per-language SDK to install.
- One schema definition shared by producer and consumer via `giant-schema`,
  so the wire `Target` cannot silently diverge between generator and engine.
- The carve-out is independently valuable: the wire contract becomes a typed
  artifact instead of serde annotations on a hybrid struct.

### Costs

- **A versioned authoring API.** The `ws` primitives, `target()`, and the
  bundled stdlib are now a contract users write against, harder to evolve than
  a closed knob schema. The TDD must declare what is stable and what may change.
- **The ecosystem-native authoring property is traded away for the blessed
  path** (decision 3): a Go shop authors generation in Starlark, not Go.
  Mitigated by Starlark's cross-ecosystem familiarity and by keeping language
  SDKs additive and unforeclosed.
- **A sizeable new dependency.** `starlark-rust` pulls a real dependency tree;
  per the project's dependency gate, the implementing changeset must record the
  binary-size delta and confirm it lands in the porcelain, not the engine.
- **Maintaining the host plus a Starlark stdlib** (`go.star`, `docker.star`),
  where before there was a Go helper library. The fate of the existing Go
  generators is decided by the migration experiment, not pre-decided here.

### What we are committing to maintaining

- `giant-schema` as the single wire-contract crate, byte-identical to today's
  serialization, aligned to TDD-0001.
- The embedded Starlark host in `crates/giant-gen`: the `ws` primitive surface,
  the `target()` constructor over `giant-schema`, the runner contract
  (`GIANT_GEN_OUT`, `--check`, group-by-package, deterministic emit, prune), and
  the bundled stdlib.
- `giant.star` (convention) plus a `generate:`-in-root escape as the discovery
  mechanism.

## Alternatives considered

### Keep adding knobs to `giant-gen.yaml`

The status quo. Rejected by trajectory: the knob set is effectively unbounded
(the floor cases already want several; the cgo-cross ceiling would want a dozen
more), and each knob re-implements a slice of a programming language in a
declarative schema. ADR-0027 §6 already named "needs real logic" as the signal
to stop; this is that signal for the common case.

### Per-language SDKs (ADR-0027's stance), no blessed dynamic host

Ship a Go SDK, a Rust SDK, and so on, and let shops write compiled generators.
Rejected as the *primary* path (kept as additive, decision 3): N libraries each
track the schema, runtime provisioning returns for the non-Rust ones, and the
evidence shows customization arrived as YAML knobs rather than forked SDK code,
so the ecosystem-native property was largely unexercised.

### Jsonnet (`jrsonnet`)

The strongest runner-up, and tempting because the output is YAML and Jsonnet is
purpose-built to template it. Rejected because the *input* is imperative I/O
(`go list`, filesystem scans, subprocesses), which fights Jsonnet's purity
model; bolting `exec`/`glob` onto a pure data-templating language is against its
grain, where Starlark's "host exposes builtins you call and loop over" fits it
naturally. We accept a less data-native output story to avoid a permanent
input-model mismatch.

### JavaScript (V8/QuickJS) or another full scripting language

Ubiquitous and JSON-native. Rejected on three counts at once: non-deterministic
by default (`Date`, `Math.random`, ordering, async), which is hostile to
`--check`; unbounded loops that can hang generation; and a heavy or immature
embed (V8 is large, QuickJS less battle-tested for this). Wrong tool for a
reproducible, diff-gated artifact.

### A Rust-native scripting language (Lua via `mlua`, Rhai)

Tiny, fast, easy host-function binding, and embeds cleanly. Rejected for lack of
the familiarity win: nobody authors builds in Lua or Rhai, so it forfeits
Starlark's "already the lingua franca of this niche" advantage while offering no
compensating benefit.

### CUE

Purpose-built config language with strong constraints that could subsume schema
validation. Rejected on embedding: it is Go-native, so hosting it in a Rust
single binary means shelling out, losing the single-binary and shared-schema
properties that motivate decision 4.

## References

- ADR-0027 - Bundled reference generators and the generator helper library
  (amended here: §3 ecosystem-native authoring, §4 `giant-gen.yaml` policy, §6
  no config language)
- ADR-0026 - `giant-gen` is a thin generator runner, not a framework
  (the runner; external generators stay supported alongside the built-in host)
- [ADR-0024 - Target config is static and generated offline](0024-static-target-config-generated-offline.md)
  (the engine evaluates no language; output stays static and checked-in)
- [ADR-0007 - YAML is sugar, JSON is the contract](0007-yaml-as-sugar-json-internal.md)
  (the wire contract `giant-schema` makes a typed artifact)
- ADR-0021 - Configurable subcommand routing
  (how `giant gen` and external generators resolve)
- [ADR-0010 - Tasks live in porcelain](0010-tasks-live-in-porcelain.md)
  (the host is porcelain, never engine code)
- ADR-0028 - Execution environments
  (ambient environment; the single-binary host avoids reintroducing runtime
  provisioning)
- TDD-0001 - Target model and config schema
  (the spec `WireTarget` aligns to)
- TDD-0022 - giant-gen: the generator runner and staleness gate
  (the runner the host embeds into)
- Prior art: `starlark-rust` and Buck2 (Starlark at scale), `jrsonnet`
  (the Jsonnet runner-up).
