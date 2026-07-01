{
  rustPlatform,
  age,
  makeWrapper,
  lib,
}:
rustPlatform.buildRustPackage {
  pname = "ssync";
  version = "0.0.1";

  src = ../.;
  cargoLock.lockFile = ../Cargo.lock;

  # ssync-crypto shells out to the `age` CLI (for post-quantum hybrid keys), both
  # in its tests (checkPhase) and at runtime.
  nativeBuildInputs = [ makeWrapper ];
  nativeCheckInputs = [ age ];

  postInstall = ''
    wrapProgram $out/bin/ssync --prefix PATH : ${lib.makeBinPath [ age ]}
  '';
}
