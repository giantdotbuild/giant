# ADR-0036 - Verify runs in a disposable worktree

- **Status**: Accepted
- **Date**: 2026-06-08
- **Amends**: [ADR-0030](0030-opt-in-sandboxing.md)

> **Revised** 2026-06-09: scoped the worktree to `giant verify` (the audit that
> must be safe on every target). `giant build --sandbox` keeps enforcing against
> the live tree - it is an iterative build of what you are editing. The worktree
> lives in a scratch directory outside the workspace, not under the state dir.

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

**`giant verify` builds in a disposable worktree of the committed state.**
birdcage still applies inside the worktree for enforcement; the worktree gives
the safety birdcage cannot. `giant build --sandbox` keeps enforcing against the
live working tree, where it belongs: it is an iterative build of what you are
editing, and it leaves non-hermetic targets `sandbox: false` so they run
normally. verify is the audit that has to be safe on every target, so verify is
the one that isolates.

- Before a verify run, create one throwaway `git worktree` of `HEAD` (a `jj`
  colocated repo works through its git view) in a scratch directory outside the
  workspace. Every selected target runs there, in dependency order, each
  birdcage-sandboxed per its `sandbox:` setting. The real tree is never a
  writable path, so a destructive command can damage only the throwaway.
- **Outputs are discarded.** The audit only cares whether each target built
  without undeclared access. Build products are captured to the cache as usual,
  but nothing is written back to the real tree.
- **Teardown.** The worktree is removed when the run ends. A crashed run leaves
  a stale checkout that the next verify reaps (`git worktree prune` plus
  removing the scratch path).
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
  construction. The per-target `sandbox: false` guards (`//:devenv`,
  `//docs-site:install`) remain valid as "this target is non-hermetic," but
  safety no longer depends on them.
- **`verify` audits committed state**, not uncommitted edits. For an audit that
  is the right scope; it matches what CI would build. A dirty working copy is
  not verified until committed.
- **Cost** is one worktree per verify run. A worktree shares the object store
  and skips gitignored trees (no `node_modules`/`target` copy), so it is cheaper
  than a filesystem copy and far simpler than overlay/mount-ns. verify is an
  occasional command, not a hot path.
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
  destructive command with a declared output could still damage the tree, so the
  safety has to be structural.
