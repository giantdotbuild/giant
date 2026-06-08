{
  description = "Giant - build orchestration with shared caching for monorepos";

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
        # drop the Starlark std collection (`std/*.star`) that giant-gen ships
        # (ADR-0031) and the `.star` fixtures its tests read. Keep `.star` files.
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
                  homepage = "https://github.com/johnae/giant";
                  license = pkgs.lib.licenses.mit;
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

        # giant-gen ships the official Starlark std collection as files (not
        # embedded in the binary; ADR-0031). The library lands in
        # share/giant/std and GIANT_STD points the `@std//` loader and
        # `giant gen vendor` at it.
        giant-gen =
          let
            pkg = craneLib.buildPackage (
              commonArgs
              // {
                inherit cargoArtifacts;
                pname = "giant-gen";
                cargoExtraArgs = "--locked -p giant-gen";
                doCheck = false;
                postInstall = ''
                  mkdir -p $out/share/giant
                  cp -r std $out/share/giant/std
                '';
                meta = {
                  description = "Part of the Giant build-orchestration suite";
                  homepage = "https://github.com/johnae/giant";
                  license = pkgs.lib.licenses.mit;
                  mainProgram = "giant-gen";
                };
              }
            );
          in
          pkgs.symlinkJoin {
            name = "giant-gen-with-std";
            paths = [ pkg ];
            nativeBuildInputs = [ pkgs.makeWrapper ];
            postBuild = ''
              wrapProgram $out/bin/giant-gen --set GIANT_STD ${pkg}/share/giant/std
            '';
            meta = pkg.meta // {
              mainProgram = "giant-gen";
            };
          };

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
          ];
          meta = {
            description = "Giant + every first-party porcelain (task, tui, gen, sandbox, graph, affected, clean, logs, explain)";
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
    );
}
