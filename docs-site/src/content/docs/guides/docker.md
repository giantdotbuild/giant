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
    exists: "docker image inspect example/api:$GIANT_CACHE_KEY >/dev/null 2>&1"
    command: |
      docker build \
        -t example/api:$GIANT_CACHE_KEY \
        -t example/api:latest \
        --build-arg VERSION=$GIANT_CACHE_KEY \
        .
```

The `exists` command runs first. If the image already exists locally
(or in the registry, if you swap the check to use `docker manifest`),
Giant skips the `command` and treats the target as built. The renderer
shows it as `≡ EXTERNAL` (an external cache hit), not a normal build
line.

`GIANT_CACHE_KEY` is the cache key, available as an environment variable
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
    exists: "docker image inspect example/api:$GIANT_CACHE_KEY >/dev/null 2>&1"
    command: "docker build -t example/api:$GIANT_CACHE_KEY ."

  - id: "docker:api:push"
    inputs: []
    deps: ["docker:api"]
    outputs: []
    cache: false
    tags: ["push"]
    exists: "docker manifest inspect example/api:$GIANT_CACHE_KEY >/dev/null 2>&1"
    command: "docker push example/api:$GIANT_CACHE_KEY"
```

Run the build alone:

```bash
giant build docker:api
```

Push only:

```bash
giant build --tag push
```

## Many Dockerfiles, one target each

With several services, write one target per Dockerfile. Each points at
its own `Dockerfile` and source tree, so a change to one service only
re-keys that service's image:

```yaml
targets:
  - id: "docker:api"
    inputs: ["services/api/Dockerfile", "services/api/**/*"]
    outputs: []
    cache: false
    exists: "docker image inspect example/api:$GIANT_CACHE_KEY >/dev/null 2>&1"
    command: "docker build -t example/api:$GIANT_CACHE_KEY -f services/api/Dockerfile services/api"

  - id: "docker:worker"
    inputs: ["services/worker/Dockerfile", "services/worker/**/*"]
    outputs: []
    cache: false
    exists: "docker image inspect example/worker:$GIANT_CACHE_KEY >/dev/null 2>&1"
    command: "docker build -t example/worker:$GIANT_CACHE_KEY -f services/worker/Dockerfile services/worker"
```

Editing `services/api/**` re-keys `docker:api` and leaves
`docker:worker` cache-warm. Build all images at once with a glob:

```bash
giant build 'docker:*'
```

## Multi-stage caching

If you want to cache intermediate build stages too, lean on Docker's
BuildKit cache mounts and target a remote builder:

```yaml
- id: "docker:api"
  command: |
    docker buildx build \
      --cache-from type=registry,ref=example/api:cache \
      --cache-to type=registry,ref=example/api:cache,mode=max \
      -t example/api:$GIANT_CACHE_KEY \
      .
```

Giant's cache and Docker's cache are independent - Giant skips the
`docker build` invocation entirely on a Giant cache hit, while Docker's
own cache covers fast layer rebuilds when Giant does call into it.
