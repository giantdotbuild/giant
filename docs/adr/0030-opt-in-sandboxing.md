# ADR-0030 - Opt-in sandboxing for enforcement

- **Status**: Accepted
- **Date**: 2026-06-06
- **Deciders**: Mr 9k

## Context

Isolation, if it returns, is **opt-in** and shaped as "a wrapper command the user
prepends (`bwrap ...`) or a separate `giant-sandbox` shim invoked the same way,
not a core feature." It removed the dead `sandbox: bool` hashing bit and parked
the rest. We now want the real thing, with a concrete mechanism and a clear
story for how it interacts with the cache.

Goals:

- **Opt-in.** Giant keeps working exactly as today; you opt in and get
  sandboxing. Default builds, and non-Linux, are untouched.
- **Linux first, simple, small core.** A genuine sandbox on Linux, with as
  little code in the engine as possible (the LOC budget is real).
- **Assumes no toolchain manager.** Core grants a generic FHS base; a workspace
  declares any scheme-specific extras (`sandbox.roots`/`sandbox.env`) as data.
  Nix/devenv is then just one configuration - bind `/nix/store` read-only and
  the toolchain is pinned by store path (ADR-0028's "identity") - but asdf
  (`~/.asdf`), an LFS/vendored `bin/`, or a plain-distro `/usr` toolchain are
  equally first-class. Giant never bakes in a particular manager.
- **Generalises to other toolchain schemes** - asdf, LFS-tracked binaries,
  repo-local toolchains - by adding config, never by reworking the core.

The forcing questions this ADR answers: **where does sandboxing live** (core,
generator, or porcelain), and **how does it interact with the cache** - because
the obvious "let the generator emit `bwrap ...` into the command" turns out to
be wrong for caching, and that drives the whole design.

## Decision

### 1. Sandboxing is *enforcement*, not a logical input - it wraps at exec time and stays out of the cache key

A target's cache key is computed from its **bare** command (plus inputs, env,
deps - TDD-0009). The sandbox is applied by **wrapping the command at execution
time**, and the wrapper is **not** part of the key. So a sandboxed run and an
unsandboxed run of the same target share the same cache key and the same cache
entries.

This is the load-bearing decision, and the reasoning is: for a correctly
declared (hermetic) target, the output is identical whether or not it ran in a
sandbox - the sandbox only *enforces* the declared inputs/outputs, it does not
change them. The only targets whose output legitimately differs sandboxed
vs not are **mis-declared** ones (they read an undeclared file, depend on a
scrubbed env var, or reach the network), and those are precisely the bugs the
sandbox exists to surface - not a distinction worth freezing into the key.

Putting the sandbox *in* the command (generator-emitted, or hand-written) would
fold it into the command hash, which would:

- **bust the entire cache** the moment sandboxing is enabled (every key
  changes), and
- **prevent a sandboxed CI from sharing cache with unsandboxed dev** (different
  keys forever).

Keeping the key transparent is what lets sandboxing be a runtime choice with no
cache penalty, and what lets a sandboxed CI and an unsandboxed dev share
artifacts. A consequence (see §5): with a transparent key, sandboxing only
*runs* on cache **misses** - but since the key turns over whenever inputs
change, that is exactly when re-enforcement is wanted.

### 2. Thin policy in the core; the mechanism is a `giant-sandbox` porcelain

- **Core (small).** The opt-in is a **mode**, not a per-target field: a
  `--sandbox` flag (and `giant verify`), Linux-gated. Sandboxing is enforcement,
  and a correctly declared target's output is identical sandboxed or not (§1),
  so "should this be sandboxed" is an operator/machine choice (CI on, dev off -
  §5), not a property baked into committed config. When the mode is on, the
  executor sandboxes every **eligible** target; it does **not** spawn
  `<command>` directly. It resolves the bind set (inputs read-only, output dirs
  read-write, toolchain paths read-only, network on/off - all already resolved
  for the cache key), writes it as a schema-versioned `SandboxSpec` manifest (a
  temp file under `.giant/`), and **prepends `giant-sandbox run --spec <file> --`
  to the target's argv**. Everything else the executor does - capturing
  stdout/stderr, timeouts, exit status, events - is unchanged, because
  `giant-sandbox` is transparent in the pipe. So this is a resolve-already-done
  plus write-spec-and-prefix change: no `unsafe`, no namespace syscalls, no
  sandbox-mechanism knowledge in the core.

- **Porcelain (`giant-sandbox`, Linux-only).** Owns the entire mechanism. It
  reads the `SandboxSpec`, applies the restrictions, and **spawns the build
  command itself** - the actual exec of the target's command moves here, because
  the restriction has to wrap the exec (the sandbox library applies its filters
  as it forks the child; you cannot apply them in the parent and hand off to a
  vanilla spawn). stdio is inherited straight through and the child's exit code
  is propagated, so the executor still sees one transparent child. The porcelain
  is also where **future scheme support** lives: new toolchain schemes ship as
  porcelain changes, never engine releases. This is a third porcelain shape: not
  a dispatched subcommand (`giant gen`) and not an event consumer (`giant tui`),
  but a **per-target exec wrapper** (in the spirit of `env` / `nice`).

Only the executor, at run time, has the resolved bind set and can enforce
"sandbox everything" for an audit, so the *policy* cannot fully leave the core;
but the *mechanism* should, and does.

### 2a. Mechanism: birdcage for v1 (enforcement, not isolation)

The forcing distinction is **enforcement vs isolation**. Giant's goal is to
catch *honest, incomplete declarations* - a target that quietly reads `$HOME`,
a system library outside its toolchain, or the network - not to contain hostile
code. For enforcement, "deny access to anything undeclared" on the filesystem,
plus network on/off, is exactly sufficient. A fully isolated filesystem *view*
with PID and tmpfs namespaces is isolation-grade capability we do not need.

v1 uses **birdcage** as the backend: a pure-Rust, embeddable sandbox (Linux:
seccomp + Landlock + namespaces under the hood; macOS: Seatbelt) scoped to
filesystem read-only / read-write exceptions and network on/off. It fits
because:

- **Pure Rust, no extra runtime binary** - the porcelain stays a single static
  binary with nothing to provision (unlike a `bwrap` runtime dependency).
- **fs + network is precisely the enforcement surface** - reads or writes
  outside the declared set fail; network is deny-by-default with a
  `network: true` exception.
- **The contract is mechanism-agnostic.** Core only ever produces a
  `SandboxSpec`; the porcelain decides how to enforce it. A stronger
  isolation-grade backend (a `bwrap`/`unshare` bind-mount sandbox with a full
  filesystem view, fresh tmpfs, and PID/network namespaces) can be added later
  behind the *same* contract, selected by a porcelain flag, with no core change.

This refines ADR-0008's "prepend `bwrap`" sketch: the same prepend-a-wrapper
shape, but the wrapper is our own porcelain over a Rust sandbox library rather
than an external bind-mount tool. The mechanism is genuinely small either way
(roughly 15-40 lines to translate a `SandboxSpec` into any of these backends);
the choice is about properties, not line count, and birdcage wins on
pure-Rust + the right enforcement surface.

### 3. Toolchain bind paths are data, declared by the toolchain/generator

What a sandbox must expose, beyond a target's own inputs/outputs, is whatever
its **toolchain** needs. That set is **data**, not an emitted `bwrap` line, and
not a built-in assumption about any toolchain manager.

Core grants a generic, read-only-plus-execute **FHS base** - `/usr`, `/bin`,
`/lib`, `/lib64`, `/etc`, filtered to those present - plus a generic env
allowlist (§4). A workspace then declares scheme-specific extras as config:

```yaml
sandbox:
  roots: ["/nix/store", "/run/current-system/sw"]   # ro+exec
  rw:    [".devenv/state/go"]                         # writable; workspace-relative
  env:   ["NIX_*", "DEVENV_*"]                        # names or `prefix*`
```

Path entries in `roots`/`rw` are absolute, `~/`-relative (against `$HOME`), or
**workspace-relative** (anything else, resolved against the workspace root) - so
a committed config is portable across machines, not pinned to an absolute home
path. A Nix/devenv repo adds the store; an asdf repo adds `~/.asdf`; an
LFS/vendored toolchain adds its `bin/`. The engine unions defaults + config into the set
handed to `giant-sandbox`, filtered to existing paths. Enforcement still bites
on the **workspace**: only declared inputs are readable there, on every scheme.
(giant's own repo dogfoods this - its `giant.yaml` carries the Nix block, so the
engine has zero Nix knowledge baked in.) The richer future form is per-toolchain
declaration via ADR-0016 (toolchains are targets) and ADR-0028 (execution
environments), where a toolchain dep contributes its own paths; the global
`sandbox:` block is the v1 of that data model, with no change to the
core/porcelain contract. A generator's role is to *declare* those paths (and
per-target flags like `sandbox`/`network`), never to emit the mechanism.

### 4. What a sandboxed target sees

- **Declared inputs**: read **and execute**. Some inputs are scripts the
  command runs (a wrapper, a codegen helper); execute on a declared input is
  harmless for enforcement and a data file cannot be meaningfully exec'd, so
  inputs get read+execute rather than read-only.
- **Output directories**: read-write.
- **Toolchain paths** (§3): read-only (the FHS base plus configured roots).
- **Extra writable paths** (`sandbox.rw`): for build caches that live outside
  the workspace (a Go `GOCACHE`, a Cargo registry). Read-write, must exist.
- **Standard pseudo-devices** - `/dev/null`, `/dev/zero`, `/dev/full`,
  `/dev/random`, `/dev/urandom`, `/dev/tty` - granted read-write by default.
  birdcage has no synthetic `/dev` (no mount namespace), and nearly every real
  command touches `/dev/null`; these are universal on Linux, not scheme-specific.
- **A writable temp directory** for scratch (granted read-write, pointed at by
  `TMPDIR`; a fresh tmpfs and PID isolation are isolation-grade extras a future
  `bwrap` backend can add, not part of the v1 birdcage surface).
- **Network off by default**, with a per-target **`network: true`** escape for
  the targets that genuinely fetch (sysroot/dependency fetches, image pulls).
- **Env scrubbed to an allowlist.** The command sees only a generic base
  (`PATH`, `HOME`, `TMPDIR`, `LANG`, `LC_*`, `SSL_CERT_FILE`, … - no toolchain
  manager assumed), the workspace's configured `sandbox.env` extras (e.g.
  `NIX_*`), and the target's declared `env:` - everything else (random user/CI
  vars) is dropped. Unlike Bazel's fixed `PATH=/bin:/usr/bin`, giant keeps the
  ambient `PATH` because under a manager like devenv it *is* the toolchain
 The engine resolves the name list (expanding `prefix*` patterns
  against the ambient env, since the sandbox grants exact names);
  `giant-sandbox` grants exactly those (an empty list means "pass the whole
  ambient env", the back-compat default).

### 4a. The two per-target fields (escape and exemption, not opt-in)

The opt-in is the **mode** (§2). The only sandbox-related fields in the wire
schema are *modifiers* of how an eligible target behaves once the mode is on:

- **`network: bool`** (default `false`) - the network escape of §4. `true`
  means this target may reach the network even when sandboxed (it genuinely
  fetches). It belongs in config because "needs the network" is a real property
  of the target, like an input.
- **`sandbox: bool`** (default `true`, i.e. *eligible*) - set `false` to
  **exempt** a target that cannot be sandboxed, so `--sandbox` skips it and runs
  it normally. There is deliberately no meaningful `sandbox: true`: a per-target
  *opt-in* would be the committed-config sandbox this ADR rejects (it would run
  for everyone and break on non-Linux), so `true` is only the explicit default.

Both fields **stay out of the cache key** (like `cache:`, `test:`,
`timeout_secs:`): they change neither the command, the inputs, nor the env, so a
hermetic target's output is identical with or without them (§1). And both are
**inert unless sandbox mode is active** - they always parse on every platform
(configs stay portable), but only act under `--sandbox`, which is Linux-only and
errors rather than degrades (§6). That single rule covers "mode off" and
"non-Linux" without special-casing.

The sandbox targets **hermetic-buildable work** - compile, codegen, test - where
the command's output is a pure function of its declared inputs + toolchain.
**Daemon- or registry-driven targets opt out via `sandbox: false`.** The
canonical case is container image builds: a `docker build`/`push` hands the work
to the **docker daemon** over a socket (the sandboxed process isn't where the
build happens, so a filesystem sandbox enforces nothing) and pulls base layers
over the **network**; daemonless builders (buildah, kaniko) create **their own
user namespaces**, which nest badly inside the sandbox's. Such targets aren't
hermetically enforceable by giant - their reproducibility is the Dockerfile's
job - so they set `sandbox: false` and `verify` skips them while every real
build target stays enforced. (Wrapping only a client, rather than exempting, is
possible with `network: true` plus a `sandbox.rw` entry for the daemon socket,
but for the daemonless builders exemption is the honest call.)

### 5. Operational modes and the cache-trust model

- **`giant build --sandbox`** - transparent key, so it *enforces on cache
  misses* and reuses hits. Re-enforcement rides the key: a target's inputs
  change → new key → miss → sandboxed run → re-verified; unchanged → reuse the
  entry that was produced sandboxed when those inputs last changed. So you get
  enforcement at exactly the moments hermeticity could break, and caching for
  the rest. You do **not** have to run fresh to get meaningful enforcement.

- **`giant verify`** (≈ `--sandbox` with cache bypass) - runs *every* target
  sandboxed, ignoring the cache, to prove each target's declared inputs are
  complete. This is the hermeticity audit ADR-0008 named.

- **Trust via who-writes, not via the key.** Only sandboxed builds should
  **write** the shared (remote) cache. CI builds `--sandbox` with read-write
  access; developers are read-only - they *pull* CI's sandboxed artifacts (fast
  local builds on trusted outputs) but never upload, so their unsandboxed local
  results stay in their local cache and never contaminate the shared one. The
  shared cache is therefore all-sandboxed by construction, and the transparent
  key (§1) is what lets developers reuse CI's entries across machines. This is
  remote-cache access control (read-only vs read-write, ADR-0006), not new
  engine machinery. The one declared hole is `sandbox: false` (§4a): an exempt
  target runs unsandboxed even in a `--sandbox` CI build, so its cache entry is
  not enforcement-verified. That is an explicit, greppable choice rather than a
  silent gap, and the optional provenance flag below is how it becomes checkable.

- **Optional hardening: a `sandboxed` provenance flag on the AC entry** - *not*
  in the key. A sandboxed reader can then refuse entries that were not produced
  sandboxed, and `verify` can assert "every entry I would reuse is sandboxed,"
  guarding against a misconfigured writer. Lead with the who-writes ACL model;
  add the provenance bit only if checkable trust is wanted.

### 6. Opt-in and gating

Off by default - giant behaves exactly as today unless the `--sandbox` mode
(or `giant verify`) is used; the per-target `network:`/`sandbox:` fields (§4a)
do nothing on their own. Gated three ways: a `sandbox` cargo feature, a
`cfg(linux)` check, and a **runtime** check (the sandbox helper is available and
namespaces/unshare actually work). If a build opts in but the environment cannot
sandbox (a non-Linux host, or a distro with unprivileged user namespaces
disabled), it **errors clearly** - it never silently degrades to an unsandboxed
run, because a silent downgrade would defeat the enforcement guarantee.

## Consequences

### Enables

- Hermeticity enforcement and a `verify` audit, while the cache stays shared
  across sandboxed and unsandboxed runs.
- CI can build sandboxed *and* use the cache - re-verification is automatic on
  input change, free reuse otherwise.
- A trust boundary for the shared cache (sandboxed-only contents) with no engine
  feature beyond the remote-cache ACL already implied by ADR-0006.
- Nix/devenv works on day one (`/nix/store` read-only); other toolchain schemes
  slot in as declared bind paths without core changes.

### Costs

- A new porcelain, `giant-sandbox`, to maintain (Linux-only). Mitigated: it is
  where *all* the OS-specific and future-scheme code lives, so the engine never
  grows for it.
- One extra exec per target when sandboxing is on (`giant-sandbox` between the
  executor and the command). Acceptable for an opt-in mode.
- **Undeclared dependencies surface.** Targets that quietly read `$HOME`,
  `/etc`, system libraries outside the toolchain, or the network will fail the
  first time they run sandboxed. That is the feature, but it is a real migration
  cost; the `network:` escape and a minimal base mount soften it.
- The transparent-key choice means a warm-cache `--sandbox` build mostly reuses
  hits and only *runs* isolated on misses; users who expect "isolate everything
  now" must use `verify` (cache bypass).

### What we commit to maintaining

- The core opt-in surface (`--sandbox`, `sandbox:`/`network:` fields) and the
  resolve-and-pass-the-bind-set step.
- The core↔`giant-sandbox` contract (how the bind set and flags are passed).
- `giant-sandbox` itself, and the toolchain-bind-path data model.

## Alternatives considered

### Generator-emitted sandbox (bake `bwrap ...` into the command)

Tempting and core-free - the generator already knows a target's
inputs/outputs/toolchain, so it could emit the wrapped command, and sandboxing
would become "just generation". Rejected on three counts: it is **not
a per-machine opt-in** (the wrapper is baked into committed `giant.*.yaml`, runs
for everyone, and breaks on non-Linux); it **cannot audit-everything at runtime**
(the sandbox is frozen at generation time); and it **pollutes the cache key**
(the wrapper is part of the command hash - §1), busting the cache on enable and
preventing cross-mode sharing. The generator keeps a smaller, correct role:
declaring toolchain bind paths and per-target flags as data (§3).

### Pure porcelain, nothing in the core

Rejected: only the executor, at run time, has the *resolved* bind set and can
enforce "sandbox every target" for an audit. A separate process cannot insert
itself into the per-target exec path unless the core decides to invoke it and
hands it the set. The mechanism leaves the core; the policy cannot.

### Sandbox state in the cache key (separate entries per mode)

Rejected: it busts cross-mode cache sharing (the very thing §1 preserves).
Sandboxing is enforcement, not an input; a hermetic target's output does not
depend on it.

### bwrap bind-mount sandbox as the v1 backend

A bind-mount sandbox via `bwrap`/`unshare` gives a complete hermetic *view* -
the process sees only bound paths, plus a fresh tmpfs and PID/network
namespaces. Stronger than birdcage, and the right bar for containing untrusted
code. Not chosen for v1 because (a) it needs an external `bwrap` binary at
runtime, against the single-static-binary porcelain ethos, and (b) its extra
isolation is isolation-grade capability beyond giant's enforcement goal (§2a).
It remains the obvious second backend behind the same `SandboxSpec` contract for
anyone who wants a full view.

### raw landlock (LSM ruleset) instead of a sandbox library

Pure-Rust and zero-extra-binary like birdcage, but lower-level: filesystem-only
by default, with a weak network story (Landlock governs TCP bind/connect ports
only - no clean "all network off" without adding seccomp), kernel-version and
ABI gated, and more wiring to drive directly. birdcage sits one layer up and
bundles seccomp + Landlock + namespaces behind a small fs+net API, giving the
same zero-binary property with less code and a real network toggle. Raw landlock
stays available as a backend if we ever want to drop the birdcage dependency.

## References

- ADR-0008 - Optional sandbox + verify
  (parked; revived and superseded here - its closing note named the
  `giant-sandbox` shim this ADR specifies)
- ADR-0016 - Toolchains are targets
  (the identity half; toolchain deps contribute sandbox bind paths)
- ADR-0028 - Execution environments
  (availability vs identity; giant runs inside devenv, so PATH and the toolchain
  closure are already in place)
- [ADR-0006 - Remote cache over HTTP](0006-no-bazel-reapi.md)
  (the read-only/read-write access control behind the cache-trust model)
- [ADR-0024 - Target config is static and generated offline](0024-static-target-config-generated-offline.md)
  (why sandboxing is *not* generation)
- TDD-0009 - Executor (cache-key composition; the
  command hash the sandbox must stay out of)
