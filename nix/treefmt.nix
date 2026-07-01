{ ... }:
{
  projectRootFile = "flake.nix";

  programs = {
    nixfmt.enable = true;
    rustfmt = {
      enable = true;
      # match the crates' edition so treefmt's rustfmt sorts imports the same
      # way `cargo fmt` does (otherwise they fight over import order).
      edition = "2021";
    };
  };

  settings.global.excludes = [
    "*.lock"
    ".envrc"
    "LICENSE*"
    "result"
    "*.jsonl"
  ];
}
