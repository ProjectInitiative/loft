{ pkgs }:
pkgs.writeShellScriptBin "cache-test" ''
  #!/usr/bin/env bash
  set -euo pipefail

  echo "🧪 Testing cache behavior for loft..."
  echo "Building with nom (no symlinks)..."

  PATHS=$(${pkgs.nix-output-monitor}/bin/nom build .#default --print-out-paths --no-link 2>&1 | tee /dev/stderr | tail -1)

  STORE_PATHS=$(echo "$PATHS" | grep -o '/nix/store/[^[:space:]]*' || echo "$PATHS")

  echo "📦 Built store paths:"
  declare -a PATH_ARRAY
  while IFS= read -r path; do
    if [[ -n "$path" ]]; then
      echo "  $path"
      PATH_ARRAY+=("$path")
    fi
  done <<< "$STORE_PATHS"

  echo ""
  echo "🚀 Using fresh loft to upload itself to cache..."

  for path in "''${PATH_ARRAY[@]}"; do
    if [[ "$path" =~ ^/nix/store/ ]]; then
      echo "📤 Uploading to cache: $path"

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

      if "$LOFT_BIN" --config .direnv/loft/loft.toml --upload-path "$path"; then
        echo "✅ Successfully uploaded: $path"
      else
        echo "❌ Failed to upload: $path"
        exit 1
      fi
    fi
  done

  echo "✨ Cache test complete!"
  echo ""
  echo "💡 To test cache hit, run this script again - it should pull from your cache!"
''
