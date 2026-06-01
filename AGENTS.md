# Agent Working Guide — Loft

## Environment

This project uses **devenv** for both builds and the development environment. The toolchain is pinned via `rust-overlay`. The dev shell is loaded automatically via `direnv`.

## Available Commands

| Command                         | Description          |
| ------------------------------- | -------------------- |
| `devenv shell -- cargo build`   | Build                 |
| `devenv shell -- cargo test`    | Run tests             |
| `devenv shell -- cargo fmt`     | Format code           |
| `devenv shell -- cargo clippy`  | Lint                  |
| `devenv test`                   | Run test suite        |
| `nix flake check`               | Run all checks        |
| `nix build`                     | Full sandboxed build  |
| `cache-test`                    | Test cache workflow   |

## Checks

| Check          | Command                                        |
| -------------- | ---------------------------------------------- |
| Formatting     | `treefmt --fail-on-change`                     |
| Clippy         | `nix build .#checks.<system>.clippy`           |
| Unit tests     | `nix build .#checks.<system>.unit-tests`       |
| Integration    | `nix build .#checks.<system>.integration`      |

## NixOS Module

The flake exports `nixosModules.loft` — a NixOS module for running loft as a systemd service. Enable it with:

```nix
{
  imports = [ inputs.loft.nixosModules.loft ];
  services.loft = {
    enable = true;
    s3 = {
      bucket = "my-cache";
      endpoint = "https://s3.example.com";
      accessKeyFile = "/run/secrets/s3-access-key";
      secretKeyFile = "/run/secrets/s3-secret-key";
    };
  };
}
```

## Architecture

Store operations use **nix C API** where available (`nix-bindings` vendored crate) and **CLI** for operations the C API doesn't expose yet:

| Operation | Backend | Reason |
|-----------|---------|--------|
| `Store::open`, `store_dir`, `real_path` | C API (`nix-bindings`) | Available in `nix_api_store.h` |
| `StorePath::parse`, `hash_part` | C API (`nix-bindings`) | Available in `nix_api_store.h` |
| `query_path_info` | CLI (`nix path-info --json`) | Not in C API — requires C++ internals |
| `nar_from_path` | CLI (`nix nar dump-path`) | Not in C API — requires C++ internals |
| `compute_fs_closure` | CLI (`nix-store --query`) | Not in C API at nix 2.31 |

**TODO**: Revisit when [nix C API](https://github.com/NixOS/nix/issues) expands to cover path info queries and NAR streaming. Tracked in `src/nix_store.rs`.

## Adding a Dependency

1. `devenv shell -- cargo add <crate>`
2. Build and test: `devenv shell -- cargo build && devenv shell -- cargo test`
3. For sys crates, add system libs to `buildInputs` in `flake.nix`

## Adding a Dependency

1. `devenv shell -- cargo add <crate>`
2. Build and test: `devenv shell -- cargo build && devenv shell -- cargo test`
3. For sys crates, add system libs to `buildInputs` in `crane.nix`
