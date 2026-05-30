#!/usr/bin/env sh
# Emit toolchain targets for giant. See the "Toolchains" guide.
#
# Each tool becomes a `//toolchain/<name>` target whose identity is the
# resolved path of the executable, written to an id file. In a devenv/Nix
# shell that path is a store path, so it moves whenever the toolchain is
# updated - which re-keys every target that depends on it. The engine just
# hashes the string; it knows nothing about Nix.
#
# Wire it up in giant.yaml:
#
#   include:
#     - id: "discover:toolchains"
#       command: "tools/discover-toolchains.sh > .giant/d/toolchains.json"
#       outputs: [".giant/d/toolchains.json"]
#
# Then a target that uses a toolchain depends on it:
#
#   - id: "bin:server"
#     deps: ["//toolchain/rust"]
#     ...
#
# A Node bump moves `//toolchain/node`'s id but not `//toolchain/rust`'s,
# so Rust targets stay cached. Per-ecosystem scoping falls out of the dep
# graph - no special engine support.

set -eu

# name:executable pairs. Edit to match your stack.
TOOLS="rust:rustc go:go node:node"

emit_target() {
    name="$1"
    exe="$2"
    # Identity = the resolved executable path. `command -v` finds it on
    # PATH, `readlink -f` resolves symlinks to the real (store) path. The
    # command writes the literal output path - giant sets no $OUT var.
    printf '{"id":"//toolchain/%s",' "$name"
    printf '"inputs":["devenv.lock","devenv.nix"],'
    printf '"command":"command -v %s | xargs readlink -f > .giant/toolchains/%s.id",' "$exe" "$name"
    printf '"outputs":[".giant/toolchains/%s.id"],' "$name"
    printf '"tags":["toolchain"]}'
}

mkdir -p .giant/toolchains

printf '{"schema_version":1,"targets":['
first=1
for pair in $TOOLS; do
    name="${pair%%:*}"
    exe="${pair##*:}"
    [ "$first" = 1 ] || printf ','
    emit_target "$name" "$exe"
    first=0
done
printf ']}\n'

# --- Checked-in / git-lfs binaries -----------------------------------------
# If a tool lives in the repo at a fixed path (e.g. bin/go tracked by
# git-lfs), the resolved-path trick does NOT work: the path is stable while
# the bytes change, so the identity never moves. Hash the content instead:
#
#   "inputs":  ["bin/go"]
#   "command": "sha256sum bin/go | cut -d' ' -f1 > .giant/toolchains/go.id"
#
# `inputs: ["bin/go"]` re-runs the target only when the binary changes; the
# id file holds the content digest, so dependents re-key on a real update.
