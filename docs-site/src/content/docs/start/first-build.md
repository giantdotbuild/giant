---
title: Your first build
description: A guided tour of one Giant build, end to end.
---

The quickstart got you a cached build in under two minutes. This page
walks through the same example slowly, naming each piece you saw.

## The config

```yaml
workspace:
  name: hello-giant
cache:
  dir: ~/.cache/giant

targets:
  - name: "demo"
    inputs: ["name.txt"]
    outputs: ["greeting.txt"]
    command: "echo \"hello, $(cat name.txt)\" > greeting.txt"
```

Three sections:

- **`workspace`** names the workspace. Used in cache keys and as the
  default label in the renderer.
- **`cache`** points to the local cache directory. `~/.cache/giant` is
  the default; you can override per-workspace.
- **`targets`** is the list of things Giant knows how to build. Each
  target has a `name`, a list of `inputs`, a list of `outputs`, and a
  `command`.

This single file is the workspace root, so its targets live in the root
package and are addressed as `//:name` - here, `//:demo`. That's enough
for a small project. As the repo grows you split config across
[packages](/concepts/packages/) (one `giant.yaml` per directory), and a
large tree's package files are usually
[generated](/guides/generating-config/) rather than hand-written.

That's it for the minimum config. Everything else - tasks, remote
cache, tags - is optional.

## Anatomy of a target

```yaml
- name: "demo"
  inputs: ["name.txt"]
  outputs: ["greeting.txt"]
  command: "echo \"hello, $(cat name.txt)\" > greeting.txt"
```

- **`name`** is the target's local name, unique within its package. The
  engine identity is the **label** `//<package>:<name>` - the package is
  the directory of this `giant.yaml`, so a root-file target named `demo`
  is `//:demo`. Selection matches against the full label (see the
  [selection language](/concepts/selection/)).
- **`inputs`** are file globs, **package-relative** by default. A bare
  glob like `src/**/*.go` resolves under the package directory; prefix
  `//` to reach the workspace root (`//Cargo.lock`). Anything they match
  contributes to the cache key.
- **`outputs`** are files the command produces. Package-relative unless
  `//`-prefixed, and resolved against the target's `cwd` (which defaults
  to the package directory). Giant fingerprints them after the build and
  stores them in the content-addressed cache.
- **`command`** is a shell command. Giant runs it via `sh -c` in the
  target's `cwd`.

## The first build (cache miss)

```console
$ giant build
✓ BUILD   //:demo   4ms
  OK    1 built · 0 cached · 0 failed  in 4ms
```

What happened, in order:

1. **Config load.** Scan the tree for `giant.yaml` files, merge them into
   one graph (here just the root file), resolve package-relative paths,
   validate.
2. **Graph build.** One target, no dependencies.
3. **Cache key compute.** SHA-256 over: the command, the cwd, the
   content hash of `name.txt`, the env vars listed under `built_in_env`,
   and the dependency hashes (none here).
4. **Cache lookup.** Local cache miss - first run.
5. **Execute.** Run `echo "hello, $(cat name.txt)" > greeting.txt`.
6. **Fingerprint outputs.** Hash `greeting.txt`, store its bytes in the
   content-addressed store under the hash.
7. **Write AC entry.** Save an action-cache JSON file mapping the cache
   key to the output hashes.

## The second build (cache hit)

```console
$ giant build
✓ CACHE   //:demo   1ms
  OK    0 built · 1 cached · 0 failed  in 1ms
```

1. **Config load + graph build** - same as before.
2. **Cache key compute.** Same inputs → same hash → same key.
3. **Cache lookup.** Hit. Read the AC entry, pull `greeting.txt`'s
   bytes out of CAS, write them to disk.
4. **Done.** No command was run.

The whole second-build path is dominated by file I/O. On a warm cache
the in-process work is sub-millisecond per target.

## Editing an input

```console
$ echo galaxy > name.txt
$ giant build
✓ BUILD   //:demo   3ms
```

- `name.txt`'s content hash changed (its bytes are different).
- New cache key.
- Lookup misses.
- Build re-runs.

## Where to go now

- **[Concepts: the cache key](/concepts/cache-key/)** - what feeds the
  hash and what doesn't.
- **[Concepts: targets and inputs](/concepts/targets/)** - full schema.
- **[CLI reference](/reference/cli/)** - every subcommand.
