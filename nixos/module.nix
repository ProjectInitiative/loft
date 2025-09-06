# nixos/module.nix
{ config, lib, pkgs, ... }:

let
  cfg = config.services.loft;
  loft-pkg = config.services.loft.package;

  # Use Nixpkgs' toml format for clean generation
  tomlFormat = pkgs.formats.toml { };

  # Define the base configuration from the explicit options
  baseConfig = {
    s3 = {
      inherit (cfg.s3) bucket region endpoint;
      access_key = cfg.s3.accessKeyFile;
      secret_key = cfg.s3.secretKeyFile;
    };
    loft = {
      inherit (cfg)
        upload_threads
        scan_on_startup
        populate_cache_on_startup
        use_disk_for_large_nars
        large_nar_threshold_mb
        compression
        prune_enabled
        prune_retention_days
        prune_max_size_gb
        prune_target_percentage
        prune_schedule;
      signing_key_path = cfg.signingKeyFile;
      signing_key_name = cfg.signingKeyName;
      skip_signed_by_keys = cfg.skipSignedByKeys;
      local_cache_db_path = cfg.localCacheDBPath;
    };
  };

  # Merge the base configuration with any user-provided extraConfig
  finalConfig = lib.recursiveUpdate baseConfig cfg.extraConfig;

  # Generate the final loft.toml content
  loftToml = tomlFormat.generate "loft.toml" finalConfig;

  # Determine if AWS credentials are required for the puller
  privatePuller = cfg.puller.enable && cfg.puller.private;
  pullerS3Url = "s3://${cfg.s3.bucket}?endpoint=${cfg.s3.endpoint}&region=${cfg.s3.region}";

in
{
  ###### OPTIONS ######
  options.services.loft = {
    enable = lib.mkEnableOption "Loft: a client-only Nix binary cache uploader for S3";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The loft package to use.";
    };

    
    localCacheDBPath = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/loft/cache.db";
      description = "Path to the local cache database file.";
    };

    puller = {
      enable = lib.mkEnableOption "this system as a cache puller.";
      private = lib.mkBoolOpt false "Whether to use private S3 access for pulling.";
      trustedPublicKeys = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "my-cache.example.com-1:abcdef..." ];
        description = "The public key(s) for the binary cache.";
      };
    };

    # ... (all the previous options like upload_threads, scan_on_startup, etc. remain the same)
    upload_threads = lib.mkOption {
      type = lib.types.int;
      default = 4;
      description = "The number of concurrent uploads to perform.";
    };

    scan_on_startup = lib.mkBoolOpt false "Whether to perform an initial scan of the store on startup.";
    populate_cache_on_startup = lib.mkBoolOpt false "Populate local cache from S3 on startup if the cache is empty.";

    signingKeyFile = lib.mkOption {
      type = with lib.types; nullOr path;
      default = null;
      description = "Absolute path to the Nix signing key file.";
    };

    signingKeyName = lib.mkOption {
      type = with lib.types; nullOr str;
      default = null;
      description = "Name of your Nix signing key (e.g., 'cache.example.org-1').";
    };

    skipSignedByKeys = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "cache.nixos.org-1" ];
      description = "List of public keys whose signed paths should be skipped for upload.";
    };

    use_disk_for_large_nars = lib.mkBoolOpt false "Use disk for large NARs instead of memory.";
    large_nar_threshold_mb = lib.mkOption {
      type = lib.types.int;
      default = 1024;
      description = "The threshold in MB for what is considered a large NAR.";
    };

    compression = lib.mkOption {
      type = lib.types.enum [ "xz" "zstd" ];
      default = "zstd";
      description = "The compression algorithm to use.";
    };

    prune_enabled = lib.mkBoolOpt false "Enable pruning of old objects from the S3 cache.";
    prune_retention_days = lib.mkOption {
      type = lib.types.int;
      default = 30;
      description = "Retention period for pruning in days.";
    };
    prune_max_size_gb = lib.mkOption {
      type = with lib.types; nullOr int;
      default = null;
      description = "Maximum desired size of the S3 bucket in GB.";
    };
    prune_target_percentage = lib.mkOption {
      type = with lib.types; nullOr (lib.types.ints.between 1 99);
      default = null;
      description = "Target percentage to prune down to when prune_max_size_gb is exceeded.";
    };
    prune_schedule = lib.mkOption {
      type = with lib.types; nullOr str;
      default = null;
      example = "24h";
      description = "Schedule for running the pruning job (e.g., '24h', '1d').";
    };

    s3 = {
      bucket = lib.mkOption { type = lib.types.str; };
      region = lib.mkOption { type = lib.types.str; default = "us-east-1"; };
      endpoint = lib.mkOption { type = lib.types.str; };
      accessKeyFile = lib.mkOption { type = lib.types.path; };
      secretKeyFile = lib.mkOption { type = lib.types.path; };
    };

    # --- NEW OPTION ---
    extraConfig = lib.mkOption {
      type = lib.types.attrs; # `attrs` is an alias for `attrsOf anything`
      default = { };
      description = ''
        Extra configuration options to be merged into the loft.toml file.
        This allows you to set new configuration options without needing to
        update the NixOS module.
      '';
    };
  };


  ###### CONFIGURATION ######
  config = lib.mkIf cfg.enable {
    # ... (the rest of the file remains the same)
    nix.settings = lib.mkMerge [
      (lib.mkIf cfg.puller.enable {
        substituters = [ pullerS3Url ];
        trusted-public-keys = cfg.puller.trustedPublicKeys;
      })
      (lib.mkIf privatePuller {
        extra-access-tokens =
          let
            awsCredsScript = pkgs.writeShellScript "aws-creds-for-nix" ''
              #!${pkgs.runtimeShell}
              set -eu
              echo "AWS_ACCESS_KEY_ID=$(<${cfg.s3.accessKeyFile})"
              echo "AWS_SECRET_ACCESS_KEY=$(<${cfg.s3.secretKeyFile})"
            '';
          in
          [ "s3://${cfg.s3.bucket} ${awsCredsScript}" ];
      })
    ];

    systemd.services.loft = {
      description = "Loft S3 Binary Cache Uploader";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      serviceConfig = {
        Type = "simple";
        ExecStart = ''
          ${loft-pkg}/bin/loft --config ${loftToml}
        '';
        Restart = "on-failure";
        RestartSec = "10s";
        User = "root";
        Group = "root";
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
