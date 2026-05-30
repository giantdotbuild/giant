---
title: Install
description: Install Giant - pre-built binaries, source builds, package managers.
---

Giant is a single static binary. No runtime dependencies, no daemon, no
JVM. Pick whichever installation method fits your environment.

## Pre-built binary (recommended)

The install script detects your OS and architecture, downloads the
matching release binary from GitHub, and drops it in `/usr/local/bin`
(or `~/.local/bin` if you don't have root):

```bash
curl -fsSL https://giant.build/install.sh | sh
```

Want a specific version? Pass `GIANT_VERSION`:

```bash
curl -fsSL https://giant.build/install.sh | GIANT_VERSION=0.1.0 sh
```

Supported platforms:

| Platform | Triple |
|---|---|
| Linux x86_64 (glibc) | `x86_64-unknown-linux-gnu` |
| Linux x86_64 (musl, static) | `x86_64-unknown-linux-musl` |
| Linux aarch64 | `aarch64-unknown-linux-gnu` |
| macOS x86_64 | `x86_64-apple-darwin` |
| macOS aarch64 (Apple Silicon) | `aarch64-apple-darwin` |

Binaries are signed and shipped with SHA-256 checksums. Verify:

```bash
curl -fsSL https://giant.build/releases/0.1.0/SHA256SUMS | sha256sum -c -
```

## From source

```bash
git clone https://github.com/johnae/giant
cd giant
cargo install --path crates/giant

# With the remote-cache feature (Bazel HTTP cache protocol)
cargo install --path crates/giant --features remote
```

Requires Rust 1.95 or newer.

## Nix flake

The repo ships a flake exposing every first-party binary as its own
package, plus a `giant-suite` meta-package that bundles all of them:

```bash
# Everything (giant + giant-task + giant-tui)
nix profile install github:johnae/giant

# Or pick individual binaries
nix profile install github:johnae/giant#giant
nix profile install github:johnae/giant#giant-tui

# Run without installing
nix run github:johnae/giant -- build
nix run github:johnae/giant#giant-tui

# Build locally and inspect the result
git clone https://github.com/johnae/giant
cd giant
nix build .#giant-suite
./result/bin/giant --version
```

The flake doesn't replace `devenv.nix` - devenv stays the dev shell
(`devenv shell` for the full toolchain). The flake's `devShells.default`
is a thin fallback for people who don't run devenv.

## Verify the install

```console
$ giant --version
giant 0.1.0

$ giant --help
```

## Uninstall

Remove the binary wherever your install method placed it:

```bash
rm "$(which giant)"
```

Clear the cache too if you don't want it lingering:

```bash
giant clean -y
# or
rm -rf ~/.cache/giant
```

## Shell completions

```bash
# bash
giant completions bash > /etc/bash_completion.d/giant

# zsh
giant completions zsh > "${fpath[1]}/_giant"

# fish
giant completions fish > ~/.config/fish/completions/giant.fish
```
