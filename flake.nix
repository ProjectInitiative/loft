{
  description = "A minimal Nix binary cache uploader for S3-compatible storage";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        # Import the crane library
        craneLib = (crane.mkLib pkgs).overrideToolchain (
          pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml
        );

        # Build the application using the logic from crane.nix
        loft = import ./crane.nix {
          inherit pkgs craneLib;
          src = ./.;
        };
      in
      {
        packages = {
          default = loft;
        };

        devShells = {
          default = craneLib.devShell {
            # Additional development tools
            packages = with pkgs; [
              # For interacting with Garage S3
              awscli2
              # For interacting with the Nix store
              nix
            ];
          };
        };

        apps.default = flake-utils.lib.mkApp {
          drv = self.packages."${system}".default;
        };

      });
}

