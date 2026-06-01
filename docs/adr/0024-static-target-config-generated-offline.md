# ADR-0024 - Target config is static and generated offline

- **Status**: Accepted
- **Date**: 2026-06-02
- **Deciders**: Mr 9k
- **Supersedes**: [ADR-0001](0001-discovery-as-a-target.md).
- **Retires**: [ADR-0017](0017-discovery-accepts-declared-inputs.md) (and
  with it the already-superseded ADR-0013), plus the dynamic-bootstrap
  portions of TDD-0003 and TDD-0015.
- **Reopens**: [ADR-0018](0018-structural-inputs-stay-first-class.md) -
  structural inputs lose their primary consumer; retiring `kind:
  structural` is deferred to its own ADR.

## Context

[ADR-0001](0001-discovery-as-a-target.md) made dynamic config a build
target: `include:` entries are commands the engine runs in a bootstrap
pass before the main graph, and their JSON output is merged in. It was
the right call against the alternative on the table at the time -
embedding a scripting language - because it kept a language runtime and
its input-tracking out of the engine.

But it still pulled a sizeable, stateful subsystem *into* the engine:
the discovery worklist and its generation/chain-depth handling, the
recorded-reads cache that keeps discovery cache keys correct
(ADR-0013), the declared-inputs handling for discovery (ADR-0017), and
the fsmonitor narrowing that exists largely to make discovery
re-verification cheap. This is the most intricate, most stateful code
in the core, and it runs on the hot path of every cold build. A lean
engine is an explicit goal of the project; this subsystem is the single
biggest thing in the core that a small build engine arguably should not
contain.

Two things make removing it viable now.

**The engine has no language to lose.** A config language (Starlark,
Jsonnet) earns its place in a build system by expressing *rules and
macros*: functions that compute a command line from attributes, expand
one declaration into many, and propagate typed information along the
dependency graph. Giant declined that entire axis. The `command:`
string is the rule, written inline. Dependencies come from output-based
inference (ADR-0004), not typed providers. Repetition is meant to come
from generation, not in-config functions. So the work a language would
do does not exist here, and nothing about reading static config gives
anything up.

**Discovery was never the language's job anyway.** Even in a system
with a full config language, target *discovery* is done by an external
generator that scans sources and writes checked-in package files -
Bazel's Starlark expresses rules and macros, but discovery is Gazelle, a
separate tool that generates `BUILD` files you check in, gated in CI
against staleness. The precedent for "discovery is an offline generator
producing static, checked-in config" already exists in the system whose
language would supposedly make it unnecessary. It does not, because the
two concerns are orthogonal.

This ADR takes that split as the model: the engine reads static target
config; *producing* that config - by filesystem discovery, by matrix
expansion, by anything - happens outside the engine.

## Decision

1. **The engine reads only static target config.** It does not run
   discovery, does not execute `include:` targets, and does not expand
   matrices or macros. The target graph is the merge of the static
   `giant.yaml` / `giant.json` files in the workspace. The bootstrap
   pass is removed.

2. **Config is found by scanning, not by an import tree.** The engine
   collects every `giant.yaml` / `giant.json` in the workspace and
   merges them into one graph. Each file defines the targets of its
   *package* - the directory it sits in - exactly as a package file's
   presence defines a package in established monorepo tools. There is no
   `include:` / `import:` registry of files to read; a file's presence
   in the tree is what puts its package in the graph.

3. **Generation is a porcelain.** Discovery, matrix/platform fan-out,
   and any other target multiplication are done by a generator that
   writes `giant.yaml` files, which are checked in like source. The
   generator is an ordinary tool on PATH (`giant-gen`, or a `giant-task`
   task) - the engine has no part in it. A `giant gen --check` mode
   regenerates into a scratch area and fails if the result differs from
   what is checked in, so CI catches drift.

4. **Generation covers matrices, not just discovery.** "Build for
   `{arm,x86} × {mac,linux}`, minus some combinations" is target
   multiplication, which is generation. It is authored compactly in
   whatever language the generator is written in (Jsonnet, CUE, Python,
   shell) and emitted as expanded static config. The engine never
   expands a matrix, and the schema gains no matrix construct. (See
   *Alternatives* for why the tempting bounded-`matrix:`-field is left
   out.)

### How the scan works

In a git workspace the file set is `git ls-files` filtered to the two
config basenames - no filesystem walk, and `.gitignore` exclusions come
for free. Outside git, a single pruned walk honouring the same excludes
as the watcher (`.git`, the state dir, the cache dir). The root
`giant.yaml` is mandatory: it marks the workspace root - what `//`
resolves against and where the scan begins - and is the only file that
may carry workspace-global settings. That includes `cache`, `remote`,
`routing`, and the porcelain-reserved fields the engine does not
interpret but passes over (`tasks:`, `services:`, read by `giant-task`).
Nested files carry targets only.

Target identity is path-derived: a target in `src/go/server/giant.yaml`
is `//src/go/server:name`. Names are unique within a package (one file),
so cross-package collisions are structurally impossible; a duplicate name
within a file is a load-time validation error. The label scheme and the
path-reference rules that follow from it are spelled out under
*Namespace* below.

The merged graph is content-addressed: keyed by the sorted file list and
each file's content hash. A warm run with no config change is a hash
check and a cache hit, not a re-parse, so the scan is not paid on warm
startup. The cheap change-detection primitives the engine already has
- git status, the fsmonitor client, content hashing - are reused to
decide *whether* the set or its contents changed. What is deleted is the
dynamic-*execution* machinery: the bootstrap worklist, the recorded-reads
verifier, and per-target `force_fresh`.

The always-on config watcher (TDD-0014) broadens from "the root
`giant.yaml` changed" to "any tracked config file was added, removed, or
edited," re-running the scan + merge and re-emitting the catalog. Live
reload in `giant session` / `giant tui` keeps working unchanged from the
client's point of view.

### Namespace: path-derived labels

Target identity is **derived from the defining file's location**, not a
free-form semantic string. `src/go/server/giant.yaml` defines the package
`//src/go/server`; a target in it is `//src/go/server:server`, with
`//src/go/server` as shorthand for the target whose name matches the last
path segment, and `:other` as the same-package shorthand. Names are local
to a package, so there is no global uniqueness burden and no cross-package
collision.

This matches what every comparable polyglot build tool does (Bazel,
Buck2, Pants), so it is what users coming from them expect. More
importantly it is *cohesive with the scan*: once config is found by its
presence in the tree, a file's location is already meaningful, and
deriving identity from it reflects real structure instead of leaving
placement semantically inert. Generating files into specific directories
only sharpens that.

**Classification moves to tags, not the label.** What a `lang:kind:name`
string used to encode - language, kind, role - lives in `tags:` and the
existing `test:` flag. Selection then splits cleanly: by *location* via
path patterns (`//src/go/...`), and by *role* via tags (`giant test`,
`--tag kind=bin`). This is the Bazel split - label for identity, tags for
selection - and the machinery is already half-present in giant. It ripples
into [TDD-0011](../tdd/0011-target-selection.md): selection gains path
patterns and leans on tags for cross-cutting picks.

**Path references are package-relative; `//` means workspace root.** Every
path in a `giant.yaml` - `inputs:`, `outputs:`, `cwd:`, and the file
references that drive output-based inference - resolves relative to the
package by default. A bare `c.lib` in `a/b/giant.yaml` is `a/b/c.lib`;
there is no "looks rooted" heuristic, so a multi-segment bare path is
still package-relative. A leading `//` anchors to the workspace root
(`//gen/proto/user.pb.go`), the same `//` that roots a label. `cwd:`
defaults to the package directory. Globs are package-scoped and stop at a
subpackage boundary (a nested `giant.yaml`), so no two packages ever claim
the same file. Cross-package files are reached by depending on the target
that owns them, or by an explicit `//` reference - never by `../`
traversal. `outputs:` follow the same rule: package-relative by default
(`server` in `src/go/server/` writes `src/go/server/server`), with `//`
for a workspace-level artifact (`//bin/server`). Root-anchored outputs are
expected to be common - a top-level `bin/`, `dist/`, or generated-source
tree - so the `//` form is first-class, not an escape hatch.
[TDD-0001](../tdd/0001-target-model-and-config-schema.md) owns the exact
canonicalisation.

**Output-based inference uses the same rule.** Matching happens on the
canonical workspace-relative path after resolution: a producer's output
`c.lib` in `x/y` canonicalises to `x/y/c.lib`; a consumer referencing
`//x/y/c.lib`, or a same-package bare path, canonicalises identically and
links. Because globs are package-scoped, glob-driven inference wires only
*same-package* producers; reaching another package's output is explicit
(`//path` or a `dep:`). That bounds the implicit linking to a package and
makes every cross-package edge visible in the file - and it stays
decoupled in the ADR-0004 sense, since you name the output *path*, not the
producer's target.

**The refactor cost, and why it is bearable.** Path-derived identity means
moving a directory renames its targets, and dependents must update - the
one real downside of this scheme (name coordination and collision
machinery vanish). Two things make it bearable. Generation owns the
cross-references, so a move is a regenerate that fixes both sides, leaving
only hand-written references to touch. And the label doubles as a file
locator, which keeps real *partial* graph loading natively open if a
workspace ever outgrows whole-graph loading - no separate index to build,
the thing a global-ID scheme would have needed.

## Consequences

### Enables

- The discovery-bootstrap subsystem leaves the core: the worklist, the
  recorded-reads cache, the discovery cache-key handling, and the
  fsmonitor narrowing that served it. The engine's most stateful code
  path is gone, and cold builds no longer fork a discovery pass.
- The target graph is transparent and reviewable: every target is a
  line in a checked-in file, diffable in a PR. "What will giant build?"
  is answered by reading the tree, not by running a tool and inspecting
  its JSON.
- Startup is deterministic and cheap - parse + merge of static files,
  cached by content hash. The warm-validation and no-op-build budgets
  get easier to hold, with no fork+exec or sidecar logic at build time.
- Matrices, macros, and discovery collapse into one mechanism
  (generation), so there is one story for "where do repetitive targets
  come from," not a special engine feature per case.
- Users keep full generative power by choosing a generator language and
  running it offline. Nothing is locked to one runtime, and the runtime
  is never linked into the engine binary.

### Costs

- **Drift.** Checked-in generated config goes stale the moment a source
  file is added without regenerating; the build then silently omits it.
  This is the one real property the previous model had that this one
  does not (it re-derived the graph every build). It is recovered, not
  eliminated, by the `giant gen --check` CI gate, and by a local
  papercut: "did you regenerate?"
- **A more verbose tree.** A 4-way matrix is checked in as its expanded
  targets, not a compact call. This is more lines, but they are honest
  and diffable; the compact source of truth is the generator input.
- **Path identity churns on directory moves.** Moving a package renames
  its targets, and dependents must update. Softened by generation (a move
  is a regenerate that fixes cross-references) but real for hand-written
  references and visible as a large diff on big moves. See *Namespace*.
- **Selection reworks.** Role-based selection moves from `lang:kind:name`
  ID globs to path patterns plus tags. The machinery is half-present
  (`tags:`, the `test:` flag), but TDD-0011 and the selection UX need a
  real pass.
- **Output-scanning discovery loses engine-sequenced ordering.** A
  generator that emits targets by reading *generated* files (rather than
  source) needs those files present when it runs; the engine no longer
  sequences "build codegen, then discover from its outputs" via `deps:`.
  This is narrow: a generator that reads sources and encodes the
  codegen's output convention avoids it entirely, generated code is
  frequently checked in anyway, and source-scanning is the norm.
  Reading build outputs to discover targets is the case that gets
  harder, and it is one worth discouraging.

### What we commit to maintaining

- The scan + merge rules: the two config basenames, the git/walk file
  source, the file → package mapping, and the within-package
  duplicate-name error.
- The path-resolution rule - package-relative by default, `//` for
  workspace root, globs stopping at subpackage boundaries - and the
  canonical workspace-relative form that output-based inference matches on.
- The content-hash freshness path that keeps warm startup a cache hit,
  and the watcher trigger set that drives live reload.
- The static target schema (additive evolution) as the sole contract
  between generators and the engine. Generators target this schema; the
  engine never knows a generator existed.

## Alternatives considered

### An explicit `include:` / `import:` tree

A root file lists the config files to read, possibly recursively.
Rejected: it does not scale. Every new package needs a registration
edit, recursive imports become a graph to reason about, and a glob
import (`import: ["**/giant.yaml"]`) is the scan with extra syntax and
worse failure modes. Presence-based discovery of config files is what
established monorepo tools converged on for this reason.

### Keep discovery as a startup target (status quo, ADR-0001)

Rejected here. It is correct and drift-free, but it is the largest
non-lean subsystem in the core and runs on every cold build. The
project values a small engine over avoiding the drift gate, and the
drift gate is a well-trodden, cheap mitigation.

### Embed a templating language in the engine

A pure, deterministic templating language (Jsonnet-class) does not have
the cache-correctness problem a filesystem-reading scripting language
has, so it is cheaper to embed than ADR-0001 feared. Still rejected: it
is a runtime and a second config surface inside a binary whose whole
pitch is leanness, it adds a format against ADR-0007, and users can run
exactly that language *as a generator* for free. The power is available;
it just lives outside the engine.

### A bounded declarative `matrix:` field in the schema

A non-language construct - `axes` + `exclude` + interpolation - that the
engine expands at load. Tempting, because hand-running a generator for a
small matrix feels heavier than a few lines of YAML. Rejected on the
hard line: the moment expansion lives in the schema it grows - `exclude`
invites conditionals, conditionals invite computed values, and the
schema reinvents a language one field at a time. It is the anti-pattern
the project's own guardrails warn against. If hand-authoring generators
proves genuinely painful for matrices, this can be added later,
additively, with eyes open.

### Semantic global IDs instead of path-derived labels

Keep free-form IDs (`go:bin:server`) unique across the whole workspace,
role baked into the ID, selection by ID glob. Rejected. Its real
advantage - refactor stability, since a move doesn't rename a target - is
largely recovered by generation, which regenerates cross-references. Its
costs fit this design worse: a global-uniqueness burden with
collision-detection machinery, a file's location meaning nothing despite
being deliberately placed, a selection model that diverges from what
monorepo users expect, and partial loading that would need a separate
ID→file index instead of falling out of the label. Once config became
scanned-and-generated, path-derived labels were the cohesive choice.

## Structural inputs: the primary consumer leaves

[ADR-0018](0018-structural-inputs-stay-first-class.md) kept `kind:
structural` as a first-class input kind, but conceded the point its
predecessor [ADR-0014](0014-structural-inputs-discovery-only.md) pressed:
the only practical consumer is discovery. Targets that *transform* source
- compile, lint, test, codegen - depend on full file content, not a
line-pattern slice. Moving discovery out of the engine therefore removes
structural inputs' flagship consumer. Two pieces are affected
differently:

- The discovery **excerpt verifier** (line patterns inside the
  recorded-reads protocol) is deleted with the rest of the discovery
  subsystem. No loss - it was discovery-internal.
- `kind: structural` as a target input kind keeps working mechanically
  (it is orthogonal to how targets are declared), but now serves only the
  narrow niche of API/signature-sensitive targets that ADR-0014 argued is
  nearly empty.

The consistent resolution is the same as for discovery itself: the
*technique* relocates to generators. A generator that wants to skip
regeneration when only function bodies changed implements
git-as-change-oracle plus line-pattern fingerprinting itself. That would
let the engine shed `structural.rs`'s fast path, the `structural_inputs`
cache-key section, and the `Input::Structural` variant - a further lean
win.

That last step **reverses ADR-0018**, so it is not folded into this
decision silently: retiring `kind: structural` from the engine is a
flagged consequence to be settled in its own ADR once this one lands.
What ADR-0024 commits to is the honest framing - structural inputs lose
their reason to be an *engine* feature when discovery leaves - not the
removal itself.

## What stays the same

- **Output-based dep inference (ADR-0004)** still links targets by output
  path without coupling to producer IDs. Its *resolution* now follows the
  package-relative / `//` rule and its glob-driven form is package-scoped
  (see *Namespace*), but the mechanism and the decoupling are unchanged.
- **YAML is sugar, JSON is the contract (ADR-0007)**: still one schema,
  two surface syntaxes. Generators may emit either.
- **Live reload (TDD-0014)** and the event protocol (ADR-0003) keep
  their shape; only the reload trigger set broadens.

## References

- [ADR-0001 - Discovery is a target](0001-discovery-as-a-target.md)
  (superseded by this ADR)
- [ADR-0004 - Output-based dep inference stays](0004-output-based-dep-inference-stays.md)
- [ADR-0007 - YAML as sugar, JSON internal](0007-yaml-as-sugar-json-internal.md)
- [ADR-0013 - Discovery cache key and recorded reads](0013-discovery-cache-key-and-recorded-reads.md)
  (retired by this ADR)
- [ADR-0017 - Discovery accepts declared inputs](0017-discovery-accepts-declared-inputs.md)
  (retired by this ADR)
- [ADR-0018 - Structural inputs remain first-class](0018-structural-inputs-stay-first-class.md)
  (reopened - see *Structural inputs* above)
- [TDD-0003 - Discovery bootstrap and merge](../tdd/0003-discovery-bootstrap-and-merge.md)
- [TDD-0014 - Engine session mode](../tdd/0014-engine-session-mode.md)
