# cargo.star - Cargo/Rust target generation over the host primitives.
#
# Part of giant's official Starlark std collection. Derives build
# targets from `cargo metadata`, the same way go.star derives from `go list`.
# Built entirely on the generic host (ws.exec + parse_json + target()), so the
# Rust-specific opinion lives here in editable Starlark, not in the host.

# Raw `cargo metadata --no-deps` for the workspace under `dir` ("." for the
# root). `--no-deps` keeps it to workspace members and needs no network.
def cargo_metadata(ws, dir = "."):
    out = ws.exec(["cargo", "metadata", "--no-deps", "--format-version", "1"], cwd = dir)
    return parse_json(out.stdout)

# Workspace member packages from raw metadata: crate name, workspace-relative
# dir, and the names of its `bin` targets (empty for a lib-only crate).
def cargo_packages(ws, meta):
    pkgs = []
    for p in meta["packages"]:
        parts = p["manifest_path"].split("/")
        pkg_dir = ws.rel("/".join(parts[:-1]))
        bins = [t["name"] for t in p["targets"] if "bin" in t["kind"]]
        pkgs.append({"name": p["name"], "dir": pkg_dir, "bins": bins})
    return pkgs

# Cache-key inputs for a crate: its Rust sources, build script if present,
# the package manifest, and the workspace manifest + lockfile (the dependency
# axis). Package-relative except the `//`-rooted workspace files; `dir` is
# the workspace dir for a nested workspace. `extra` adds non-Rust inputs the
# crate compiles in (sqlx queries, templates, included assets).
def cargo_inputs(ws, pkg, dir = ".", extra = []):
    prefix = "" if dir == "." else dir + "/"
    items = ["src/**/*.rs", "Cargo.toml", "//" + prefix + "Cargo.toml", "//" + prefix + "Cargo.lock"]
    if ws.glob(pkg["dir"] + "/build.rs"):
        items.append("build.rs")
    return items + extra

# Emit a release build-and-install target for binary `bin` of `pkg`. Installs
# to `//bin/<bin>`; cwd is the workspace dir so cargo drives the whole
# workspace and resolves intra-workspace deps itself.
def cargo_bin(ws, pkg, bin, deps = [], dir = ".", features = [], extra_inputs = []):
    flags = " --features " + ",".join(features) if features else ""
    target(
        name = bin,
        inputs = cargo_inputs(ws, pkg, dir, extra_inputs),
        outputs = ["//bin/" + bin],
        cwd = "//" if dir == "." else "//" + dir,
        command = "cargo build --release -p " + pkg["name"] + " --bin " + bin + flags +
                  " && install -m 0755 target/release/" + bin + " $GIANT_WORKSPACE_ROOT/bin/" + bin,
        deps = deps,
        timeout_secs = 600,
        tags = ["bin", "rust"],
        package = pkg["dir"],
    )

# The floor: a build-and-install target for every binary in the workspace.
# `deps` ride every target (e.g. a toolchain-identity target so a toolchain
# change re-keys the build). `exclude` skips members by crate name - example
# or fixture members a workspace lists but doesn't ship. A binary needing
# features or extra inputs drops to `cargo_bin` for that one and excludes
# its crate here.
def cargo_targets(ws, deps = [], dir = ".", exclude = []):
    meta = cargo_metadata(ws, dir)
    for pkg in cargo_packages(ws, meta):
        if pkg["name"] in exclude:
            continue
        for bin in pkg["bins"]:
            cargo_bin(ws, pkg, bin, deps = deps, dir = dir)
