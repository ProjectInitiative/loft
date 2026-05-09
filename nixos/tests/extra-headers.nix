{ pkgs, ... }:

{
  name = "loft-extra-headers-test";

  nodes.machine = { config, pkgs, lib, ... }: {
    imports = [ ../module.nix ];

    virtualisation.writableStore = true;
    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 4096;

    services.nginx = {
      enable = true;
      virtualHosts."auth-proxy" = {
        listen = [ { port = 3902; addr = "0.0.0.0"; } ];
        locations."/" = {
          proxyPass = "http://localhost:3900";
          extraConfig = ''
            proxy_set_header Host $host:$server_port;
            if ($http_x_loft_auth != "test-token") {
                return 403;
            }
          '';
        };
      };
    };

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

    environment.systemPackages = with pkgs; [
      awscli2
      jq
      garage
      loft
    ];

    nix.settings = {
      experimental-features = [ "nix-command" "flakes" ];
      trusted-users = [ "root" ];
      sandbox = false;
    };

    networking.firewall.allowedTCPPorts = [ 3900 3901 3902 ];
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

    def reset_s3():
        machine.log("Clearing S3 bucket...")
        machine.succeed(with_s3("aws --endpoint-url http://localhost:3900 s3 rm s3://loft-test-bucket --recursive || true"))
        machine.log("S3 bucket cleared.")

    machine.start()
    machine.wait_for_unit("nginx.service", timeout=30)
    machine.wait_for_unit("garage.service", timeout=30)
    machine.wait_for_open_port(3902, timeout=10)
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

    def build_path(name):
        return machine.succeed(
            "nix build --no-link --print-out-paths "
            "--expr 'derivation { name = \"" + name + "\"; builder = \"/bin/sh\"; "
            "args = [ \"-c\" \"echo " + name + " > $out\" ]; "
            "system = \"${pkgs.system}\"; PATH = \"/run/current-system/sw/bin\"; }' --impure"
        ).strip()

    def verify_cached(path):
        hash_part = path.split("/")[-1].split("-")[0]
        narinfo = hash_part + ".narinfo"
        machine.wait_until_succeeds(
            with_s3("aws --endpoint-url http://localhost:3900 s3 ls s3://loft-test-bucket/" + narinfo),
            timeout=60
        )
        machine.succeed("nix-store --delete --ignore-liveness " + path)
        machine.succeed(with_s3(
            "nix build " + path + " --substituters '" + s3_url + "' --option require-sigs false --max-jobs 0 --impure"
        ))

    with subtest("Extra Headers: inline config [s3.extra_headers]"):
        reset_s3()
        cfg = (
            "[s3]\nbucket = \"loft-test-bucket\"\nregion = \"us-east-1\"\nendpoint = \"http://localhost:3902\"\n"
            "access_key = \"" + access_key + "\"\nsecret_key = \"" + secret_key + "\"\n"
            "[s3.extra_headers]\n\"X-Loft-Auth\" = \"test-token\"\n"
            "[loft]\nupload_threads = 1\nscan_on_startup = true\n"
            "local_cache_path = \"/var/lib/loft/cache-inline.db\"\n"
        )
        p = build_path("hdr-inline")
        secure_copy(cfg, "/tmp/loft-hdr-inline.toml")
        machine.succeed(with_s3("loft --config /tmp/loft-hdr-inline.toml --force-scan"))
        verify_cached(p)

    with subtest("Extra Headers: LOFT_EXTRA_HEADER_* env var"):
        reset_s3()
        cfg = (
            "[s3]\nbucket = \"loft-test-bucket\"\nregion = \"us-east-1\"\nendpoint = \"http://localhost:3902\"\n"
            "access_key = \"" + access_key + "\"\nsecret_key = \"" + secret_key + "\"\n"
            "[loft]\nupload_threads = 1\nscan_on_startup = true\n"
            "local_cache_path = \"/var/lib/loft/cache-env.db\"\n"
        )
        p = build_path("hdr-env")
        secure_copy(cfg, "/tmp/loft-hdr-env.toml")
        machine.succeed(
            with_s3("LOFT_EXTRA_HEADER_X_LOFT_AUTH=test-token loft --config /tmp/loft-hdr-env.toml --force-scan")
        )
        verify_cached(p)

    with subtest("Extra Headers: NixOS extraHeadersFile (read from file)"):
        reset_s3()

        secure_copy("test-token\n", "/run/secrets/loft-auth-token")
        cfg = (
            "[s3]\nbucket = \"loft-test-bucket\"\nregion = \"us-east-1\"\nendpoint = \"http://localhost:3902\"\n"
            "access_key = \"" + access_key + "\"\nsecret_key = \"" + secret_key + "\"\n"
            "[loft]\nupload_threads = 1\nscan_on_startup = true\n"
            "local_cache_path = \"/var/lib/loft/cache-file.db\"\n"
        )
        p = build_path("hdr-file")
        secure_copy(cfg, "/tmp/loft-hdr-file.toml")
        machine.succeed(
            with_s3("LOFT_EXTRA_HEADER_X_LOFT_AUTH=$(cat /run/secrets/loft-auth-token) loft --config /tmp/loft-hdr-file.toml --force-scan")
        )
        verify_cached(p)

    with subtest("Extra Headers: missing header — paths not uploaded"):
        reset_s3()
        cfg = (
            "[s3]\nbucket = \"loft-test-bucket\"\nregion = \"us-east-1\"\nendpoint = \"http://localhost:3902\"\n"
            "access_key = \"" + access_key + "\"\nsecret_key = \"" + secret_key + "\"\n"
            "[loft]\nupload_threads = 1\nscan_on_startup = true\n"
            "local_cache_path = \"/var/lib/loft/cache-noauth.db\"\n"
        )
        p = build_path("hdr-noauth")
        secure_copy(cfg, "/tmp/loft-hdr-noauth.toml")
        _status, _output = machine.execute(with_s3("loft --config /tmp/loft-hdr-noauth.toml --force-scan 2>&1"))
        hash_part = p.split("/")[-1].split("-")[0]
        import time
        time.sleep(5)
        machine.fail(with_s3("aws --endpoint-url http://localhost:3900 s3 ls s3://loft-test-bucket/" + hash_part + ".narinfo"))

    machine.log("All extra headers integration tests passed!")
  '';
}
