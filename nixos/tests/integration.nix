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
      uploadThreads = 4;
      scanOnStartup = true;
      populateCacheOnStartup = true;
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

    def reset_state(env_vars):
        machine.log("Resetting Loft and S3 state...")
        machine.succeed("systemctl stop loft.service || true")
        machine.succeed(env_vars + " aws --endpoint-url http://localhost:3900 s3 rm s3://loft-test-bucket --recursive || true")
        machine.succeed("rm -f /var/lib/loft/cache.db")
        machine.log("State reset complete.")

    def verify_cache(path, env_vars, s3_url, timeout=60):
        machine.log("Waiting for path in S3 cache: " + path)
        hash_part = path.split("-")[0].split("/")[-1]
        narinfo = hash_part + ".narinfo"
        
        machine.wait_until_succeeds(env_vars + " aws --endpoint-url http://localhost:3900 s3 ls s3://loft-test-bucket/" + narinfo, timeout=timeout)
        
        machine.succeed("nix-store --delete --ignore-liveness " + path)
        machine.succeed(env_vars + " nix build " + path + " --substituters '" + s3_url + "' --option require-sigs false --max-jobs 0")
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
        machine.succeed("echo -n '" + access_key + "' > /etc/loft-s3-access-key")
        machine.succeed("echo -n '" + secret_key + "' > /etc/loft-s3-secret-key")
        machine.succeed("garage bucket create loft-test-bucket")
        machine.succeed("garage bucket allow loft-test-bucket --key test-key --read --write")

    env_vars = "AWS_ACCESS_KEY_ID=" + access_key + " AWS_SECRET_ACCESS_KEY=" + secret_key + " AWS_DEFAULT_REGION=us-east-1"
    s3_url = "s3://loft-test-bucket?scheme=http&endpoint=localhost:3900&region=us-east-1"

    # --- SUBTEST: Mass Watcher Test ---
    with subtest("Service: Mass Watcher Upload (via nix-build)"):
        reset_state(env_vars)
        machine.succeed("systemctl start loft.service")
        machine.wait_for_unit("loft.service", timeout=30)
        
        paths = []
        machine.log("Adding 10 files via nix-build to trigger watcher...")
        for i in range(10):
            # runCommand will trigger a .lock file removal event
            p = machine.succeed("nix-build --no-out-link -E '(import <nixpkgs> {}).runCommand \"mass-" + str(i) + "\" {} \"echo " + str(i) + " > $out\"'").strip()
            paths.append(p)
        
        for p in paths:
            verify_cache(p, env_vars, s3_url)

    # --- SUBTEST: Cache Recovery ---
    with subtest("Service: Cache Recovery (Populate from S3)"):
        machine.succeed("systemctl stop loft.service")
        machine.succeed("rm -f /var/lib/loft/cache.db")
        machine.succeed("systemctl start loft.service")
        machine.wait_until_succeeds("journalctl -u loft.service | grep 'Local cache populated from S3'", timeout=30)
        
        p = machine.succeed("nix-build --no-out-link -E '(import <nixpkgs> {}).runCommand \"recovery-test\" {} \"echo recovery > $out\"'").strip()
        verify_cache(p, env_vars, s3_url)

    # --- SUBTEST: Cache Inconsistency ---
    with subtest("Handling Inconsistency: S3 cleared, DB intact"):
        p = machine.succeed("nix-build --no-out-link -E '(import <nixpkgs> {}).runCommand \"inconsistent\" {} \"echo oops > $out\"'").strip()
        verify_cache(p, env_vars, s3_url)
        
        machine.succeed(env_vars + " aws --endpoint-url http://localhost:3900 s3 rm s3://loft-test-bucket --recursive")
        
        # Reset via CLI
        # Generate a temporary config for manual CLI operations
        machine.succeed("cat > /tmp/manual-config.toml <<EOF\n" +
            "[s3]\nbucket = \"loft-test-bucket\"\nregion = \"us-east-1\"\nendpoint = \"http://localhost:3900\"\n" +
            "access_key = \"" + access_key + "\"\nsecret_key = \"" + secret_key + "\"\n" +
            "[loft]\nupload_threads = 1\nscan_on_startup = false\nlocal_cache_path = \"/var/lib/loft/cache.db\"\nEOF")
        
        machine.log("Clearing cache via CLI...")
        machine.succeed("systemctl stop loft.service")
        machine.succeed("loft --config /tmp/manual-config.toml --clear-cache")
        # Manually upload to prove it can recover
        machine.succeed("loft --config /tmp/manual-config.toml --upload-path " + p)
        verify_cache(p, env_vars, s3_url, timeout=60)
        
        # Restart service for any remaining logic
        machine.succeed("systemctl start loft.service")

    machine.log("All advanced integration tests passed!")
  '';
}
