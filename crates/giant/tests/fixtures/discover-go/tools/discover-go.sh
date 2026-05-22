#!/usr/bin/env bash
# Example: emit a Giant discovery fragment from `go list -json`.
#
# Pattern: one target per package in the current module. Library
# packages become test targets; the `main` package becomes a binary
# target. Same-module imports become explicit deps.
#
# The output also carries a `reads` manifest (TDD-0015) so the engine
# can verify whether the cached fragment is still valid on the next
# run without re-executing this script:
#   - go.mod / go.sum: whole-file. Any edit re-runs discovery.
#   - each .go file: excerpt on `package`, `import`, `//go:embed` -
#     function-body edits don't re-run; package/import changes do.
#   - each package's directory: listing filtered to `*.go` - adding
#     or removing a .go file in a known package re-runs.
#
# Caveat: adding a brand-new package directory is not caught by the
# manifest above. A production discovery would also record parent
# directories (or the module root with a recursive listing). Out of
# scope for this example.
#
# Drop this in tools/ and reference it from giant.yaml:
#
#   include:
#     - id: "discover:go"
#       command: "tools/discover-go.sh > .giant/d/go.json"
#       outputs: [".giant/d/go.json"]
#       scope: ["."]
set -euo pipefail

MODULE=$(go list -m)
HAVE_GOSUM=$([ -f go.sum ] && echo 1 || echo 0)

go list -json -deps ./... 2>/dev/null \
  | jq -s \
      --arg module "$MODULE" \
      --arg cwd "$PWD" \
      --argjson have_gosum "$HAVE_GOSUM" '
      # Keep only packages in our module (drop std + third-party deps).
      map(select(.Module.Path == $module)) as $pkgs

      # For every same-module package, every .go file relative to $cwd.
      | ($pkgs | map(
          . as $p
          | ($p.Dir | sub("^" + $cwd + "/?"; "")) as $reldir
          | (($p.GoFiles // []) + ($p.TestGoFiles // []) + ($p.CgoFiles // []))
          | map(if $reldir == "" then . else $reldir + "/" + . end)
        ) | add // []) as $all_go_files

      # Every same-module package directory, deduplicated.
      | ($pkgs
          | map(.Dir | sub("^" + $cwd + "/?"; ""))
          | map(if . == "" then "." else . end)
          | unique) as $pkg_dirs

      | {
          schema_version: 1,
          targets: ($pkgs | map(
            . as $p
            | ($p.ImportPath | sub("^" + $module + "/?"; "")) as $rel
            | (if $rel == "" then "root" else $rel end) as $relname
            | ($p.Dir | sub("^" + $cwd + "/?"; "")) as $reldir
            | {
                id: ("go:pkg:" + $relname),
                inputs: (
                  (($p.GoFiles // []) + ($p.TestGoFiles // []) + ($p.CgoFiles // []))
                  | map(if $reldir == "" then . else $reldir + "/" + . end)
                ) + ["go.mod"] + (if $have_gosum == 1 then ["go.sum"] else [] end),
                deps: (
                  ($p.Imports // [])
                  | map(select(startswith($module + "/") or . == $module))
                  | map(sub("^" + $module + "/?"; ""))
                  | map(if . == "" then "go:pkg:root" else "go:pkg:" + . end)
                ),
                command: (
                  if $p.Name == "main"
                  then ("go build -o bin/" + ($relname | split("/") | last) + " ./" + $reldir)
                  else ("go test ./" + (if $reldir == "" then "." else $reldir end))
                  end
                ),
                outputs: (
                  if $p.Name == "main"
                  then ["bin/" + ($relname | split("/") | last)]
                  else []
                  end
                ),
                test: ($p.Name != "main"),
                cache: ($p.Name == "main")
              }
          )),
          reads: {
            files: (
              [ {"path": "go.mod"} ]
              + (if $have_gosum == 1 then [ {"path": "go.sum"} ] else [] end)
              + ($all_go_files | map({
                  "path": .,
                  "lines": ["package ", "import ", "//go:embed "]
                }))
            ),
            dirs: ($pkg_dirs | map({"path": ., "filter": "*.go"}))
          }
        }'
