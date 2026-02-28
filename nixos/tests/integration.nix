{ pkgs, ... }:

{
  name = "loft-integration-test";

  nodes.machine = { config, pkgs, lib, ... }: {
    imports = [ ../module.nix ];

    virtualisation.writableStore = true;
    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 4096; # 4GB to handle datasets

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
      uploadThreads = 8;
      scanOnStartup = true;
      populateCacheOnStartup = true;
      skipSignedByKeys = [ "test-exclude-key-1" "cache.nixos.org-1" ];
    };

    # We override the systemd service to NOT start automatically
    systemd.services.loft.wantedBy = lib.mkForce [];

    environment.systemPackages = with pkgs; [
      awscli2
      jq
      garage
      loft
    ];

    nix.settings = {
      experimental-features = [ "nix-command" "flakes" ];
      trusted-users = [ "root" ];
      substituters = [];
    };
    
    # Ensure we have a dummy nixpkgs available for nix-build
    environment.variables.NIX_PATH = lib.mkForce "nixpkgs=/etc/nixpkgs";
    environment.etc."nixpkgs/default.nix".text = ''
      { ... }: {
        runCommand = name: env: script: derivation {
          inherit name script;
          builder = "/bin/sh";
          args = [ "-c" script ];
          system = "${pkgs.system}";
        };
      }
    '';

    networking.firewall.allowedTCPPorts = [ 3900 3901 ];
  };

  testScript = ''
    import re
    import tempfile
    import os

    # Helper to wrap commands with S3 credentials from files to avoid logging them
    def with_s3(cmd):
        return f"AWS_ACCESS_KEY_ID=$(cat /etc/loft-s3-access-key) AWS_SECRET_ACCESS_KEY=$(cat /etc/loft-s3-secret-key) AWS_DEFAULT_REGION=us-east-1 {cmd}"

    def secure_copy(content, target):
        with tempfile.NamedTemporaryFile(mode='w', delete=False) as tf:
            tf.write(content)
            temp_name = tf.name
        try:
            machine.copy_from_host(temp_name, target)
        finally:
            os.remove(temp_name)

    def reset_state():
        machine.log("Resetting Loft and S3 state...")
        machine.succeed("systemctl stop loft.service || true")
        machine.succeed(with_s3("aws --endpoint-url http://localhost:3900 s3 rm s3://loft-test-bucket --recursive || true"))
        machine.succeed("rm -f /var/lib/loft/cache.db")
        machine.log("State reset complete.")

    def verify_cache(path, s3_url, timeout=60):
        machine.log("Waiting for path in S3 cache: " + path)
        hash_part = path.split("-")[0].split("/")[-1]
        narinfo = hash_part + ".narinfo"
        
        machine.wait_until_succeeds(with_s3("aws --endpoint-url http://localhost:3900 s3 ls s3://loft-test-bucket/" + narinfo), timeout=timeout)
        
        machine.succeed("nix-store --delete --ignore-liveness " + path)
        machine.succeed(with_s3("nix build " + path + " --substituters '" + s3_url + "' --option require-sigs false --max-jobs 0"))
        machine.log("Verified: " + path + " is fetchable from S3")

    machine.start()
    machine.wait_for_unit("garage.service", timeout=30)
    machine.wait_for_open_port(3900, timeout=10)

    # --- SETUP: Credentials ---
    with subtest("Initialize Garage"):
        status = machine.succeed("garage status")
        m = re.search(r"([0-9a-f]{16})", status)
        if not m: raise Exception("No node ID")
        node_id = m.group(1)
        machine.succeed("garage layout assign " + node_id + " -z 1 -c 100M")
        machine.succeed("garage layout apply --version 1")
        key_info = machine.succeed("garage key create test-key")
        ak_m = re.search(r"Key ID: (\S+)", key_info)
        sk_m = re.search(r"Secret key: (\S+)", key_info)
        if not ak_m or not sk_m: raise Exception("No keys")
        access_key = ak_m.group(1)
        secret_key = sk_m.group(1)
        
        # Write keys to files on the host and copy them to the VM to avoid logging them in command strings
        secure_copy(access_key, "/etc/loft-s3-access-key")
        secure_copy(secret_key, "/etc/loft-s3-secret-key")
        
        machine.succeed("garage bucket create loft-test-bucket")
        machine.succeed("garage bucket allow loft-test-bucket --key test-key --read --write")

    s3_url = "s3://loft-test-bucket?scheme=http&endpoint=localhost:3900&region=us-east-1"

    # --- SUBTEST: Stress / Concurrency ---
    with subtest("Concurrency: 30 rapid path additions"):
        reset_state()
        machine.succeed("systemctl start loft.service")
        machine.wait_for_unit("loft.service", timeout=30)
        
        machine.log("Stress testing with 30 rapid builds...")
        # Launch 30 nix-build processes in parallel inside the VM
        machine.succeed("mkdir -p /tmp/stress")
        machine.succeed(
            "for i in {0..29}; do "
            "  nix-build --no-out-link -E \"(import <nixpkgs> {}).runCommand \\\"stress-$i\\\" {} \\\"echo $i > \$out\\\"\" > /tmp/stress/$i & "
            "done; wait"
        )
        paths = machine.succeed("cat /tmp/stress/*").strip().splitlines()
        
        # Verify all 30 were processed
        for p in paths:
            verify_cache(p, s3_url, timeout=90)

    # --- SUBTEST: skipSignedByKeys ---
    with subtest("Config: skipSignedByKeys rejection"):
        reset_state()
        machine.succeed("systemctl start loft.service")
        
        # 1. Generate a key matching the excluded key name in config
        machine.succeed("nix-store --generate-binary-cache-key test-exclude-key-1 /tmp/sk1 /tmp/pk1")
        
        # 2. Create a path and SIGN it
        p_to_sign = machine.succeed("nix-build --no-out-link -E '(import <nixpkgs> {}).runCommand \"signed-path\" {} \"echo signed > $out\"'").strip()
        machine.succeed("nix store sign --key-file /tmp/sk1 " + p_to_sign)
        
        # 3. Verify it is NOT in S3 after some time
        hash_part = p_to_sign.split("-")[0].split("/")[-1]
        narinfo = hash_part + ".narinfo"
        
        import time
        machine.log("Waiting to ensure signed path is NOT uploaded...")
        time.sleep(10)
        machine.fail(with_s3("aws --endpoint-url http://localhost:3900 s3 ls s3://loft-test-bucket/" + narinfo))
        machine.log("Verified: Path signed by excluded key was correctly skipped.")

    # --- SUBTEST: Pruning Verification ---
    with subtest("Service: Pruning verification"):
        # Use the manual config we created in subtest before (or recreate it)
        manual_config = "/tmp/prune-config.toml"
        prune_config_content = (
            "[s3]\nbucket = \"loft-test-bucket\"\nregion = \"us-east-1\"\nendpoint = \"http://localhost:3900\"\n" +
            "access_key = \"" + access_key + "\"\nsecret_key = \"" + secret_key + "\"\n" +
            "[loft]\nupload_threads = 1\nscan_on_startup = false\nlocal_cache_path = \"/tmp/prune-cache.db\"\n" +
            "prune_enabled = true\nprune_retention_days = 30\n"
        )
        secure_copy(prune_config_content, manual_config)
        
        machine.log("Running manual prune...")
        machine.succeed("loft --config " + manual_config + " --prune")
        machine.log("Pruning command completed successfully.")

    machine.log("All advanced integration tests passed!")
  '';
  
}
