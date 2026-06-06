# @giant//go.star - Go target generation over the host primitives.
#
# Embedded in the giant-gen binary and reached with
# `load("@giant//go.star", ...)`; nothing to install. Built entirely on the
# generic host capabilities (ws.exec + parse_json_stream + target()), so the
# Go-specific opinion lives here in editable Starlark, not in the host.

# Raw `go list -json -deps ./...` for the module under `dir` (the dir holding
# go.mod, workspace-relative; "." for a root module). Returns every package in
# the build graph, vendor included. Run this ONCE and feed the result to
# `packages_of` / `cgo_index_of` to avoid re-listing.
def go_list(ws, dir = "."):
    out = ws.exec(["go", "list", "-json", "-deps", "./..."], cwd = dir)
    return parse_json_stream(out.stdout)

# First-party packages (part of the main module, per `go list`'s Module.Main)
# from a raw `go_list`.
def packages_of(ws, raw):
    pkgs = []
    for p in raw:
        mod = p.get("Module")
        if not mod or not mod.get("Main", False):
            continue
        pkgs.append({
            "import": p["ImportPath"],
            "dir": ws.rel(p["Dir"]),
            "name": p["Name"],
            "is_main": p["Name"] == "main",
            "has_tests": len(p.get("TestGoFiles", [])) + len(p.get("XTestGoFiles", [])) > 0,
            "deps": p.get("Deps", []),
            "embeds": p.get("EmbedFiles", []),
        })
    return pkgs

# Map import-path -> cgo link libs (the `-l<lib>` from each package's
# `#cgo LDFLAGS`) for every cgo package in a raw `go_list`, vendored C bindings
# included. Lets a generator detect cgo binaries and their native libs from
# `go list` alone, with no per-package sidecar config.
def cgo_index_of(raw):
    idx = {}
    for p in raw:
        if not p.get("CgoFiles"):
            continue
        libs = [f[2:] for f in p.get("CgoLDFLAGS", []) if f.startswith("-l")]
        if libs:
            idx[p["ImportPath"]] = libs
    return idx

# Convenience one-shot wrappers (each runs `go list` once). Prefer `go_list` +
# `packages_of`/`cgo_index_of` when you need both, to list only once.
def go_packages(ws, dir = "."):
    return packages_of(ws, go_list(ws, dir))

def go_cgo_index(ws, dir = "."):
    return cgo_index_of(go_list(ws, dir))

# Index packages by import path, for input derivation.
def go_index(pkgs):
    return {p["import"]: p for p in pkgs}

# Inputs for a target whose package is `pkg`: a single-level *.go glob for the
# package and each of its first-party transitive deps, their go:embed files,
# and the module's go.mod/go.sum. Sorted and deduped (determinism).
def go_inputs(pkg, index, mod_dir = "."):
    items = {}
    chain = [pkg] + [index[i] for i in pkg["deps"] if i in index]
    for q in chain:
        items["//" + q["dir"] + "/*.go"] = True
        for e in q["embeds"]:
            items["//" + q["dir"] + "/" + e] = True
    prefix = "" if mod_dir == "." else mod_dir + "/"
    items["//" + prefix + "go.mod"] = True
    items["//" + prefix + "go.sum"] = True
    return sorted(items.keys())

# Binary name from a package dir, collapsing a trailing `cmd` wrapper
# (`foo/cmd` -> `foo`); `cmd/foo` already yields `foo` as the leaf.
def bin_name(dir):
    parts = dir.split("/")
    if parts[-1] == "cmd" and len(parts) > 1:
        return parts[-2]
    return parts[-1]

# Emit a build target. A goos/goarch pair makes it a pure-Go (CGO_ENABLED=0)
# cross build; otherwise a host build. cwd defaults to the package dir, so
# `go build .` builds this package and finds the module's go.mod upward.
def go_binary(name, pkg, inputs, output, deps = [], goos = None, goarch = None):
    env = {}
    tags = ["lang=go", "kind=bin"]
    if goos:
        env = {"CGO_ENABLED": "0", "GOOS": goos, "GOARCH": goarch}
        tags.append("platform=" + goos + "-" + goarch)
    target(
        name = name,
        command = "go build -o $GIANT_WORKSPACE_ROOT/" + output + " .",
        inputs = inputs,
        outputs = ["//" + output],
        deps = deps,
        env = env,
        tags = tags,
        package = pkg["dir"],
    )

# Emit a test target (uncached by default; tests touch the world). Marks a
# sentinel output so it can participate in the cache when `cache = True`.
def go_test(pkg, inputs, deps = [], cache = False):
    marker = "output/test/" + pkg["dir"].replace("/", "_") + ".ok"
    target(
        name = "test",
        command = "go test ./... && mkdir -p $GIANT_WORKSPACE_ROOT/output/test && touch $GIANT_WORKSPACE_ROOT/" + marker,
        inputs = inputs,
        outputs = ["//" + marker],
        deps = deps,
        test = True,
        cache = cache,
        tags = ["lang=go", "kind=test"],
        package = pkg["dir"],
    )

# The floor: a build target per main package and a test target per package
# with tests, for the module under `dir`. `deps` ride every target; tests also
# get `test_deps` (default: `deps`). `output` is a template with `{name}`.
def go_targets(ws, dir = ".", deps = [], test_deps = None, output = "bin/{name}"):
    pkgs = go_packages(ws, dir)
    index = go_index(pkgs)
    tdeps = deps if test_deps == None else test_deps
    for p in pkgs:
        inputs = go_inputs(p, index, dir)
        if p["is_main"]:
            name = bin_name(p["dir"])
            go_binary(name, p, inputs, output.replace("{name}", name), deps = deps)
        if p["has_tests"]:
            go_test(p, inputs, deps = tdeps)
