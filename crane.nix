# This file contains the core logic for building the Rust crate with crane.
{ pkgs, craneLib, src }:

craneLib.buildPackage {
  inherit src;

  # Dependencies needed by the crate at build time
  nativeBuildInputs = with pkgs; [
    pkg-config
  ];

  buildInputs = with pkgs; [
    openssl # Common dependency for HTTP clients
  ];

  # You can add tests here if you have them
  # doCheck = true;
  # cargoCheckCommand = ''
  #   cargo test -- --nocapture
  # '';
}

