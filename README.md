# Loft

Loft is a lightweight, client-only Nix binary cache uploader designed for S3-compatible storage like Garage or MinIO. The name "Loft" is a nod to its inspiration, [Attic](https://github.com/zhaofengli/attic), and its primary backend target, [Garage](https://garage.deuxfleurs.fr/). It sits somewhere in between—a cozy loft between the attic and the garage.

While Attic is a fantastic, feature-rich solution, it requires a server-client setup that may be more than what's needed for simpler use cases. Loft fills a specific gap: providing the convenience of a client-side helper with some of Attic’s best features (like cache checking, native Nix bindings, and watching the Nix store) without the overhead of deploying and managing a server, users, and permissions.

It's designed for scenarios where you want more than just a raw S3 bucket but don't need a full-scale cache server. Think of it as the perfect tool for:

*   **CI/CD pipelines**: Quickly and efficiently pushing build artifacts to a cache.
*   **Single-user setups**: A simple way to manage your own binary cache.
*   **Homelabs**: An easy-to-deploy cache for your local network.

If you're looking for a straightforward, no-fuss way to manage a Nix cache on S3, Loft is for you.

## Features

*   Direct S3 Upload: Uploads NARs directly to your S3 bucket.
*   Nix Store Watcher: Watches the /nix/store for new paths and automatically uploads them.
*   Multi-threaded Uploads: Uploads multiple NARs in parallel to speed up the process.
*   Closure Deduplication: Before uploading, it checks which paths in a closure already exist in the cache to avoid redundant work.
*   **Initial Scan on Startup**: Optionally scans existing Nix store paths on startup and uploads them if they are missing from the cache. This scan runs only once.
*   **Automatic Retries for Failed Uploads**: Automatically retries failed uploads (e.g., due to transient network issues) a few times before giving up.
*   **Nix Store Path Signing**: Supports signing uploaded Nix store paths with a provided Nix signing key, ensuring authenticity and integrity.


## NixOS Integration

Loft provides a NixOS module to simplify configuration and deployment.

### 1. Add Loft to your Flake

First, add the Loft flake to the inputs of your system's flake.nix.

```nix
# flake.nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Add the loft flake
    loft.url = "github:projectinitiative/loft";
  };

  outputs = { self, nixpkgs, loft, ... }: {
    nixosConfigurations.my-machine = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./configuration.nix
        # Simply import the loft module. It handles the rest.
        loft.nixosModules.loft
      ];
    };
  };
}
```

### 2. Configure the Loft Service

In your `configuration.nix` (or a related file), you can now enable and configure the service.

```nix
# configuration.nix
{ pkgs, ... }:

{
  services.loft = {
    enable = true;
    package = pkgs.loft; # Or your own overlay package

    # --- S3 Configuration ---
    s3 = {
      bucket = "nix-cache";
      region = "us-east-1";
      endpoint = "http://172.16.1.50:31292"; # Or your S3 endpoint

      # It's highly recommended to use sops-nix or agenix for secrets
      accessKeyFile = "/path/to/your/s3-access-key";
      secretKeyFile = "/path/to/your/s3-secret-key";
    };

    # --- Loft Service Configuration ---
    debug = false; # Enable debug logging
    localCachePath = "/var/lib/loft/cache.db";
    uploadThreads = 12;
    scanOnStartup = true;
    populateCacheOnStartup = false; # Populate local cache from S3 on startup
    compression = "zstd"; # "zstd" or "xz"

    # --- Large NAR Handling ---
    useDiskForLargeNars = true;
    largeNarThresholdMb = 1024;

    # --- Path Signing ---
    signingKeyPath = "/path/to/your/nix-private-key";
    signingKeyName = "nix-cache";
    skipSignedByKeys = [
      "cache.nixos.org-1"
      "nix-community.cachix.org-1"
    ];

    # --- Pruning Configuration ---
    pruning = {
      enable = false;
      schedule = "00:00"; # Run pruning daily at midnight
      retentionDays = 30;
      # The following are optional:
      # maxSizeGb = 1000;
      # targetPercentage = 80;
    };

    # Use extraConfig for new or unlisted options to prevent module errors
    extraConfig = {
      loft = {
        # This will be merged into the final toml
        some_new_feature_flag = true;
      };
    };
  };
}
```

### 3. Important Notes on Configuration

When using the NixOS module, please be aware of the following:

*   **`localCachePath` Location:** Due to the security sandboxing of the systemd service, the `localCachePath` must be located within the `/var/lib/loft` directory. The service does not have permission to write to other locations on the filesystem. The default path is `/var/lib/loft/cache.db`, which is the recommended setting.

## Configuration

Loft is configured via a `loft.toml` file. The NixOS module generates this file for you. For other systems, you may need to create it manually.

Here's an example `loft.toml` that reflects the available options:

```toml
[s3]
bucket = "nix-cache"
region = "us-east-1"
endpoint = "http://172.16.1.50:31292"

[loft]
upload_threads = 12
scan_on_startup = true
local_cache_path = ".direnv/cache.db"
compression = "zstd"
use_disk_for_large_nars = true
large_nar_threshold_mb = 1024
signing_key_path = "/run/secrets/nix-signing-key"
signing_key_name = "nix-cache"
skip_signed_by_keys = ["cache.nixos.org-1", "nix-community.cachix.org-1"]

# Optional: Pruning configuration
prune_enabled = false
prune_retention_days = 30
# prune_max_size_gb = 1000
# prune_target_percentage = 80
# prune_schedule = "24h"
```

## Command-line arguments

You can also use command-line arguments to override settings or perform one-off actions.

```
Usage: loft [OPTIONS]

Options:
  -c, --config <CONFIG>     Path to the configuration file [default: loft.toml]
      --debug               Enable debug logging
      --clear-cache         Clear the local cache
      --reset-initial-scan  Reset the initial scan complete flag
      --force-scan          Force a full scan, bypassing the local cache
      --populate-cache      Populate the local cache from S3
      --upload-path <PATH>  Manually upload a specific Nix store path
      --prune               Manually trigger pruning of old objects
  -h, --help                Print help
  -V, --version             Print version
```
