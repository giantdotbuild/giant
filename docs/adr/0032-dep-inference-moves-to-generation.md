# ADR-0032 - Dependency inference moves to generation; the engine graph is explicit

- **Status**: Accepted
- **Date**: 2026-06-07

## Context

The engine used to derive dependency edges: at graph-build time
`compute_inferred_edges` builds a global output-path to producer index over
every target, then matches each target's input globs against those output
strings and adds an edge per match. It is the one place the engine derives a
graph edge instead of reading one.

Three things have shifted since that decision:

1. **It blocks scale.** Inference needs the global output index, so the engine
   cannot resolve any one target's deps without holding every target in memory.
   That forecloses parallel/partial loading and a persisted graph index, which a
   100k-target workspace would need (see `local` notes on loading at scale).

2. **The coupling rationale is largely moot.** ADR-0004 existed to let a target
   consume a discovery-emitted output without naming an unstable producer label.
   ADR-0024 made labels path-derived and predictable, so an explicit dep on a
   generated target is no longer tight coupling - you can write the label and it
   is stable.

3. **It is a pure function of declared strings.** Inference never touches the
   filesystem: it pattern-matches declared input-glob strings against declared
   output strings. So the same edges can be computed anywhere that has the target
   set, not only inside the engine.

It also sits against a stated principle - "all inputs are declared" - by being
the lone derived edge in an otherwise declarative graph.

## Decision

**The engine no longer infers edges. The build graph reads explicit `deps:`
only.** `compute_inferred_edges` and the `inferred_deps` plumbing leave the
engine. A cheap O(n) output-uniqueness check stays (two targets declaring the
same output is a config error), decoupled from edge derivation.

**Inference moves to generation, as a workspace-global link pass in giant-gen,**
run after generators emit. The pass reads every config in the workspace - the
generated `giant.<infix>.yaml` files and the hand-written `giant.yaml` files -
builds the global output-to-producer map, and writes resolved deps into the
generated targets it owns. This is the same algorithm ADR-0004 ran in the
engine, relocated offline and run once per regeneration instead of on every
build.

Scope of the pass:

- It **auto-fills deps for generated consumers**, resolving against all
  producers in the workspace (generated and hand-written alike, since it reads
  everything). This preserves cross-generator and generated-consumes-handwritten
  reach - the full reach the engine had - so there is no correctness regression
  from dropping the live pass.
- It does **not** rewrite hand-written files. A hand-written target therefore
  declares its own `deps:`. Predictable labels make that
  straightforward, and small hand-authored workspaces do it trivially.

**Liveness** is the existing generated-config freshness story, nothing new.
Inferred deps are a generated artifact; they go stale under exactly the
conditions the rest of a generated file does (the declared globs/outputs change
when sources change), and `giant gen --check` already gates that. Regenerate on
change (watch mode) plus generation caching keep it fresh and fast. Because the
edge set is a pure function of declared globs/outputs, inference adds no new
class of staleness.

## Consequences

- **Core shrinks.** The inference machinery leaves the engine, advancing the
  small-core goal.
- **Scale unblocked.** The per-build engine path no longer needs the whole graph
  in memory to resolve a target's deps, clearing the way for parallel parse,
  lazy/partial loading, and a persisted graph index.
- **Fully declarative engine graph**, consistent with "all inputs are declared".
- **No reach lost.** Cross-scope edges (cross-generator, and against hand-written
  producers) are recovered by the global link pass.
- **Costs.** Hand-written consumers must list their deps. giant-gen takes on a
  global parse-and-resolve at generation time - offline, amortized, cacheable -
  which is precisely the "parse everything" cost we want off the per-build path.
- Output globs as producer keys were always fragile (they matched only as
  literal strings); concrete outputs are the supported shape for inference, now
  enforced by the generation pass.

## Follow-up (not in this ADR)

- A TDD for the giant-gen global link pass (resolution, where deps are written,
  interaction with `--check` and generation caching).
- Remove engine inference; retain output-uniqueness validation.
- Revisit generator stdlib once the pass exists (intra-run dep tricks may become
  unnecessary).

## Relationship to prior decisions

Supersedes ADR-0004. Builds on ADR-0024 (generation is offline; labels are
path-derived and predictable) and ADR-0026 (a package may hold several
generator-owned config files - the link pass spans them).
