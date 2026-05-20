---
title: Docker images
description: Build, cache, and push container images with Giant.
---

Docker images are a good test of Giant's `exists:` mechanism: the
images themselves are large enough that you don't want to cache them
in Giant's content-addressed store, but you do want to skip the
`docker build` step when nothing has changed.

## Simple case: per-Dockerfile target

```yaml
targets:
  - id: "docker:api"
    inputs:
      - "Dockerfile"
      - "src/**/*"
    outputs: []
    cache: false
    exists: "docker image inspect example/api:$INPUTS_HASH >/dev/null 2>&1"
    command: |
      docker build \
        -t example/api:$INPUTS_HASH \
        -t example/api:latest \
        --build-arg VERSION=$INPUTS_HASH \
        .
```

The `exists` command runs first. If the image already exists locally
(or in the registry, if you swap the check to use `docker manifest`),
Giant skips the `command` and treats the target as built.

`INPUTS_HASH` is the cache key, available as an environment variable
in both `exists` and `command`. Tagging the image with it gives you a
trivial "is this image up to date?" lookup.

## Push as a separate target

Keep building and pushing in separate targets so you can opt into the
push:

```yaml
targets:
  - id: "docker:api"
    inputs: ["Dockerfile", "src/**/*"]
    outputs: []
    cache: false
    exists: "docker image inspect example/api:$INPUTS_HASH >/dev/null 2>&1"
    command: "docker build -t example/api:$INPUTS_HASH ."

  - id: "docker:api:push"
    inputs: []
    deps: ["docker:api"]
    outputs: []
    cache: false
    tags: ["push"]
    exists: "docker manifest inspect example/api:$INPUTS_HASH >/dev/null 2>&1"
    command: "docker push example/api:$INPUTS_HASH"
```

Run the build alone:

```bash
giant build docker:api
```

Push only:

```bash
giant build --tag push
```

## Discovery for many Dockerfiles

If you have many Dockerfiles (one per service), use discovery:

```yaml
include:
  - id: "discover:docker"
    inputs:
      - "tools/discover-docker.sh"
      - kind: structural
        files: "**/Dockerfile*"
        lines: ["FROM ", "COPY ", "ADD "]
    outputs: [".giant/d/docker.json"]
    command: "tools/discover-docker.sh > .giant/d/docker.json"
```

```bash
#!/usr/bin/env bash
# tools/discover-docker.sh
set -euo pipefail

find . -name 'Dockerfile' -not -path './.giant/*' \
  | while read -r df; do
      svc="$(dirname "$df" | sed 's|^\./||;s|/|-|g')"
      jq -n \
        --arg id "docker:$svc" \
        --arg dir "$(dirname "$df")" \
        --arg cmd "docker build -t example/$svc:\$INPUTS_HASH -f $df $dir" \
        '{
          id: $id,
          inputs: ["\($dir)/Dockerfile", "\($dir)/**/*"],
          outputs: [],
          cache: false,
          exists: ("docker image inspect example/" + ($id | sub("docker:"; "")) + ":$INPUTS_HASH >/dev/null 2>&1"),
          command: $cmd
        }'
    done | jq -s '{targets: .}'
```

You get one `docker:<service>` target per Dockerfile, automatically.

## Multi-stage caching

If you want to cache intermediate build stages too, lean on Docker's
BuildKit cache mounts and target a remote builder:

```yaml
- id: "docker:api"
  command: |
    docker buildx build \
      --cache-from type=registry,ref=example/api:cache \
      --cache-to type=registry,ref=example/api:cache,mode=max \
      -t example/api:$INPUTS_HASH \
      .
```

Giant's cache and Docker's cache are independent - Giant skips the
`docker build` invocation entirely on a Giant cache hit, while Docker's
own cache covers fast layer rebuilds when Giant does call into it.
