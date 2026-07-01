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

  # The two-node tests spin up in-process iroh nodes; running them in parallel
  # starves each other and they time out under sandbox/CI load. Serialize the
  # test threads so convergence is reliable.
  dontUseCargoParallelTests = true;
  checkFlags = [ "--test-threads=1" ];

  postInstall = ''
    wrapProgram $out/bin/ssync --prefix PATH : ${lib.makeBinPath [ age ]}
  '';
}
