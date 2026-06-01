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

  nixConfig = {
    extra-trusted-public-keys = "devenv.cachix.org-1:w1cLUi8dv3hnoSPGAuibQv+f9TZLr6cv/Hm9XgU50cw=";
    extra-substituters = "https://devenv.cachix.org";
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
        { config, self', inputs', pkgs, system, ... }:
        let
          overlays = [ (import inputs.rust-overlay) ];
          pkgsWithOverlays = import inputs.nixpkgs {
            inherit system overlays;
          };

          loft = pkgsWithOverlays.rustPlatform.buildRustPackage {
            pname = "loft";
            version = "0.3.2";
            src = pkgsWithOverlays.lib.cleanSourceWith {
              src = ./.;
              filter = path: type:
                let
                  relPath = pkgsWithOverlays.lib.removePrefix (toString ./.) (toString path);
                in
                !(pkgsWithOverlays.lib.hasPrefix "/target" relPath)
                && !(pkgsWithOverlays.lib.hasPrefix "/.direnv" relPath)
                && !(pkgsWithOverlays.lib.hasPrefix "/.devenv" relPath)
                && !(pkgsWithOverlays.lib.hasPrefix "/vendor" relPath);
            };

            nativeBuildInputs = with pkgsWithOverlays; [
              pkg-config
              clang
              llvmPackages.libclang.lib
            ];

            buildInputs = with pkgsWithOverlays; [
              nix
              nix.dev
              openssl
            ];

            PKG_CONFIG_PATH = "${pkgsWithOverlays.nix.dev}/lib/pkgconfig";
            LIBCLANG_PATH = "${pkgsWithOverlays.llvmPackages.libclang.lib}/lib";

            cargoHash = "sha256-TG3RHFyyGpRa8/9J5KOzI/hd2GyiKb01p/NOpIv3VxI=";
          };

          cache-test = import ./nix/cache-test.nix { pkgs = pkgsWithOverlays; };

          pkgsForTest = import inputs.nixpkgs {
            inherit system;
            overlays = [
              (import inputs.rust-overlay)
              (final: prev: { loft = loft; })
            ];
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
            clippy = loft.overrideAttrs (final: prev: {
              pname = "${prev.pname}-clippy";
              buildPhase = "cargo clippy --all-targets -- --deny warnings";
              doInstall = false;
              installPhase = "mkdir -p $out";
            });
            unit-tests = loft.overrideAttrs (final: prev: {
              pname = "${prev.pname}-test";
              doCheck = true;
              installPhase = "mkdir -p $out";
              checkPhase = "cargo test";
            });
            integration = pkgsForTest.testers.nixosTest (import ./nixos/tests/integration.nix);
          };

          formatter = pkgsWithOverlays.nixfmt;
        };

      flake = {
        nixosModules.loft = {
          imports = [
            ({ pkgs, ... }: {
              nixpkgs.overlays = [ inputs.self.overlays.default ];
            })
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
