#!/usr/bin/env bash
# Example: emit a Giant discovery fragment from `go list -json`.
#
# Pattern: one target per package in the current module. Library
# packages become test targets; the `main` package becomes a binary
# target. Same-module imports become explicit deps.
#
# Drop this in tools/ and reference it from giant.yaml:
#
#   include:
#     - id: "discover:go"
#       inputs:
#         - "tools/discover-go.sh"
#         - "go.mod"
#         - "go.sum"
#         - kind: structural
#           files: "**/*.go"
#           lines: ["package ", "import ", "//go:embed "]
#       outputs: [".giant/d/go.json"]
#       command: "tools/discover-go.sh > .giant/d/go.json"
set -euo pipefail

MODULE=$(go list -m)

go list -json -deps ./... 2>/dev/null \
  | jq -s --arg module "$MODULE" --arg cwd "$PWD" '
      # Keep only packages in our module (drop std + third-party deps).
      map(select(.Module.Path == $module))
      | {
          schema_version: 1,
          targets: map(
            . as $p
            | ($p.ImportPath | sub("^" + $module + "/?"; "")) as $rel
            | (if $rel == "" then "root" else $rel end) as $relname
            | ($p.Dir | sub("^" + $cwd + "/?"; "")) as $reldir
            | {
                id: ("go:pkg:" + $relname),
                inputs: (
                  (($p.GoFiles // []) + ($p.TestGoFiles // []) + ($p.CgoFiles // []))
                  | map(if $reldir == "" then . else $reldir + "/" + . end)
                ) + ["go.mod", "go.sum"],
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
          )
        }'
