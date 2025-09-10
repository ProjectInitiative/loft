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
        
        # Cache testing script
        cache-test = pkgs.writeShellScriptBin "cache-test" ''
          #!/usr/bin/env bash
          set -euo pipefail
          
          echo "🧪 Testing cache behavior for loft..."
          echo "Building with nom (no symlinks)..."
          
          # Build without creating symlinks and capture the store paths
          PATHS=$(${pkgs.nix-output-monitor}/bin/nom build .#default --print-out-paths --no-link 2>&1 | tee /dev/stderr | tail -1)
          
          # Extract just the store paths (nom adds extra output)
          STORE_PATHS=$(echo "$PATHS" | grep -o '/nix/store/[^[:space:]]*' || echo "$PATHS")
          
          echo "📦 Built store paths:"
          # Convert to array to avoid subshell issues
          declare -a PATH_ARRAY
          while IFS= read -r path; do
            if [[ -n "$path" ]]; then
              echo "  $path"
              PATH_ARRAY+=("$path")
            fi
          done <<< "$STORE_PATHS"
          
          echo ""
          echo "🚀 Using fresh loft to upload itself to cache..."
          
          # Use the freshly built loft to upload each store path
          for path in "''${PATH_ARRAY[@]}"; do
            if [[ "$path" =~ ^/nix/store/ ]]; then
              echo "📤 Uploading to cache: $path"
              
              # Check if the loft binary exists
              LOFT_BIN="$path/bin/loft"
              if [[ ! -f "$LOFT_BIN" ]]; then
                echo "❌ loft binary not found at: $LOFT_BIN"
                echo "   Contents of $path:"
                ls -la "$path" || echo "   Cannot list directory"
                if [[ -d "$path/bin" ]]; then
                  echo "   Contents of $path/bin:"
                  ls -la "$path/bin"
                fi
                exit 1
              fi
              
              # echo "🔍 Using loft binary: $LOFT_BIN"
              # echo "🔍 Checking binary details:"
              # file "$LOFT_BIN" || echo "   file command failed"
              # ldd "$LOFT_BIN" || echo "   ldd failed (static binary?)"
              
              # echo "🔍 Testing direct execution:"
              # if sudo "$LOFT_BIN" --help >/dev/null 2>&1; then
              #   echo "   ✅ Binary executes successfully"
              # else
              #   echo "   ❌ Binary execution failed"
              #   echo "   Trying without sudo:"
              #   if "$LOFT_BIN" --help >/dev/null 2>&1; then
              #     echo "   ✅ Works without sudo"
              #   else
              #     echo "   ❌ Still fails without sudo"
              #   fi
              # fi
              
              if "$LOFT_BIN" --config .direnv/loft/loft.toml --upload-path "$path"; then
                echo "✅ Successfully uploaded: $path"
              else
                echo "❌ Failed to upload: $path"
                exit 1
              fi
            fi
          done
          
          # echo ""
          # echo "🧹 Cleaning up build artifacts..."
          
          # Remove any result symlinks that might exist
          # rm -f result*
          
          # Try to delete the specific store paths
          # for path in "''${PATH_ARRAY[@]}"; do
          #   if [[ "$path" =~ ^/nix/store/ ]]; then
          #     echo "Attempting to delete: $path"
          #     if nix store delete "$path" 2>/dev/null; then
          #       echo "✅ Deleted: $path"
          #     else
          #       echo "⚠️  Could not delete $path (may have references)"
          #       echo "   Checking what keeps it alive:"
          #       nix-store --query --roots "$path" 2>/dev/null || echo "   No roots found"
          #     fi
          #   fi
          # done
          
          # Run garbage collection to clean up any unreferenced paths
          # echo "🗑️  Running garbage collection..."
          # nix-collect-garbage --quiet
          
          echo "✨ Cache test complete!"
          echo ""
          echo "💡 To test cache hit, run this script again - it should pull from your cache!"
        '';
      in
      {
        # The module is now a list containing an inline overlay module and the main module.
        nixosModules.loft = [
          # 1. This small, anonymous module adds the overlay to the user's system.
          ({ pkgs, ... }: {
            nixpkgs.overlays = [ self.overlays.default ];
          })
          # 2. This is your main configuration module from the file above.
          (import ./nixos/module.nix)
        ];
        # Expose the overlay to make the package easily available
        overlays.default = final: prev: {
          loft = self.packages.${prev.system}.default;
          cache-test = cache-test;
        };
        packages = {
          default = loft;
          cache-test = cache-test;
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
              # Add our cache testing script
              cache-test
              # nom for better build output
              nix-output-monitor
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
local_cache_path = ".direnv/cache.db"
compression = "xz"
EOF
              if [ -n "$LOFT_USE_DISK_FOR_LARGE_NARS" ]; then
                echo "use_disk_for_large_nars = $LOFT_USE_DISK_FOR_LARGE_NARS" >> "$LOFT_CONFIG_DIR/loft.toml"
              fi
              if [ -n "$LOFT_LARGE_NAR_THRESHOLD_MB" ]; then
                echo "large_nar_threshold_mb = $LOFT_LARGE_NAR_THRESHOLD_MB" >> "$LOFT_CONFIG_DIR/loft.toml"
              fi
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
              
              echo ""
              echo "🧪 Cache testing script available! Run: cache-test"
              echo "   This will build loft with nom, then clean up all artifacts."
            '';
          };
        };
        apps.default = flake-utils.lib.mkApp {
          drv = self.packages."${system}".default;
        };
        apps.cache-test = flake-utils.lib.mkApp {
          drv = cache-test;
        };
      });
}
