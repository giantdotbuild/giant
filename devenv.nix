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

  enterShell = ''
    export PATH="$DEVENV_ROOT"/bin:$PATH
  '';

  enterTest = ''
    echo "Running tests"
    cargo test
  '';
}
