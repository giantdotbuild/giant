{
  description = "Giant - build orchestration with shared caching for monorepos";

  # CI pushes every build here; nix offers the substituter to consumers
  # automatically, so installs come prebuilt.
  nixConfig = {
    extra-substituters = [ "https://giant.cachix.org" ];
    extra-trusted-public-keys = [
      "giant.cachix.org-1:v3xudJPm6zp3waq/lTUVqKBwm+BWzbs3aVZopsD4QM4="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
      crane,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        # Match devenv.nix's channel so flake builds and dev shells
        # agree on the toolchain version.
        rustToolchain = pkgs.rust-bin.stable.latest.default;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

        # crane's `cleanCargoSource` keeps only Rust/Cargo files, which would
        # drop the `.star` fixtures giant-gen's tests read. Keep `.star` files.
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          name = "giant-source";
          filter = path: type: (craneLib.filterCargoSources path type) || (pkgs.lib.hasSuffix ".star" path);
        };

        commonArgs = {
          inherit src;
          strictDeps = true;

          # Native deps surfaced by transitive crates. Most of giant
          # is pure Rust; the items here cover crates that wrap a C
          # library (none currently) plus tools cargo invokes for
          # build scripts.
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ ];

          # GIANT_TARGET_TRIPLE is read by crates/giant/build.rs from
          # cargo's TARGET env var - no manual wiring needed inside
          # the build sandbox.
        };

        # Build all dependencies once and reuse the artifact set for
        # every per-binary build. This is what makes per-crate
        # derivations cheap.
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        # One derivation per binary. `cargoExtraArgs = "-p NAME"`
        # selects a single workspace member; the resulting derivation
        # contains exactly that crate's bin output.
        mkBin =
          {
            name,
            runtimeInputs ? [ ],
          }:
          let
            pkg = craneLib.buildPackage (
              commonArgs
              // {
                inherit cargoArtifacts;
                pname = name;
                cargoExtraArgs = "--locked -p ${name}";
                doCheck = false;
                meta = {
                  description = "Part of the Giant build-orchestration suite";
                  homepage = "https://github.com/giantdotbuild/giant";
                  license = pkgs.lib.licenses.asl20;
                  mainProgram = name;
                };
              }
            );
          in
          if runtimeInputs == [ ] then
            pkg
          else
            # Wrap so binaries that shell out to other tools find
            # them via PATH without the user having to install them
            # separately. The original derivation is still in the
            # closure, just wrapped.
            pkgs.symlinkJoin {
              name = "${name}-with-runtime";
              paths = [ pkg ];
              nativeBuildInputs = [ pkgs.makeWrapper ];
              postBuild = ''
                wrapProgram $out/bin/${name} \
                  --prefix PATH : ${pkgs.lib.makeBinPath runtimeInputs}
              '';
              meta = pkg.meta // {
                mainProgram = name;
              };
            };

        giant = mkBin { name = "giant"; };
        giant-task = mkBin { name = "giant-task"; };
        giant-tui = mkBin { name = "giant-tui"; };
        giant-sandbox = mkBin { name = "giant-sandbox"; };
        giant-graph = mkBin { name = "giant-graph"; };
        giant-affected = mkBin { name = "giant-affected"; };
        giant-clean = mkBin { name = "giant-clean"; };
        giant-logs = mkBin { name = "giant-logs"; };
        giant-explain = mkBin { name = "giant-explain"; };
        # One crate, three bins: giant-build, giant-test, giant-verify.
        giant-build = mkBin { name = "giant-build"; };

        # `@std//` modules come from the pinned giant-std repo (or a
        # GIANT_STD / vendored copy); the binary itself ships no data.
        giant-gen = mkBin { name = "giant-gen"; };

        # Meta-package: `nix profile install .` drops all three
        # binaries onto PATH at once. Implementation is a
        # `symlinkJoin`, so each underlying derivation is still
        # available via `nix profile install .#giant` etc.
        giant-suite = pkgs.symlinkJoin {
          name = "giant-suite";
          paths = [
            giant
            giant-task
            giant-tui
            giant-gen
            giant-sandbox
            giant-graph
            giant-affected
            giant-clean
            giant-logs
            giant-explain
            giant-build
          ];
          meta = {
            description = "Giant + every first-party porcelain (task, tui, gen, sandbox, graph, affected, clean, logs, explain, build/test/verify)";
            mainProgram = "giant";
          };
        };
      in
      {
        packages = {
          inherit
            giant
            giant-task
            giant-tui
            giant-gen
            giant-sandbox
            giant-graph
            giant-affected
            giant-clean
            giant-logs
            giant-explain
            giant-build
            giant-suite
            ;
          default = giant-suite;
        };

        apps = {
          giant = flake-utils.lib.mkApp { drv = giant; };
          giant-task = flake-utils.lib.mkApp { drv = giant-task; };
          giant-tui = flake-utils.lib.mkApp { drv = giant-tui; };
          giant-gen = flake-utils.lib.mkApp { drv = giant-gen; };
          giant-sandbox = flake-utils.lib.mkApp { drv = giant-sandbox; };
          giant-graph = flake-utils.lib.mkApp { drv = giant-graph; };
          giant-affected = flake-utils.lib.mkApp { drv = giant-affected; };
          giant-clean = flake-utils.lib.mkApp { drv = giant-clean; };
          giant-logs = flake-utils.lib.mkApp { drv = giant-logs; };
          giant-explain = flake-utils.lib.mkApp { drv = giant-explain; };
          giant-build = flake-utils.lib.mkApp { drv = giant-build; };
          default = flake-utils.lib.mkApp { drv = giant; };
        };

        # A minimal devshell for users who want to develop against
        # giant without devenv. The primary dev environment is still
        # devenv.nix (richer: process-compose, language servers,
        # cross-toolchain). This shell is the no-devenv fallback.
        devShells.default = pkgs.mkShell {
          packages = [
            rustToolchain
            pkgs.pkg-config
            pkgs.git
            pkgs.jujutsu
          ];
        };

        formatter = pkgs.nixfmt-rfc-style;
      }
    )
    // {
      # Overlay for consumers who want `pkgs.giant` / `pkgs.giant-suite`
      # in their own nixpkgs (devenv overlays, NixOS configs).
      overlays.default = final: _prev: {
        giant = self.packages.${final.stdenv.hostPlatform.system}.giant;
        giant-suite = self.packages.${final.stdenv.hostPlatform.system}.giant-suite;
      };
    };
}
