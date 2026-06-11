# giant.star - generator for giant's own build graph (dogfooding).
#
# Derives a release build+install target for every Rust binary in the workspace
# from `cargo metadata`, keyed to the devenv toolchain identity. Replaces the
# hand-written per-crate giant.yaml build targets, so adding a crate can't drift
# out of sync with the build graph (the generators and the link pass).
#
# cargo.star is fetched from giant-std so this repo's own
# The collection lives in github.com/giantdotbuild/giant-std.
# See giant.yaml for the ref
load("@std//cargo.star", "cargo_metadata", "cargo_packages", "cargo_targets")

# The platforms a release ships, matching .github/workflows/release.yml.
RELEASE_PLATFORMS = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
]

# One target per platform tarball: the full workspace cross-built with the
# remote feature, binaries installed under output/release/<triple>/. The
# tarball is the release unit - any source change rebuilds it wholesale -
# so coarse granularity is the honest granularity, and a re-release of
# unchanged sources is a pure cache restore.
#
# Tagged `release` and keyed to //:rustc (not //:devenv): these also run on
# release runners that have rustup but no devenv. Cross-toolchains are the
# runner's problem; select with `--tag platform=<triple>` and exclude from
# bulk builds with `--no-tag release`.
def release_targets(ws):
    pkgs = cargo_packages(ws, cargo_metadata(ws))
    inputs = ["//Cargo.toml", "//Cargo.lock"]
    bins = []
    for p in pkgs:
        inputs.append("//" + p["dir"] + "/src/**/*.rs")
        inputs.append("//" + p["dir"] + "/Cargo.toml")
        if ws.glob(p["dir"] + "/build.rs"):
            inputs.append("//" + p["dir"] + "/build.rs")
        bins += p["bins"]
    bins = sorted(bins)
    for triple in RELEASE_PLATFORMS:
        out = "output/release/" + triple
        installs = " && ".join([
            "install -m 0755 target/" + triple + "/release/" + b + " " + out + "/" + b
            for b in bins
        ])
        target(
            name = "release-" + triple,
            inputs = inputs,
            outputs = ["//" + out + "/" + b for b in bins],
            cwd = "//",
            command = "mkdir -p " + out +
                      " && cargo build --release --locked --workspace --features giant/remote --target " +
                      triple + " && " + installs,
            deps = ["//:rustc"],
            timeout_secs = 3600,
            tags = ["release", "platform=" + triple],
        )

def generate(ws):
    # Every workspace binary, re-keyed by the toolchain (//:devenv) so a devenv
    # change invalidates them. The root giant.yaml keeps the workspace settings,
    # //:devenv, sandbox config, and tasks; docs-site keeps its npm targets.
    cargo_targets(ws, deps = ["//:devenv"])
    release_targets(ws)
