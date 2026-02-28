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
        rpc_secret = "0000000000000000000000000000000000000000000000000000000000000000";
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
      trusted-users = [ "root" ];
    };

    networking.firewall.allowedTCPPorts = [ 3900 3901 ];
  };

  testScript = ''
    machine.start()
    # Fail fast if garage doesn't start within 30s
    machine.wait_for_unit("garage.service", timeout=30)
    machine.wait_for_open_port(3900, timeout=10)
    machine.wait_for_open_port(3901, timeout=10)

    # 1. Setup Garage
    status = machine.succeed("garage status")
    import re
    match = re.search(r"([0-9a-f]{16})", status)
    if not match:
        raise Exception("Could not find node ID in garage status:\n" + status)
    node_id = match.group(1)
    
    machine.succeed("garage layout assign " + node_id + " -z 1 -c 100M")
    machine.succeed("garage layout apply --version 1")

    key_info_str = machine.succeed("garage key create test-key")

    access_key = ""
    secret_key = ""
    for line in key_info_str.splitlines():
        if "Key ID:" in line:
            access_key = line.split("Key ID:")[1].strip()
        if "Secret key:" in line:
            secret_key = line.split("Secret key:")[1].strip()

    if not access_key or not secret_key:
        raise Exception("Failed to parse keys from:\n" + key_info_str)

    machine.succeed("echo -n '" + access_key + "' > /etc/loft-s3-access-key")
    machine.succeed("echo -n '" + secret_key + "' > /etc/loft-s3-secret-key")

    machine.succeed("garage bucket create loft-test-bucket")
    machine.succeed("garage bucket allow loft-test-bucket --key test-key --read --write")

    # 2. Prepare test file
    machine.succeed("echo 'hello world' > /tmp/test-file")
    out_path = machine.succeed("nix-store --add /tmp/test-file").strip()
    print("Created path: " + out_path)

    # 3. Start Loft
    machine.succeed("systemctl start loft.service")
    # Fail fast if loft doesn't start or crashes within 30s
    machine.wait_for_unit("loft.service", timeout=30)

    # 4. Basic Upload
    hash_part = out_path.split("-")[0].split("/")[-1]
    narinfo = hash_part + ".narinfo"

    env_vars = "AWS_ACCESS_KEY_ID=" + access_key + " AWS_SECRET_ACCESS_KEY=" + secret_key + " AWS_DEFAULT_REGION=us-east-1"
    
    # Wait for upload with a 60s timeout
    machine.wait_until_succeeds(env_vars + " aws --endpoint-url http://localhost:3900 s3 ls s3://loft-test-bucket/" + narinfo, timeout=60)
    print("Upload verified on S3")

    # 5. End-to-End Cache (Fetch)
    machine.succeed("systemctl stop loft.service")

    # Delete the path
    machine.succeed("nix store delete " + out_path)

    # Try to build again (should fetch from S3)
    s3_url = "s3://loft-test-bucket?scheme=http&endpoint=localhost:3900&region=us-east-1"
    machine.succeed(env_vars + " nix build " + out_path + " --substituters '" + s3_url + "' --option require-sigs false --max-jobs 0")

    print("Fetch verified!")
  '';
}
