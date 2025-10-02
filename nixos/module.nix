# nixos/module.nix
{ config, lib, pkgs, ... }:

with lib;

let
  cfg = config.services.loft;
  loft-pkg = cfg.package;

  # Use Nixpkgs' toml format for clean generation
  tomlFormat = pkgs.formats.toml { };

  # Define the base configuration from the explicit options.
  # Secrets are handled separately by the systemd service.
  baseConfig = {
    s3 = {
      inherit (cfg.s3) bucket region endpoint;
    };
    loft = {
      upload_threads = cfg.uploadThreads;
      scan_on_startup = cfg.scanOnStartup;
      populate_cache_on_startup = cfg.populateCacheOnStartup;
      use_disk_for_large_nars = cfg.useDiskForLargeNars;
      large_nar_threshold_mb = cfg.largeNarThresholdMb;
      compression = cfg.compression;
      local_cache_path = cfg.localCachePath;

      # Pruning options
      prune_enabled = cfg.pruning.enable;
      prune_retention_days = cfg.pruning.retentionDays;
      prune_max_size_gb = cfg.pruning.maxSizeGb;
      prune_target_percentage = cfg.pruning.targetPercentage;
      prune_schedule = cfg.pruning.schedule;

      # Other options
      signing_key_path = cfg.signingKeyPath;
      signing_key_name = cfg.signingKeyName;
      skip_signed_by_keys = cfg.skipSignedByKeys;
    };
  };

  # Helper to remove nulls, as TOML doesn't support them
  removeNulls = lib.filterAttrsRecursive (n: v: v != null);

  # Merge the base configuration with any user-provided extraConfig
  finalConfig = lib.recursiveUpdate baseConfig cfg.extraConfig;

  # Generate the final loft.toml content, stripping nulls
  loftToml = tomlFormat.generate "loft.toml" (removeNulls finalConfig);

in
{
  ###### OPTIONS ######
  options.services.loft = {
    enable = mkEnableOption "the Loft pusher service on this system.";

    package = mkOption {
      type = types.package;
      default = pkgs.loft;
      defaultText = literalExpression "pkgs.loft";
      description = "The loft package to use.";
    };

    debug = mkOption {
      type = types.bool;
      default = false;
      description = "Enable debug logging.";
    };

    s3 = {
      bucket = mkOption { type = types.str; description = "S3 bucket name for the cache."; };
      region = mkOption { type = types.str; default = "us-east-1"; description = "S3 region for the cache."; };
      endpoint = mkOption { type = types.str; description = "S3 endpoint URL for the cache."; };
      accessKeyFile = mkOption { type = types.path; description = "Path to a file containing the S3 access key."; };
      secretKeyFile = mkOption { type = types.path; description = "Path to a file containing the S3 secret key."; };
    };

    localCachePath = mkOption {
      type = types.path;
      default = "/var/lib/loft/cache.db";
      description = "Path to the local cache database file for the pusher service.";
    };

    signingKeyPath = mkOption {
      type = with types; nullOr path;
      default = null;
      description = "Absolute path to the Nix signing key file.";
    };

    signingKeyName = mkOption {
      type = with types; nullOr str;
      default = null;
      description = "Name of your Nix signing key (e.g., 'cache.example.org-1').";
    };

    uploadThreads = mkOption {
      type = types.int;
      default = 12;
      description = "The number of concurrent uploads to perform.";
    };
    scanOnStartup = mkOption {
      type = types.bool;
      default = true;
      description = "Whether to perform an initial scan of the store on startup.";
    };
    populateCacheOnStartup = mkOption {
      type = types.bool;
      default = false;
      description = "Whether to populate the local cache from S3 on startup if the cache is empty.";
    };
    skipSignedByKeys = mkOption {
      type = types.listOf types.str;
      default = [ "cache.nixos.org-1" "nix-community.cachix.org-1" ];
      example = literalExpression ''[ "cache.nixos.org-1" "nix-community.cachix.org-1" ]'';
      description = "List of public keys whose signed paths should be skipped for upload.";
    };
    useDiskForLargeNars = mkOption {
      type = types.bool;
      default = true;
      description = "Use disk for large NARs instead of memory.";
    };
    largeNarThresholdMb = mkOption {
      type = types.int;
      default = 1024;
      description = "The threshold in MB for what is considered a large NAR.";
    };
    compression = mkOption {
      type = types.enum [ "xz" "zstd" ];
      default = "zstd";
      description = "The compression algorithm to use.";
    };
    pruning = {
      enable = mkOption {
        type = types.bool;
        default = false;
        description = "Enable pruning of old objects from the S3 cache.";
      };
      retentionDays = mkOption {
        type = types.int;
        default = 30;
        description = "Retention period for pruning in days. Objects older than this will be deleted.";
      };
      maxSizeGb = mkOption {
        type = with types; nullOr int;
        default = null;
        description = "Maximum desired size of the S3 bucket in GB. If exceeded, oldest objects are pruned.";
      };
      targetPercentage = mkOption {
        type = with types; nullOr (types.ints.between 1 99);
        default = null;
        description = "Target percentage to prune down to when maxSizeGb is exceeded (e.g., 80).";
      };
      schedule = mkOption {
        type = with types; nullOr str;
        default = null;
        description = "Schedule for running the pruning job (e.g., '24h', '1d').";
      };
    };

    extraConfig = mkOption {
      type = types.attrs;
      default = { };
      description = "Extra configuration to be merged into loft.toml.";
    };
  };

  ###### CONFIGURATION ######
  config = mkIf cfg.enable {
    # Create the state directory for the cache database
    systemd.tmpfiles.rules = [
      "d ${builtins.dirOf cfg.localCachePath} 0750 root root -"
    ];

    # Configure the systemd service for the pusher
    systemd.services.loft = {
      description = "Loft S3 Binary Cache Uploader";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      serviceConfig = {
        Type = "simple";
        ExecStart =
          let
            # A wrapper script to securely load S3 credentials into environment variables
            wrapper = pkgs.writeShellScript "loft-wrapper" ''
              #!${pkgs.runtimeShell}
              set -eu
              export AWS_ACCESS_KEY_ID=$(cat ${cfg.s3.accessKeyFile})
              export AWS_SECRET_ACCESS_KEY=$(cat ${cfg.s3.secretKeyFile})
              exec ${loft-pkg}/bin/loft --config ${loftToml} ${optionalString cfg.debug "--debug"}
            '';
          in
          "${wrapper}";

        Restart = "on-failure";
        RestartSec = "10s";
        User = "root"; # Required to read /nix/store
        Group = "root";

        # Hardening options
        CapabilityBoundingSet = [ "CAP_NET_BIND_SERVICE" ];
        DevicePolicy = "closed";
        LockPersonality = true;
        NoNewPrivileges = true;
        PrivateDevices = true;
        PrivateTmp = true;
        ProtectClock = true;
        ProtectControlGroups = true;
        ProtectHome = true;
        ProtectHostname = true;
        ProtectKernelLogs = true;
        ProtectKernelModules = true;
        ProtectKernelTunables = true;
        ProtectSystem = "strict";
        RestrictAddressFamilies = [ "AF_INET" "AF_INET6" ];
        RestrictNamespaces = true;
        RestrictRealtime = true;
        SystemCallArchitectures = "native";
        SystemCallFilter = "@system-service";
        UMask = "0077";
      };
    };
  };
}
