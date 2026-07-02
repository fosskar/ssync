_: {
  projectRootFile = "flake.nix";

  programs = {
    nixfmt.enable = true;
    rustfmt = {
      enable = true;
      # match the crates' edition so treefmt's rustfmt sorts imports the same
      # way `cargo fmt` does (otherwise they fight over import order).
      edition = "2024";
    };
    # TOML (Cargo.toml, config examples)
    taplo.enable = true;
    # nix linters: dead code and anti-patterns
    deadnix.enable = true;
    statix.enable = true;
  };

  settings.global.excludes = [
    "*.lock"
    ".envrc"
    "LICENSE*"
    "result"
    "*.jsonl"
  ];
}
