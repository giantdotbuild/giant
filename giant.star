# giant.star - generator for giant's own build graph (dogfooding).
#
# Derives a release build+install target for every Rust binary in the workspace
# from `cargo metadata`, keyed to the devenv toolchain identity. Replaces the
# hand-written per-crate giant.yaml build targets, so adding a crate can't drift
# out of sync with the build graph (the generators and the link pass).
#
# cargo.star is loaded from the in-repo std collection by its repo-local path:
# this repo *is* the std collection's source, so no @std// / GIANT_STD needed.
load("std/cargo.star", "cargo_targets")

def generate(ws):
    # Every workspace binary, re-keyed by the toolchain (//:devenv) so a devenv
    # change invalidates them. The root giant.yaml keeps the workspace settings,
    # //:devenv, sandbox config, and tasks; docs-site keeps its npm targets.
    cargo_targets(ws, deps = ["//:devenv"])
