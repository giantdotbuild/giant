---
title: Install
description: Install Giant - pre-built binaries, source builds, package managers.
---

Giant ships as a small suite of static binaries: the `giant` engine plus
the porcelains it dispatches to (`giant-build`, `giant-task`, `giant-tui`,
and friends). No runtime dependencies, no daemon, no JVM. Pick whichever
installation method fits your environment.

## Pre-built binaries (recommended)

The install script detects your OS and architecture, downloads the
matching release tarball from GitHub, and installs the suite into
`/usr/local/bin` (or `~/.local/bin` if you don't have root):

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
| Linux x86_64 (musl, static) | `x86_64-unknown-linux-musl` |
| Linux aarch64 (musl, static) | `aarch64-unknown-linux-musl` |
| macOS x86_64 | `x86_64-apple-darwin` |
| macOS aarch64 (Apple Silicon) | `aarch64-apple-darwin` |

Each release ships a `SHA256SUMS` file and the install script verifies
the tarball against it. To check by hand:

```bash
curl -fsSL https://github.com/giantdotbuild/giant/releases/latest/download/SHA256SUMS
```

## From source

The engine alone can't do much - `giant build` dispatches to the
`giant-build` porcelain on PATH - so install at least the engine and the
build porcelain:

```bash
git clone https://github.com/giantdotbuild/giant
cd giant
cargo install --path crates/giant        # the engine
cargo install --path crates/giant-build  # giant build / test / verify

# Add the other porcelains you want (each becomes `giant <name>`):
cargo install --path crates/giant-task
cargo install --path crates/giant-gen
cargo install --path crates/giant-tui

# Engine with the remote-cache feature (Bazel HTTP cache protocol)
cargo install --path crates/giant --features remote
```

Requires Rust 1.95 or newer.

## Nix flake

The repo ships a flake exposing every first-party binary as its own
package, plus a `giant-suite` meta-package that bundles all of them.
CI pushes every build to the [`giant` Cachix
cache](https://giant.cachix.org) and the flake advertises it, so when
nix asks whether to trust the substituter, saying yes gets you prebuilt
binaries instead of a compile (Linux and macOS, x86_64 and aarch64 on
Linux, aarch64 on macOS):

```bash
# Everything (the giant-suite meta-package: all first-party binaries)
nix profile install github:giantdotbuild/giant

# Or pick individual binaries
nix profile install github:giantdotbuild/giant#giant
nix profile install github:giantdotbuild/giant#giant-tui

# Run without installing
nix run github:giantdotbuild/giant -- build
nix run github:giantdotbuild/giant#giant-tui

# Build locally and inspect the result
git clone https://github.com/giantdotbuild/giant
cd giant
nix build .#giant-suite
./result/bin/giant --version
```

The flake doesn't replace `devenv.nix` - devenv stays the dev shell
(`devenv shell` for the full toolchain). The flake's `devShells.default`
is a thin fallback for people who don't run devenv.

### The binary cache

CI builds every commit on `main` for x86_64/aarch64 Linux and aarch64
macOS and pushes the results to
[`giant.cachix.org`](https://giant.cachix.org), so nix installs
substitute prebuilt binaries instead of compiling.

The flake advertises the cache, so plain flake commands just need a yes
at the trust prompt (or `--accept-flake-config` in scripts):

```console
$ nix profile install github:giantdotbuild/giant
do you want to allow configuration setting 'extra-substituters'
to be set to 'https://giant.cachix.org' (y/N)? y
```

To trust it permanently (no prompts, works for transitive use too):

```bash
cachix use giant
# or by hand, in nix.conf:
#   extra-substituters = https://giant.cachix.org
#   extra-trusted-public-keys = giant.cachix.org-1:v3xudJPm6zp3waq/lTUVqKBwm+BWzbs3aVZopsD4QM4=
```

### In a devenv project

Add the flake as an input, put the suite in your packages, and pull the
cache - devenv has cachix support built in:

```yaml
# devenv.yaml
inputs:
  giant:
    url: github:giantdotbuild/giant
```

```nix
# devenv.nix
{ inputs, pkgs, ... }:
{
  packages = [ inputs.giant.packages.${pkgs.stdenv.system}.giant-suite ];
  cachix.pull = [ "giant" ];
}
```

Every `devenv shell` then has the whole suite on PATH, prebuilt. The
flake also exposes `overlays.default` for setups that prefer
`pkgs.giant` / `pkgs.giant-suite` via an overlay.

## Verify the install

```console
$ giant --version
giant 0.1.0

$ giant --help
```

## Uninstall

Remove the binaries wherever your install method placed them:

```bash
(cd "$(dirname "$(which giant)")" && rm giant giant-*)
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
