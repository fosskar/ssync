{
  mkShell,
  rust-analyzer,
  rustfmt,
  clippy,
  cargo,
  rustc,
  pkg-config,
  age,
}:
# Plain toolchain shell (not the package derivation, whose build env leaks vars
# like `version` into the shell). `age` is needed by ssync-crypto's tests.
mkShell {
  packages = [
    cargo
    rustc
    clippy
    rustfmt
    rust-analyzer
    pkg-config
    age
  ];
}
