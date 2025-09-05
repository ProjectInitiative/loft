{
  description = "A minimal Nix binary cache uploader for S3-compatible storage";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
    attic-flake.url = "github:zhaofengli/attic";
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, crane, attic-flake }@inputs:
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
          attic = attic-flake.packages.${system}.default;
        };
      in
      {
        packages = {
          default = loft;
        };

        devShells = {
          default = craneLib.devShell {
            inputsFrom = [ attic-flake.devShells.${system}.default ];
            # Additional development tools
            packages = with pkgs; [
              rust-analyzer
              # For interacting with Garage S3
              awscli2
              # For interacting with the Nix store
              nix
              # For interacting with S3-compatible storage (e.g., MinIO, Garage)
              # Remember to configure rclone (e.g., rclone config) for your S3 bucket.
              rclone
              # For openssl-sys dependency of reqwest
              pkg-config
              openssl
            ];
            shellHook = ''
              # Source the .env file to make environment variables available
              if [ -f ./.env ]; then
                source ./.env
              fi

              export RCLONE_CONFIG_DIR="$(pwd)/.direnv/rclone"
              mkdir -p "$RCLONE_CONFIG_DIR"
              cat > "$RCLONE_CONFIG_DIR/rclone.conf" << EOF
[s3]
type = s3
access_key_id = $AWS_ACCESS_KEY_ID
secret_access_key = $AWS_SECRET_ACCESS_KEY
EOF
              if [ -n "$S3_ENDPOINT" ]; then
                echo "provider = Other" >> "$RCLONE_CONFIG_DIR/rclone.conf"
              else
                echo "provider = AWS" >> "$RCLONE_CONFIG_DIR/rclone.conf"
              fi
              cat >> "$RCLONE_CONFIG_DIR/rclone.conf" << EOF
s3_force_path_style = true
EOF
              # Add endpoint and bucket if they exist in environment variables
              if [ -n "$S3_ENDPOINT" ]; then
                echo "endpoint = $S3_ENDPOINT" >> "$RCLONE_CONFIG_DIR/rclone.conf"
              fi
              if [ -n "$S3_BUCKET" ]; then
                echo "bucket_name = $S3_BUCKET" >> "$RCLONE_CONFIG_DIR/rclone.conf"
              fi

              cat >> "$RCLONE_CONFIG_DIR/rclone.conf" << EOF
[s3-test]
type = s3
access_key_id = $AWS_ACCESS_KEY_ID
secret_access_key = $AWS_SECRET_ACCESS_KEY
EOF
              if [ -n "$S3_ENDPOINT" ]; then
                echo "provider = Other" >> "$RCLONE_CONFIG_DIR/rclone.conf"
              else
                echo "provider = AWS" >> "$RCLONE_CONFIG_DIR/rclone.conf"
              fi
              cat >> "$RCLONE_CONFIG_DIR/rclone.conf" << EOF
s3_force_path_style = true
EOF
              # Add endpoint and bucket if they exist in environment variables
              if [ -n "$S3_ENDPOINT" ]; then
                echo "endpoint = $S3_ENDPOINT" >> "$RCLONE_CONFIG_DIR/rclone.conf"
              fi
              echo "bucket_name = nix-cache-test" >> "$RCLONE_CONFIG_DIR/rclone.conf"

              export RCLONE_CONFIG="$RCLONE_CONFIG_DIR/rclone.conf"
              echo "rclone config generated at $RCLONE_CONFIG"

              # Generate loft.toml
              export LOFT_CONFIG_DIR="$(pwd)/.direnv/loft"
              mkdir -p "$LOFT_CONFIG_DIR"

              # Expand variables with defaults

              cat > "$LOFT_CONFIG_DIR/loft.toml" << EOF
[s3]
bucket = "$S3_BUCKET"
region = "$S3_REGION"
endpoint = "$S3_ENDPOINT"
access_key = "$AWS_ACCESS_KEY_ID"
secret_key = "$AWS_SECRET_ACCESS_KEY"

[loft]
upload_threads = $LOFT_UPLOAD_THREADS
scan_on_startup = $LOFT_SCAN_ON_STARTUP
EOF
              if [ -n "$NIX_SIGNING_KEY_PATH" ]; then
                echo "signing_key_path = \"$NIX_SIGNING_KEY_PATH\"" >> "$LOFT_CONFIG_DIR/loft.toml"
              fi
              if [ -n "$NIX_SIGNING_KEY_NAME" ]; then
                echo "signing_key_name = \"$NIX_SIGNING_KEY_NAME\"" >> "$LOFT_CONFIG_DIR/loft.toml"
              fi

              if [ -n "$LOFT_SKIP_SIGNED_BY_KEYS" ]; then
                IFS=',' read -ra ADDR <<< "$LOFT_SKIP_SIGNED_BY_KEYS"
                printf 'skip_signed_by_keys = [' >> "$LOFT_CONFIG_DIR/loft.toml"
                for i in "''${ADDR[@]}"; do
                  printf '"%s",' "$i" >> "$LOFT_CONFIG_DIR/loft.toml"
                done
                # Remove trailing comma and close array
                sed -i 's/,$//' "$LOFT_CONFIG_DIR/loft.toml"
                printf ']\n' >> "$LOFT_CONFIG_DIR/loft.toml"
              fi

              export LOFT_CONFIG="$LOFT_CONFIG_DIR/loft.toml"
              echo "loft.toml generated at $LOFT_CONFIG"
            '';
          };
        };

        apps.default = flake-utils.lib.mkApp {
          drv = self.packages."${system}".default;
        };

      });
}

