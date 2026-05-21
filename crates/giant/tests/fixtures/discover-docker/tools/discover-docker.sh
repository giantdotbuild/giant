#!/usr/bin/env bash
# Example: emit a Giant discovery fragment, one target per Dockerfile.
#
# Pattern: walk the workspace looking for Dockerfile files; each becomes
# a docker:<service> target. The target's cache key incorporates the
# whole service directory, and `exists:` checks the registry by content
# hash so we don't rebuild images that are already pushed.
#
# This fixture uses safe placeholder commands (echo) instead of real
# `docker buildx build` so the test doesn't require a docker daemon.
# In a real workspace replace BUILD_CMD and EXISTS_CMD with the
# commented-out docker commands.
set -euo pipefail

REGISTRY="${DOCKER_REGISTRY:-localhost:5000}"

# Find all Dockerfiles, skipping common build / VCS / cache dirs.
mapfile -t dockerfiles < <(
  find . \
    -name 'Dockerfile' \
    -not -path '*/.git/*' \
    -not -path '*/node_modules/*' \
    -not -path '*/target/*' \
    -not -path '*/.giant/*' \
    -not -path '*/cache/*' \
    | sort
)

jq -n \
  --argjson files "$(printf '%s\n' "${dockerfiles[@]}" | jq -R . | jq -s .)" \
  --arg registry "$REGISTRY" '
    {
      schema_version: 1,
      targets: ($files | map(
        sub("^\\./"; "") as $rel
        | ($rel | split("/")) as $parts
        | ($parts | length) as $n
        | (if $n >= 2 then $parts[$n - 2] else "root" end) as $service
        | (if $n >= 2 then ($parts[0:$n - 1] | join("/")) else "." end) as $dir
        | {
            id: ("docker:" + $service),
            inputs: [
              $rel,
              ($dir + "/**/*")
            ],
            outputs: [],
            # SAFE PLACEHOLDER for tests - replace with the real docker
            # command in your workspace:
            #   "docker buildx build --push -t " + $registry + "/" + $service + ":$GIANT_CACHE_KEY " + $dir
            command: ("echo would-build " + $service + " :$GIANT_CACHE_KEY"),
            # SAFE PLACEHOLDER - replace with:
            #   "docker manifest inspect " + $registry + "/" + $service + ":$GIANT_CACHE_KEY"
            exists: "false"
          }
      ))
    }'
