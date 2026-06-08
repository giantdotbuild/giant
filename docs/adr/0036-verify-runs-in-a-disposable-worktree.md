# ADR-0036 - Sandboxed runs execute in a disposable worktree

- **Status**: Accepted
- **Date**: 2026-06-08
- **Amends**: [ADR-0030](0030-optional-sandbox-and-verify.md) (sandbox + verify)

## Context

ADR-0030 chose birdcage (Landlock + seccomp) for the sandbox because, for
*enforcement* - catching a target that reads an undeclared file or reaches the
network - filesystem deny plus a network switch is enough, with no `bwrap` to
provision. birdcage works by allow/deny on real paths: the engine grants the
command read on its declared inputs, read-write on each declared output's
directory, and chdir's into the real workspace.

That model has no isolation. A sandboxed command runs against the live working
tree with write access to its output directories. The failure showed up the
first time `giant verify` ran on this repo:

- `//docs-site:install`'s command is `npm ci`, whose output is declared as the
  marker `node_modules/.package-lock.json`. birdcage therefore granted
  read-write on the real `node_modules/`. `npm ci` deletes `node_modules`, then
  downloads - so it wiped the real `node_modules` and then failed on the
  denied network. An audit destroyed working-tree state.

Three problems, one of them serious: (1) **safety** - a sandboxed run can
mutate or destroy the real tree, which is intolerable for `verify`, billed as a
read-only audit; (2) **hermeticity reality** - this repo's cargo and npm builds
need the registry/network and undeclared files, so they can't pass an audit
regardless; (3) **diagnostics** - failures leaked the tool's own errors
("failed to read .../Cargo.toml", "npm error Exit handler never called!")
rather than naming the undeclared access.

Sandboxing is a nice-to-have in giant, not a core feature. The bar for keeping
it: safe to run, meaningful errors, reasonably fast. Heavy isolation machinery
(overlayfs / mount namespaces) is the very complexity ADR-0030 avoided.

## Decision

**A sandboxed run executes in a disposable worktree of the committed state, not
the live working tree.** birdcage still applies inside the worktree for
enforcement; the worktree gives the safety birdcage cannot.

- Before a sandboxed build or `verify`, create one throwaway worktree (a `jj`
  workspace / `git worktree` of the current commit) under the state dir. All
  selected targets run there, in dependency order, each birdcage-sandboxed per
  its `sandbox:` setting. The real tree is never an output target's writable
  path, so a destructive command can damage only the throwaway.
- **Outputs.** For a kept build (`giant build --sandbox`), declared outputs are
  read out of the worktree into the CAS and restored to the real tree, exactly
  like a cache restore. For `verify`, outputs are discarded - the audit only
  cares whether the target built without undeclared access.
- **Teardown.** The worktree is removed when the run ends (and on crash, it is
  orphaned state under the state dir that a later run can reap).
- One worktree per run, not per target, so dependency outputs produced earlier
  are present for later targets, and the cost is paid once.

### Gitignored state (node_modules, target/, .giant)

A worktree contains tracked files only, so `node_modules`, `target/`, and
`.giant/` are absent. That is correct, not a gap:

- A `sandbox: false` target (e.g. `//docs-site:install`, the `//:devenv`
  toolchain probe) runs un-sandboxed *in the worktree*. `npm ci` repopulates
  `node_modules` inside the throwaway with the network it needs; the real tree
  is untouched and the worktree is discarded.
- A sandboxed target that reads gitignored, undeclared state (build reading
  `node_modules` that `install` does not declare as an output) is flagged - the
  audit doing its job. To bring such a target under verify, declare the real
  artifact (have `install` output `node_modules`); otherwise mark it
  `sandbox: false` and verify skips it.

### Diagnostics

When a sandboxed command fails, annotate the failure with the likely cause from
birdcage's signals - an `EACCES`/`ENOENT` on a path outside the declared set
("read undeclared path X") or a blocked socket ("attempted network; denied") -
instead of surfacing only the tool's own error.

## Consequences

- **`verify` is safe to run.** It cannot modify the working tree, by
  construction. The per-target `sandbox: false` guards added as stopgaps
  (`//:devenv`, `//docs-site:install`) remain valid as "this target is
  non-hermetic," but they are no longer load-bearing for safety.
- **`verify` audits committed state**, not uncommitted edits. For an audit that
  is the right scope; it matches what CI would build. A dirty working copy is
  not verified until committed.
- **Cost** is one worktree per sandboxed run. A worktree shares the object
  store and skips gitignored trees (no `node_modules`/`target` copy), so it is
  cheaper than a filesystem copy and far simpler than overlay/mount-ns. `verify`
  and `--sandbox` are occasional, not hot paths.
- **This repo gains little from verify** - its cargo/npm builds are not
  hermetic, so most targets stay `sandbox: false` or fail-clean. verify earns
  its keep on hermetic (vendored, fully-declared) builds. That is an honest
  limitation, not a regression.
- ADR-0030's birdcage enforcement stands; this changes only *where* the
  enforced command runs.

## Alternatives considered

- **Full filesystem copy per run.** Safe and simple but copies everything,
  including large gitignored trees unless filtered; a worktree gets the same
  safety while sharing the object store and excluding gitignored paths.
- **Overlayfs / tmpfs upper in a mount namespace.** Fastest (copy-on-write, no
  checkout) and safe, but reintroduces the bwrap-style mechanism ADR-0030
  avoided - platform-quirky and the part that makes sandboxing error-prone. Not
  worth it for a nice-to-have.
- **Keep the live-tree model, add guards + diagnostics only.** Rejected: a
  destructive command with a declared output could still damage the tree; the
  safety has to be structural, not a discipline.
