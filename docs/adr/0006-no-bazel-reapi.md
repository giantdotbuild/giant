# ADR-0006 - No Bazel REAPI gRPC; use Bazel HTTP cache protocol instead

- **Status**: Accepted
- **Date**: 2026-05-20

> **Revised** 2026-05-20: changed from "small bespoke HTTP CAS" to
> "Bazel HTTP cache protocol". Research showed that Bazel publishes a
> stable, simple HTTP protocol (`GET/PUT /ac/<hash>` and `/cas/<hash>`)
> that bazel-remote, sccache-style backends, and any S3/MinIO/nginx
> setup can serve. Adopting the published protocol costs roughly the
> same as our own and buys us bazel-remote compatibility for free.

## Context

A remote cache is one of the most-used features of a content-addressed
build tool. Two well-known protocols cover the space:

- **REAPI (Bazel Remote Execution API v2)** - gRPC-based, used by
  BuildBuddy, bazel-remote, Buildbarn, NativeLink. Comprehensive
  (covers both remote cache and remote execution). Implementation
  requires tonic, prost, hyper-util, rustls, the `bazel-remote-apis`
  crate - tens of transitive deps and a noticeable binary-size hit.
  Cache layout is a Merkle tree of directories, ActionResult ↔ blob
  mapping, batch operations - features designed for remote execution
  that don't match a simple "outputs are files" model.

- **Bazel HTTP cache protocol** - much simpler. Two endpoint
  families:
  - `GET/PUT /ac/<sha256>` for action results (small JSON-like blobs
    describing outputs).
  - `GET/PUT /cas/<sha256>` for content-addressed blobs (the outputs
    themselves).
  - Authentication via standard HTTP (Basic, Bearer, mTLS).

bazel-remote serves both protocols; clients can use either. Other
ecosystems (sccache, Nix binary cache, Turborepo) all converged on
HTTP CAS with content-hash paths.

## Decision

Skip REAPI gRPC. Implement the **Bazel HTTP cache protocol** as
giant's remote cache transport.

- Endpoints: `GET/PUT /ac/<hash>` and `GET/PUT /cas/<hash>`.
- Hash algorithm: **sha256**, matching Bazel's default. Hashes are
  hex-encoded and used directly as URL path components. This gives us
  drop-in compatibility with bazel-remote, sccache backends, and
  `sha256sum`-based shell debugging.
- Auth: HTTP headers (`Authorization: Bearer <token>`,
  `Authorization: Basic ...`). Token sourced from env var or
  `cache.remote.auth_env` field in config.
- Implementation: tiny `reqwest`-based client including retry/backoff
  and concurrent uploads.
- Feature-flagged (`remote`) so default builds don't carry it.

We **do not implement** REAPI gRPC (Action/Directory/Tree wire types).

## Consequences

### Enables

- Core binary stays free of tonic, prost, bazel-remote-apis, and the
  dozens of transitive deps they pull in.
- The cache protocol is well-known and proven; we're not inventing it.
- Compatible with **bazel-remote** out of the box (the most common
  open-source server), and with any HTTP/S3 backend via standard
  reverse proxy / object-storage paths.
- Servers can be trivial - an nginx + filesystem setup, an S3 bucket
  with HTTP-style PUT/GET, MinIO, R2, garage, all work.
- The protocol matches giant's "outputs are files" model: each output
  is one blob, no Merkle directories.

### Costs

- Loss of compatibility with **REAPI-only** servers (BuildBuddy's RBE
  backend, EngFlow, NativeLink, Buildbarn in REAPI-mode). Teams using
  REAPI-only servers can't drop in.

  (bazel-remote *also* speaks HTTP, so teams using bazel-remote
  remain fully compatible.)
- No remote execution. We never had that anyway; HTTP cache protocol
  is cache-only by design.
- Hash algorithm is sha256, matching Bazel - no algorithm mismatch
  with the broader ecosystem.

### What we're committing to maintaining

- Conformance with Bazel's HTTP cache protocol specification.
- A reference test against bazel-remote to catch regressions.
- Documentation of supported server configurations (bazel-remote,
  nginx + filesystem, S3 via shim, MinIO direct).

## Alternatives considered

### Full REAPI gRPC

Maximum compatibility (including REAPI-only RBE servers).

Rejected: the dependency cost and complexity load is large, and most
users of a remote cache run bazel-remote (which speaks HTTP). Paying
for REAPI buys access to a small slice of the ecosystem at a
disproportionate complexity cost.

### REAPI subset

Implement just AC + CAS via gRPC, skip ExecutionAPI.

Rejected: even the AC + CAS subset requires the Merkle tree
representation, Directory + Tree + FileNode wire types, and a
significant chunk of bazel-remote-apis. Doesn't save much, and the
HTTP protocol is simpler still.

### Bespoke HTTP CAS protocol

Roll our own URL scheme and metadata format.

Rejected: same cost as adopting Bazel's HTTP protocol but with no
existing-server compatibility. Strictly worse.

### S3-only via AWS SDK

Bind directly to the AWS S3 Rust SDK.

Rejected: AWS SDK is enormous; the HTTP cache protocol works against
S3 (via path-style URLs), MinIO, R2, garage, plain nginx, and
bazel-remote with no SDK dependency.

### Pure filesystem cache (no remote)

Drop remote cache entirely.

Rejected: remote cache is high-value enough to keep.

### OpenDAL-style abstraction

Use OpenDAL (which Pants and sccache both use) to abstract over
S3/GCS/Azure/WebDAV/GHA-cache.

Deferred. The HTTP cache protocol covers the same backends via
standard reverse-proxy patterns, and OpenDAL adds a dependency we
don't need yet. Revisit if more exotic backends turn out to matter.

## References

- [Bazel HTTP cache protocol](https://bazel.build/remote/caching#http-caching)
- [bazel-remote](https://github.com/buchgr/bazel-remote) - reference server
- [sccache backends](https://github.com/mozilla/sccache/blob/main/docs/Storage.md)
- [TDD-0006 - Remote cache HTTP protocol](../tdd/0006-remote-cache-http-protocol.md)
