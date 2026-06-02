---
title: Docker images
description: Build, cache, and push container images with Giant.
---

Docker images are a good test of Giant's `exists:` mechanism: the
images themselves are large enough that you don't want to cache them
in Giant's content-addressed store, but you do want to skip the
`docker build` step when nothing has changed.

A containerised service is a natural [package](/concepts/packages/):
its `Dockerfile`, its sources, and its build target all live in one
directory. A service in `services/api/` is package `//services/api`,
and its image target is `//services/api:image`. Paths inside that
package file are package-relative, so the target reads its own tree with
bare globs.

## Simple case: per-Dockerfile target

```yaml
# services/api/giant.yaml
targets:
  - name: "image"
    inputs:
      - "Dockerfile"
      - "src/**/*"
    outputs: []
    cache: false
    tags: ["kind=image"]
    exists: "docker image inspect example/api:$GIANT_CACHE_KEY >/dev/null 2>&1"
    command: |
      docker build \
        -t example/api:$GIANT_CACHE_KEY \
        -t example/api:latest \
        --build-arg VERSION=$GIANT_CACHE_KEY \
        .
```

The target's label is `//services/api:image`. The `cwd` defaults to the
package directory (`services/api/`), so the trailing `.` in `docker
build` is the package itself.

The `exists` command runs first. If the image already exists locally
(or in the registry, if you swap the check to use `docker manifest`),
Giant skips the `command` and treats the target as built. The renderer
shows it as `≡ EXTERNAL` (an external cache hit) and skips the usual build
line.

`GIANT_CACHE_KEY` is the cache key, available as an environment variable
in both `exists` and `command`. Tagging the image with it gives you a
trivial "is this image up to date?" lookup.

## Push as a separate target

Keep building and pushing in separate targets so you can opt into the
push:

```yaml
# services/api/giant.yaml
targets:
  - name: "image"
    inputs: ["Dockerfile", "src/**/*"]
    outputs: []
    cache: false
    tags: ["kind=image"]
    exists: "docker image inspect example/api:$GIANT_CACHE_KEY >/dev/null 2>&1"
    command: "docker build -t example/api:$GIANT_CACHE_KEY ."

  - name: "push"
    inputs: []
    deps: ["//services/api:image"]
    outputs: []
    cache: false
    tags: ["kind=push"]
    exists: "docker manifest inspect example/api:$GIANT_CACHE_KEY >/dev/null 2>&1"
    command: "docker push example/api:$GIANT_CACHE_KEY"
```

Run the build alone:

```bash
giant build //services/api:image
```

Push only:

```bash
giant build --tag kind=push
```

## Many services, one package each

With several services, give each its own package directory and one image
target apiece. Each `giant.yaml` points at its own `Dockerfile` and
source tree with package-relative paths, so a change to one service only
re-keys that service's image:

```yaml
# services/api/giant.yaml
targets:
  - name: "image"
    inputs: ["Dockerfile", "src/**/*"]
    outputs: []
    cache: false
    tags: ["kind=image"]
    exists: "docker image inspect example/api:$GIANT_CACHE_KEY >/dev/null 2>&1"
    command: "docker build -t example/api:$GIANT_CACHE_KEY ."
```

```yaml
# services/worker/giant.yaml
targets:
  - name: "image"
    inputs: ["Dockerfile", "src/**/*"]
    outputs: []
    cache: false
    tags: ["kind=image"]
    exists: "docker image inspect example/worker:$GIANT_CACHE_KEY >/dev/null 2>&1"
    command: "docker build -t example/worker:$GIANT_CACHE_KEY ."
```

Editing `services/api/**` re-keys `//services/api:image` and leaves
`//services/worker:image` cache-warm. Each package's globs stop at its
own boundary, so the two never claim each other's files. Build every
image at once with the `kind=image` tag, or with a recursive selection:

```bash
giant build --tag kind=image     # all image targets
giant build //services/...       # everything under services/
```

When the service count grows past what you want to hand-write, generate
the per-service `giant.yaml` files offline and check them in - see
[Generating config](/guides/generating-config/).

## Multi-stage caching

If you want to cache intermediate build stages too, lean on Docker's
BuildKit cache mounts and target a remote builder:

```yaml
# services/api/giant.yaml
- name: "image"
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
