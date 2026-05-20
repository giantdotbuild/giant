{pkgs, ...}: {
  env.GREET = "giant2";

  packages = [
    pkgs.git
    pkgs.jujutsu
    pkgs.b3sum
    pkgs.ripgrep
    pkgs.jq
    pkgs.yamlfmt
    # For sandboxing experiments later
    pkgs.bubblewrap
    # Cross-compilation toolchain for musl static builds
    pkgs.pkgsCross.musl64.stdenv.cc
  ];

  languages.rust = {
    enable = true;
    channel = "stable";
    targets = [
      "x86_64-unknown-linux-musl"
    ];
  };

  # Go is available for discovery-script testing in fixtures.
  languages.go.enable = true;

  # Node is used to build the docs site (docs-site/, Astro + Starlight).
  # Doesn't affect the giant binary at all - pure static-site generator.
  languages.javascript = {
    enable = true;
    npm.enable = true;
  };

  enterShell = ''
    export PATH="$DEVENV_ROOT"/bin:$PATH
  '';

  enterTest = ''
    echo "Running tests"
    cargo test
  '';
}
