# ADR-0007 - YAML is sugar; JSON is the internal contract

- **Status**: Accepted
- **Date**: 2026-05-19

## Context

Now that the engine has no embedded scripting language
([ADR-0001](0001-discovery-as-a-target.md)), we need to decide what
config format it actually consumes. The choices: YAML, JSON, TOML, or
a structured schema language (CUE, Dhall). Discovery scripts also
need a format to emit.

## Decision

The engine reads JSON natively. YAML is supported as a thin sugar layer
that parses to the same internal representation. Discovery outputs
(JSON files written by `include:` targets) are the same schema.

`giant.yaml` and `giant.json` are equivalent. A project can use either.
Discovery output is always JSON.

The internal contract - the JSON Schema - is the authoritative
specification.

## Consequences

### Enables

- One schema definition, one parser path. YAML support is ~30 LOC of
  conversion.
- JSON Schema can be published, used for editor validation, used by
  discovery script authors to generate output.
- Discovery scripts can output JSON without going through YAML
  (faster, fewer escaping issues).
- Tooling (`giant validate`, `giant schema`) becomes trivial.

### Costs

- YAML-only features (anchors, aliases, multi-document streams) are
  not supported beyond what YAML→JSON conversion preserves.
- Users who want YAML's brevity (no quotes on string keys) get it; users
  who want JSON's strictness get it. Choice is now visible.

### What we're committing to maintaining

- A versioned JSON Schema for `giant.json` and discovery output.
- YAML→JSON conversion behavior stays standards-compliant.
- Error reporting must point at the original file (YAML line numbers
  if YAML, JSON line numbers if JSON), not at intermediate
  representation.

## Alternatives considered

### YAML as primary, internal model is "config struct"

Skip JSON entirely; YAML is the format, internal representation is just
parsed Rust structs.

Rejected: discovery scripts need a format to emit. YAML is annoying to
emit precisely from arbitrary scripts (indentation, quoting, escapes).
JSON is universal and trivially emitted.

### TOML

Cleaner than YAML, popular in Rust ecosystem.

Rejected: nested arrays of objects (what targets are) are awkward in
TOML. YAML and JSON express this naturally; TOML doesn't.

### CUE / Dhall as authoritative schema

Use a more rigorous schema language. Generate JSON Schema from it.

Rejected: extra toolchain dependency for users; JSON Schema is good
enough and universally tooled.

## References

- [JSON Schema](https://json-schema.org/)
- [TDD-0001 - Target model and config schema](../tdd/0001-target-model-and-config-schema.md)
