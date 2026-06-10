# giant.star - generator for giant's own build graph (dogfooding).
#
# Derives a release build+install target for every Rust binary in the workspace
# from `cargo metadata`, keyed to the devenv toolchain identity. Replaces the
# hand-written per-crate giant.yaml build targets, so adding a crate can't drift
# out of sync with the build graph (the generators and the link pass).
#
# cargo.star is vendored (`giant gen vendor cargo.star`) so this repo's own
# generation never needs the network. The collection lives in
# github.com/giantdotbuild/giant-std.
load("@std//cargo.star", "cargo_targets")

def generate(ws):
    # Every workspace binary, re-keyed by the toolchain (//:devenv) so a devenv
    # change invalidates them. The root giant.yaml keeps the workspace settings,
    # //:devenv, sandbox config, and tasks; docs-site keeps its npm targets.
    cargo_targets(ws, deps = ["//:devenv"])
