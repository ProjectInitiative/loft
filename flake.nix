{
  description = "A minimal Nix binary cache uploader for S3-compatible storage";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
    devenv.url = "github:cachix/devenv";
    devenv.inputs.nixpkgs.follows = "nixpkgs";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    nix2container.url = "github:nlewo/nix2container";
    nix2container.inputs.nixpkgs.follows = "nixpkgs";
    mk-shell-bin.url = "github:rrbutani/nix-mk-shell-bin";
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [
        inputs.devenv.flakeModule
      ];

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      perSystem =
        {
          config,
          self',
          pkgs,
          system,
          ...
        }:
        let
          overlays = [ (import inputs.rust-overlay) ];
          pkgsWithOverlays = import inputs.nixpkgs {
            inherit system overlays;
          };

          loft = config.devenv.shells.default.outputs.loft;
          cache-test = config.devenv.shells.default.outputs.cache-test;

          pkgsForTest = import inputs.nixpkgs {
            inherit system;
            overlays = overlays ++ [ (final: prev: { inherit loft; }) ];
          };
        in
        {
          packages = {
            default = loft;
            inherit cache-test;
          };

          devenv.shells.default = {
            imports = [ ./devenv.nix ];
            packages = [
              config.packages.default
              config.packages.cache-test
            ];
            devenv.root = toString ./.;
          };

          checks = {
            clippy = loft.overrideAttrs (
              final: prev: {
                pname = "${prev.pname}-clippy";
                nativeBuildInputs = prev.nativeBuildInputs or [ ] ++ [ pkgs.clippy ];
                buildPhase = "cargo clippy --all-targets -- --deny warnings";
                doInstall = false;
                installPhase = "mkdir -p $out";
              }
            );
            unit-tests = loft.overrideAttrs (
              final: prev: {
                pname = "${prev.pname}-test";
                doCheck = true;
                installPhase = "mkdir -p $out";
                checkPhase = "cargo test";
              }
            );
            integration = pkgsForTest.testers.nixosTest (import ./nixos/tests/integration.nix);
          };

          formatter = pkgsWithOverlays.nixfmt;
        };

      flake = {
        nixosModules.loft = {
          imports = [
            (
              { pkgs, ... }:
              {
                nixpkgs.overlays = [ inputs.self.overlays.default ];
              }
            )
            (import ./nixos/module.nix)
          ];
        };

        overlays.default = final: prev: {
          loft = inputs.self.packages.${prev.system}.default;
          cache-test = inputs.self.packages.${prev.system}.cache-test;
        };
      };
    };
}
