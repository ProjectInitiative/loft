{ pkgs, ... }:
{
  cachix.enable = false;

  languages.rust = {
    enable = true;
    channel = "stable";
    components = [
      "rustc"
      "cargo"
      "clippy"
      "rustfmt"
      "rust-analyzer"
    ];
  };

  packages = with pkgs; [
    awscli2
    rclone
    nix-output-monitor
  ];

  env.PKG_CONFIG_PATH = "${pkgs.nix.dev}/lib/pkgconfig";
  env.LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

  enterShell = ''
    if [ -f ./.env ]; then
      set -a
      source ./.env
      set +a
    fi

    export RCLONE_CONFIG_DIR="$(pwd)/.direnv/rclone"
    mkdir -p "$RCLONE_CONFIG_DIR"
    {
      echo "[s3]"
      echo "type = s3"
      echo "access_key_id = $AWS_ACCESS_KEY_ID"
      echo "secret_access_key = $AWS_SECRET_ACCESS_KEY"
      if [ -n "$S3_ENDPOINT" ]; then
        echo "provider = Other"
      else
        echo "provider = AWS"
      fi
      echo "s3_force_path_style = true"
      if [ -n "$S3_ENDPOINT" ]; then
        echo "endpoint = $S3_ENDPOINT"
      fi
      if [ -n "$S3_BUCKET" ]; then
        echo "bucket_name = $S3_BUCKET"
      fi
    } > "$RCLONE_CONFIG_DIR/rclone.conf"

    {
      echo "[s3-test]"
      echo "type = s3"
      echo "access_key_id = $AWS_ACCESS_KEY_ID"
      echo "secret_access_key = $AWS_SECRET_ACCESS_KEY"
      if [ -n "$S3_ENDPOINT" ]; then
        echo "provider = Other"
      else
        echo "provider = AWS"
      fi
      echo "s3_force_path_style = true"
      if [ -n "$S3_ENDPOINT" ]; then
        echo "endpoint = $S3_ENDPOINT"
      fi
      echo "bucket_name = nix-cache-test"
    } >> "$RCLONE_CONFIG_DIR/rclone.conf"

    export RCLONE_CONFIG="$RCLONE_CONFIG_DIR/rclone.conf"
    echo "rclone config generated at $RCLONE_CONFIG"

    export LOFT_CONFIG_DIR="$(pwd)/.direnv/loft"
    mkdir -p "$LOFT_CONFIG_DIR"
    {
      echo "[s3]"
      echo "bucket = \"$S3_BUCKET\""
      echo "region = \"$S3_REGION\""
      echo "endpoint = \"$S3_ENDPOINT\""
      echo "access_key = \"$AWS_ACCESS_KEY_ID\""
      echo "secret_key = \"$AWS_SECRET_ACCESS_KEY\""
      echo "[loft]"
      echo "upload_threads = $LOFT_UPLOAD_THREADS"
      echo "scan_on_startup = $LOFT_SCAN_ON_STARTUP"
      echo "local_cache_path = \".direnv/cache.db\""
      echo "compression = \"zstd\""
      if [ -n "$NIX_SIGNING_KEY_PATH" ]; then
        echo "signing_key_path = \"$NIX_SIGNING_KEY_PATH\""
      fi
      if [ -n "$NIX_SIGNING_KEY_NAME" ]; then
        echo "signing_key_name = \"$NIX_SIGNING_KEY_NAME\""
      fi
      if [ -n "$LOFT_SKIP_SIGNED_BY_KEYS" ]; then
        echo -n "skip_signed_by_keys = ["
        IFS=',' read -ra KEYS <<< "$LOFT_SKIP_SIGNED_BY_KEYS"
        first=true
        for key in "''${KEYS[@]}"; do
          if [ "$first" = true ]; then
            first=false
          else
            echo -n ", "
          fi
          echo -n "\"$key\""
        done
        echo "]"
      fi
    } > "$LOFT_CONFIG_DIR/loft.toml"

    export LOFT_CONFIG="$LOFT_CONFIG_DIR/loft.toml"
    echo "loft.toml generated at $LOFT_CONFIG"

    echo ""
    echo "🧪 Cache testing script available! Run: cache-test"
    echo "   This will build loft with nom, then clean up all artifacts."
    echo ""
    echo "🧪 Run all checks (integration, clippy, unit-tests):"
    echo "   nix flake check"
    echo ""
    echo "   Or individually:"
    echo "   nix build .#checks.${pkgs.system}.integration"
    echo "   nix build .#checks.${pkgs.system}.clippy"
    echo "   nix build .#checks.${pkgs.system}.unit-tests"
    echo ""
    echo "   Use --rebuild to re-run a cached result (e.g. nix build .#checks.${pkgs.system}.integration --rebuild)"
  '';

  git-hooks.hooks = {
    nixfmt.enable = true;
    rustfmt.enable = true;
  };

  enterTest = ''
    cargo test
  '';
}
