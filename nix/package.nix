{
  lib,
  rustPlatform,
  pkg-config,
  openssl,
}:

rustPlatform.buildRustPackage {
  pname = "koerier-wallet";
  version = (lib.importTOML ../Cargo.toml).package.version;
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
      ../migrations
      ../assets
      ../example
      ../LICENSE-MIT
      ../LICENSE-APACHE
      ../README.md
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];

  meta = {
    description = "Multi-account Lightning wallet and Lightning Address server for test networks only";
    homepage = "https://github.com/tee8z/koerier-wallet";
    license = with lib.licenses; [
      mit
      asl20
    ];
    mainProgram = "koerier-wallet";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
  };
}
