---
title: Quickstart
description: From zero to a cached build in under two minutes.
---

This is the shortest path from "I have Giant installed" to "I just got
a cache hit on a real build."

## Install

Pick whichever you prefer. The binary is a single static-linked file;
no daemon, no dependencies at runtime.

```bash
# Pre-built binary (Linux/macOS)
curl -fsSL https://giant.build/install.sh | sh

# From source
cargo install --path . --git https://github.com/johnae/giant

# With the remote cache feature
cargo install --path . --features remote --git https://github.com/johnae/giant
```

Verify:

```console
$ giant --version
giant 0.1.0
```

## A first config

Create `giant.yaml` in any directory:

```yaml
workspace:
  name: hello-giant
cache:
  dir: ~/.cache/giant

targets:
  - id: "demo:greet"
    inputs: ["name.txt"]
    outputs: ["greeting.txt"]
    command: "echo \"hello, $(cat name.txt)\" > greeting.txt"
```

Add an input file:

```console
$ echo world > name.txt
```

## Build it

```console
$ giant build
✓ BUILD   demo:greet   4ms
  OK    1 built · 0 cached · 0 failed  in 4ms

$ cat greeting.txt
hello, world
```

## Watch the cache work

Run it again with no changes:

```console
$ giant build
✓ CACHE   demo:greet   1ms
  OK    0 built · 1 cached · 0 failed  in 1ms
```

Cache hit. Delete the output to prove the cache restores it:

```console
$ rm greeting.txt
$ giant build
✓ CACHE   demo:greet   2ms
$ cat greeting.txt
hello, world
```

Now edit the input:

```console
$ echo galaxy > name.txt
$ giant build
✓ BUILD   demo:greet   3ms
$ cat greeting.txt
hello, galaxy
```

Giant noticed `name.txt` changed (its content hash differs), invalidated
the cache key, and re-ran the command.

## Where to next

- **[Your first build](/start/first-build/)** walks through the same
  example with more annotations on what's happening under the hood.
- **[Concepts: targets and inputs](/concepts/targets/)** is the model.
- **[Go monorepo guide](/guides/go-monorepo/)** is a real-world recipe
  showing discovery, structural inputs, and inferred deps together.
