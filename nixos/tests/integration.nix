{ pkgs, ... }:

{
  name = "loft-integration-test";

  nodes.machine = { config, pkgs, lib, ... }: {
    imports = [ ../module.nix ];

    virtualisation.writableStore = true;
    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 4096;

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
      scanOnStartup = false;
      populateCacheOnStartup = false;
      skipSignedByKeys = [ "test-exclude-key-1" "cache.nixos.org-1" ];
    };

    systemd.services.loft.wantedBy = lib.mkForce [];

    environment.systemPackages = with pkgs; [
      awscli2
      jq
      garage
      loft
      coreutils
    ];

    nix.settings = {
      experimental-features = [ "nix-command" "flakes" ];
      trusted-users = [ "root" ];
      substituters = [];
      sandbox = false;
    };

    networking.firewall.allowedTCPPorts = [ 3900 3901 ];
  };

  testScript = ''
    import re
    import tempfile
    import os

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
        actual_hash = path.split("/")[-1].split("-")[0]
        narinfo = actual_hash + ".narinfo"
        
        machine.wait_until_succeeds(with_s3("aws --endpoint-url http://localhost:3900 s3 ls s3://loft-test-bucket/" + narinfo), timeout=timeout)
        
        machine.succeed("nix-store --delete --ignore-liveness " + path)
        machine.succeed(with_s3("nix build " + path + " --substituters '" + s3_url + "' --option require-sigs false --max-jobs 0 --impure"))
        machine.log("Verified: " + path + " is fetchable from S3")

    machine.start()
    machine.wait_for_unit("garage.service", timeout=30)
    machine.wait_for_open_port(3900, timeout=10)

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
        
        secure_copy(access_key, "/etc/loft-s3-access-key")
        secure_copy(secret_key, "/etc/loft-s3-secret-key")
        
        machine.succeed("garage bucket create loft-test-bucket")
        machine.succeed("garage bucket allow loft-test-bucket --key test-key --read --write")

    s3_url = f"s3://loft-test-bucket?scheme=http&endpoint=localhost:3900&region=us-east-1&access_key_id={access_key}&secret_access_key={secret_key}"

    with subtest("Concurrency: 2 rapid path additions"):
        reset_state()
        machine.succeed("systemctl start loft.service")
        machine.wait_for_unit("loft.service", timeout=30)
        
        machine.log("Stress testing with 2 paths...")
        p1 = machine.succeed("nix build --no-link --print-out-paths --expr 'derivation { name = \"p1\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo 1 > $out\" ]; system = \"${pkgs.system}\"; PATH = \"/run/current-system/sw/bin\"; }' --impure").strip()
        p2 = machine.succeed("nix build --no-link --print-out-paths --expr 'derivation { name = \"p2\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo 2 > $out\" ]; system = \"${pkgs.system}\"; PATH = \"/run/current-system/sw/bin\"; }' --impure").strip()
        
        verify_cache(p1, s3_url, timeout=60)
        verify_cache(p2, s3_url, timeout=60)

    with subtest("Concurrency: Deduplication of shared dependencies"):
        reset_state()
        machine.succeed("systemctl start loft.service")
        machine.wait_for_unit("loft.service", timeout=30)
        
        # Create a shared dependency
        dep = machine.succeed("nix build --no-link --print-out-paths --expr 'derivation { name = \"shared-dep\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo shared > $out\" ]; system = \"${pkgs.system}\"; PATH = \"/run/current-system/sw/bin\"; }' --impure").strip()
        dep_hash = dep.split("/")[-1].split("-")[0]
        
        # Create two paths that depend on it
        machine.log(f"Building p-alpha and p-beta sharing {dep_hash}")
        nix_expr = "let dep = \"" + dep + "\"; in [ (derivation { name = \"p-alpha\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo a > $out; cat ''${dep} > /dev/null\" ]; system = \"${pkgs.system}\"; PATH = \"/run/current-system/sw/bin\"; }) (derivation { name = \"p-beta\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo b > $out; cat ''${dep} > /dev/null\" ]; system = \"${pkgs.system}\"; PATH = \"/run/current-system/sw/bin\"; }) ]"
        machine.succeed(f"nix build --no-link --expr '{nix_expr}' --impure")
        
        # Wait for them to show up in S3
        machine.wait_until_succeeds(with_s3(f"aws --endpoint-url http://localhost:3900 s3 ls s3://loft-test-bucket/{dep_hash}.narinfo"), timeout=60)
        
        # Check logs for duplicate uploads of the shared dependency
        logs = machine.succeed("journalctl -u loft.service")
        # We look for the "Initiating streaming multipart upload" message which happens once per upload attempt
        upload_count = logs.count(f"Initiating streaming multipart upload for 'nar/{dep_hash}")
        
        if upload_count > 1:
            # Note: It might be 0 if it was already in S3, but reset_state() clears S3.
            # It should be exactly 1.
            raise Exception(f"Shared dependency {dep_hash} was uploaded {upload_count} times! Expected 1.")
        
        machine.log(f"Verified: {dep_hash} was uploaded {upload_count} time(s).")

    with subtest("Config: skipSignedByKeys rejection"):
        reset_state()
        machine.succeed("systemctl start loft.service")
        machine.succeed("nix-store --generate-binary-cache-key test-exclude-key-1 /tmp/sk1 /tmp/pk1")
        p_to_sign = machine.succeed("nix build --no-link --print-out-paths --expr 'derivation { name = \"signed-path\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo signed > $out\" ]; system = \"${pkgs.system}\"; PATH = \"/run/current-system/sw/bin\"; }' --impure").strip()
        machine.succeed("nix store sign --key-file /tmp/sk1 " + p_to_sign)
        
        hash_part = p_to_sign.split("/")[-1].split("-")[0]
        narinfo = hash_part + ".narinfo"
        
        import time
        machine.log("Waiting to ensure signed path is NOT uploaded...")
        time.sleep(10)
        machine.fail(with_s3("aws --endpoint-url http://localhost:3900 s3 ls s3://loft-test-bucket/" + narinfo))

    manual_config = "/tmp/loft-cli-config.toml"
    manual_config_content = (
        "[s3]\nbucket = \"loft-test-bucket\"\nregion = \"us-east-1\"\nendpoint = \"http://localhost:3900\"\n" +
        "access_key = \"" + access_key + "\"\nsecret_key = \"" + secret_key + "\"\n" +
        "[loft]\nupload_threads = 1\nscan_on_startup = true\nlocal_cache_path = \"/var/lib/loft/cache.db\"\n" +
        "prune_enabled = true\n"
    )

    with subtest("Service: Pruning verification"):
        reset_state()
        machine.succeed("systemctl start loft.service")
        p = machine.succeed("nix build --no-link --print-out-paths --expr 'derivation { name = \"pp\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo prune > $out\" ]; system = \"${pkgs.system}\"; PATH = \"/run/current-system/sw/bin\"; }' --impure").strip()
        verify_cache(p, s3_url)
        
        machine.succeed("systemctl stop loft.service")
        prune_config = "/tmp/prune-config.toml"
        prune_config_content = manual_config_content + "prune_max_size_gb = 0\nprune_target_percentage = 0\n"
        secure_copy(prune_config_content, prune_config)
        
        machine.log("Running manual prune with 0GB size limit...")
        machine.succeed(with_s3("loft --config " + prune_config + " --prune"))
        
        hash_part = p.split("/")[-1].split("-")[0]
        machine.fail(with_s3("aws --endpoint-url http://localhost:3900 s3 ls s3://loft-test-bucket/" + hash_part + ".narinfo"))

    with subtest("CLI: clear-cache and Force Scan"):
        reset_state()
        machine.succeed("systemctl stop loft.service")
        machine.log("Creating pre-existing paths...")
        pre_paths = []
        for i in range(3):
            p = machine.succeed(f"nix build --no-link --print-out-paths --expr 'derivation {{ name = \"pre-{i}\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo {i} > $out\" ]; system = \"${pkgs.system}\"; PATH = \"/run/current-system/sw/bin\"; }}' --impure").strip()
            pre_paths.append(p)
            
        secure_copy(manual_config_content, manual_config)
        machine.succeed(with_s3("loft --config " + manual_config + " --clear-cache"))
        machine.succeed(with_s3("loft --config " + manual_config + " --force-scan"))
        
        for p in pre_paths:
            actual_hash = p.split("/")[-1].split("-")[0]
            narinfo = actual_hash + ".narinfo"
            machine.wait_until_succeeds(with_s3("aws --endpoint-url http://localhost:3900 s3 ls s3://loft-test-bucket/" + narinfo), timeout=60)
            
    with subtest("Bulk: Force scan picks up missing remote paths"):
        reset_state()
        machine.succeed("systemctl stop loft.service")
        bulk_paths = []
        for i in range(5):
            p = machine.succeed(f"nix build --no-link --print-out-paths --expr 'derivation {{ name = \"bulk-{i}\"; builder = \"/bin/sh\"; args = [ \"-c\" \"echo {i} > $out\" ]; system = \"${pkgs.system}\"; PATH = \"/run/current-system/sw/bin\"; }}' --impure").strip()
            bulk_paths.append(p)
        
        secure_copy(manual_config_content, manual_config)
        machine.succeed(with_s3("loft --config " + manual_config + " --force-scan"))
        for p in bulk_paths:
            verify_cache(p, s3_url)
            
        machine.log("Deleting 2 paths from S3...")
        for i in range(2):
            actual_hash = bulk_paths[i].split("/")[-1].split("-")[0]
            machine.succeed(with_s3("aws --endpoint-url http://localhost:3900 s3 rm s3://loft-test-bucket/" + actual_hash + ".narinfo"))
            
        machine.succeed(with_s3("loft --config " + manual_config + " --force-scan"))
        for p in bulk_paths:
            verify_cache(p, s3_url)

    machine.log("All advanced integration tests passed!")
  '';
}
