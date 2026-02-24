{ pkgs, ... }:

{
  name = "loft-integration-test";

  nodes.machine = { config, pkgs, lib, ... }: {
    imports = [ ../module.nix ];

    virtualisation.writableStore = true;
    virtualisation.memorySize = 2048;

    services.garage = {
      enable = true;
      package = pkgs.garage;
      settings = {
        metadata_dir = "/var/lib/garage/meta";
        data_dir = "/var/lib/garage/data";
        replication_factor = 1;
        rpc_bind_addr = "[::]:3901";
        s3_api = {
          s3_region = "us-east-1";
          api_bind_addr = "[::]:3900";
          root_domain = ".s3.garage.localhost";
        };
      };
    };

    services.loft = {
      enable = true;
      s3 = {
        bucket = "loft-test-bucket";
        endpoint = "http://localhost:3900";
        region = "us-east-1";
        accessKeyFile = "/etc/loft-s3-access-key";
        secretKeyFile = "/etc/loft-s3-secret-key";
      };
      uploadThreads = 2;
      scanOnStartup = true;
    };

    # We override the systemd service to NOT start automatically
    systemd.services.loft.wantedBy = lib.mkForce [];

    environment.systemPackages = with pkgs; [
      awscli2
      jq
      garage
    ];

    nix.settings = {
      experimental-features = [ "nix-command" "flakes" ];
      substituters = lib.mkForce [ "http://localhost:3900/loft-test-bucket" ];
      trusted-substituters = [ "http://localhost:3900/loft-test-bucket" ];
      trusted-users = [ "root" ];
      # We turn off signature checking since we are using generated keys without signing
      # (or we could generate a signing key, but let's keep it simple for now)
    };

    networking.firewall.allowedTCPPorts = [ 3900 3901 ];
  };

  testScript = ''
    import json
    import time

    machine.start()
    machine.wait_for_unit("garage.service")
    machine.wait_for_open_port(3900)

    # 1. Setup Garage
    # Configure garage layout
    machine.succeed("garage layout assign -z 1 -c 1")
    machine.succeed("garage layout apply --version 1")

    # Create key
    key_info_str = machine.succeed("garage key create test-key")

    # Parse key info
    lines = key_info_str.splitlines()
    access_key = ""
    secret_key = ""
    for line in lines:
        if "Key ID:" in line:
            access_key = line.split("Key ID:")[1].strip()
        if "Secret key:" in line:
            secret_key = line.split("Secret key:")[1].strip()

    if not access_key or not secret_key:
        raise Exception(f"Failed to parse keys from:\n{key_info_str}")

    print(f"Access Key: {access_key}")

    machine.succeed(f"echo -n '{access_key}' > /etc/loft-s3-access-key")
    machine.succeed(f"echo -n '{secret_key}' > /etc/loft-s3-secret-key")

    # Create Bucket
    env_vars = f"AWS_ACCESS_KEY_ID={access_key} AWS_SECRET_ACCESS_KEY={secret_key} AWS_DEFAULT_REGION=us-east-1"
    machine.succeed(f"{env_vars} aws --endpoint-url http://localhost:3900 s3 mb s3://loft-test-bucket")

    # Make bucket public so Nix can fetch without credentials
    machine.succeed(f"{env_vars} aws --endpoint-url http://localhost:3900 s3api put-bucket-acl --bucket loft-test-bucket --acl public-read")

    # 2. Start Loft
    machine.systemctl("start loft.service")
    machine.wait_for_unit("loft.service")

    # 3. Basic Upload
    # Create a path
    out_path = machine.succeed("nix-build -E '(import <nixpkgs> {}).runCommand \"test-pkg\" {} \"echo hello > $out\"'").strip()
    print(f"Created path: {out_path}")

    # Wait for upload
    hash_part = out_path.split("-")[0].split("/")[-1]
    narinfo = f"{hash_part}.narinfo"

    # Loft watcher should pick it up. Give it some time.
    machine.wait_until_succeeds(f"{env_vars} aws --endpoint-url http://localhost:3900 s3 ls s3://loft-test-bucket/{narinfo}")
    print("Upload verified on S3")

    # 4. End-to-End Cache (Fetch)
    machine.systemctl("stop loft.service")

    # Delete the path
    machine.succeed(f"nix store delete {out_path}")

    # Try to build again (should fetch)
    # Use --option require-sigs false, and force remote fetch with --max-jobs 0
    machine.succeed(f"nix build {out_path} --option require-sigs false --max-jobs 0")

    print("Fetch verified!")
  '';
}
