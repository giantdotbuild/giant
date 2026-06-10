---
title: Starlark host
description: Every primitive giant gen exposes to a giant.star generator.
---

`giant gen` runs `giant.star` scripts in an embedded
[Starlark](https://github.com/bazelbuild/starlark) interpreter. The host is
deliberately small: a workspace handle, a few parsers, and a `target()`
builtin. Language- and tool-specific opinion lives in Starlark on top -
the [std collection](https://github.com/giantdotbuild/giant-std) or your
own modules - never in the host.

Starlark exists only here, inside the `giant-gen` binary, and only to
produce YAML. The engine loads the committed `giant.<infix>.yaml` files and
has no interpreter to evaluate anything else; a build never runs a
generator. See [Generating config](/guides/generating-config/) for the
workflow that follows from that (commit the output, gate drift in CI with
`giant gen --check`).

## The contract

A generator script defines `generate(ws)`. The host evaluates the script,
calls `generate`, and collects every `target()` registered while it runs
(the return value is ignored). Collected targets are grouped by package and
written as one `giant.<infix>.yaml` per package, where `<infix>` comes from
the `generate:` entry:

```yaml
generate:
  - { script: gen-go.star, infix: go }        # writes giant.go.yaml files
  - { script: gen-docker.star, infix: docker } # writes giant.docker.yaml files
```

Emission is deterministic and byte-stable, which is what makes
`giant gen --check` a diff gate.

## `load()` resolution

| Form | Resolves to |
|---|---|
| `load("@std//go.star", ...)` | A module in the std collection - the workspace's [`std:` pin or path](/reference/config/#std), a `GIANT_STD` directory, or an install-relative copy. |
| `load("star/cargo.star", ...)` | A repo-local file, relative to the workspace root (the [vendoring](/guides/generating-config/#vendoring) convention). |

Loaded modules see the same globals as the entry script, and each module
loads once per run.

## `ws` - the workspace handle

| Method | Returns |
|---|---|
| `ws.glob(pattern)` | Workspace-relative paths matching a glob; sorted, gitignore-aware. |
| `ws.read(path)` | The contents of a workspace-relative file. |
| `ws.exec(args, cwd = None, check = True)` | Runs a subprocess from the workspace root (or `cwd`, workspace-relative). Returns `struct(stdout, stderr, code)`; raises on nonzero exit unless `check = False`. |
| `ws.rel(path)` | An absolute or `//`-rooted path, relativized against the workspace root. |
| `ws.stem(path)` | A filename without its directory or extension. |

`ws.exec` is how generators ask toolchains about the tree - `go list`,
`cargo metadata`, `buf ls-files` - instead of reimplementing their file
layouts.

## Parsers

| Function | Parses |
|---|---|
| `parse_json(s)` | One JSON value. |
| `parse_json_stream(s)` | Concatenated JSON objects (the `go list -json` shape) into a list. |
| `parse_yaml(s)` | One YAML document. |
| `parse_toml(s)` | One TOML document. |

All return plain Starlark data (dicts, lists, strings, ints, bools, None).
`parse_yaml(ws.read("giant.yaml"))` is the idiom for reading
[workspace-level generator config](/guides/generating-config/#workspace-config)
out of the root file.

## `target()` - emit one target

Validated against the same wire schema as hand-written
[`giant.yaml` targets](/reference/config/#target-fields); a bad call fails
the generation run, never the build.

| Parameter | Type | Meaning |
|---|---|---|
| `name` | string | Required. Target name, unique in its package. |
| `command` | string | Required. The shell command. |
| `inputs` | list | File globs feeding the cache key. Package-relative; `//` anchors the workspace root. |
| `outputs` | list | Files the command produces. |
| `deps` | list | Explicit deps as labels (inference fills the rest at link time). |
| `env` | dict | Environment variables, hashed into the cache key. |
| `cwd` | string | Working dir; defaults to the package dir. |
| `cache` | bool | `False` disables caching. Default: `True` for build targets, `False` for tests. |
| `remote_cache` | bool | `False` excludes from remote uploads. Default `True`. |
| `network` | bool | Network access when sandboxed. Default `False`. |
| `sandbox` | bool | `False` exempts the target from `--sandbox`. Default `True`. |
| `exists` | string | External check; exit 0 skips the command. |
| `timeout_secs` | int | Kill the command after this long. |
| `test` | bool | Marks a test target. Default `False`. |
| `tags` | list | Free-form labels for `--tag` filtering. |
| `package` | string | Which package's `giant.<infix>.yaml` the target lands in. Defaults to the dir of a relative `cwd`, else the root package. |
| `label` | string | Explicit label override; rarely needed. |

After all generators run, a link pass resolves inferred dependencies (an
input glob matching another target's output) across the whole generated
tree and writes them into the files.

## The dialect

Standard Starlark: no `while`, no recursion, no unbounded loops -
generation always terminates. Determinism is on you only where you call
out: `ws.exec` output is whatever the tool prints, so keep generator
commands deterministic (sorted output, no timestamps) or sort what you
take from them.
