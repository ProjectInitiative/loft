# Loft Testing Plan

This document outlines the strategy for ensuring the reliability and correctness of Loft through a two-phased testing approach: Unit Testing and Integration Testing.

## Phase 1: Unit Testing

The goal of this phase is to establish a baseline of trust in individual components by testing them in isolation. Currently, the codebase has minimal unit test coverage.

### Tools
*   **Rust Built-in Testing**: Standard `#[cfg(test)]` modules within source files.
*   **Mocking**: Use traits or conditional compilation to mock external dependencies like S3 and the Nix Store where necessary.

### Priorities

1.  **`src/local_cache.rs`**
    *   **Goal**: Verify that the local Redb cache correctly stores and retrieves state.
    *   **Tests**:
        *   Initialize a cache in a temporary directory.
        *   `add_path` / `has_path`: Verify insertion and existence checks.
        *   `remove_hash`: Verify deletion.
        *   `clear`: Verify complete database wipe.
        *   Concurrency: Verify thread safety (though Redb handles most of this).

2.  **`src/nix_utils.rs`**
    *   **Goal**: Verify pure functions related to Nix path manipulation and parsing.
    *   **Tests**:
        *   `extract_hash_from_path`: Test with valid and invalid paths.
        *   `parse_path_signatures_from_json`: Test with sample JSON output from `nix path-info`.
        *   `get_narinfo_key`: Verify correct key generation from store paths.

3.  **`src/cache_checker.rs`**
    *   **Goal**: Verify the logic that decides *what* to upload.
    *   **Strategy**: This is harder to test without mocking `S3Uploader` and `LocalCache`. We should refactor these into traits (e.g., `CacheStorage`, `RemoteStorage`) or use struct mocking to verify:
        *   If path is in local cache -> Do not check remote -> Do not upload.
        *   If path is NOT in local cache -> Check remote.
        *   If path is on remote -> Add to local cache -> Do not upload.
        *   If path is NOWHERE -> Upload.

## Phase 2: Integration Testing (NixOS Tests)

The goal of this phase is to verify the entire system in a realistic, fully automated environment using NixOS's testing framework. This will simulate a real deployment with a real S3 backend.

### Infrastructure

We will use a **NixOS VM Test** (defined in `nixos/tests/` and exposed via `flake.nix` checks). This VM will host:
1.  **Garage S3**: A lightweight, self-hosted S3-compatible object store.
2.  **Loft**: The service under test, configured to push to the local Garage instance.
3.  **Nix Daemon**: The standard Nix installation to generate store paths and (optionally) try to fetch them back.

### Architecture

The test will be defined as a Nix expression that builds a VM with the following configuration:

*   **Network**: A simple isolated network.
*   **Services**:
    *   `services.garage`: Configured with a single node, ephemeral storage, and a known access/secret key.
    *   `services.loft`: Configured to point to `http://localhost:3900` (Garage), with the known credentials.
*   **Test Runner**: A script (Python or Rust) that orchestrates the test steps inside the VM.

### Test Scenarios

The integration test will execute the following sequence:

#### 1. Setup & Initialization
*   Start Garage.
*   Create a bucket (e.g., `loft-test-bucket`) using `awscli` or `mc`.
*   Start the Loft service.
*   **Verification**: Check Loft logs to ensure it started without error and connected to S3.

#### 2. Basic Upload
*   **Action**: Build a unique Nix store path (e.g., a simple `runCommand "test-pkg" {} "echo hello > $out"`).
*   **Trigger**: Loft should automatically detect this new path (via `nix-store-watcher`) or we can trigger a manual scan.
*   **Verification**:
    *   Query Garage S3 to see if the `.narinfo` and `.nar` files exist for that path.
    *   Verify `local_cache` (by inspecting the DB or logs) marks the path as uploaded.

#### 3. Deduplication
*   **Action**: Trigger a re-upload of the same path (e.g., via `loft --upload-path ...`).
*   **Verification**: Loft logs should indicate "already exists" or "skipping". The S3 object timestamp should *not* update (if logic prevents re-upload).

#### 4. Service Resilience
*   **Action**: Kill the Loft process (`systemctl stop loft`).
*   **Action**: Create a new store path.
*   **Action**: Start Loft (`systemctl start loft`).
*   **Verification**: Loft should perform its "initial scan", find the missed path, and upload it.

#### 5. End-to-End Cache (Fetch)
*   **Action**:
    *   Stop Loft.
    *   Configure the system's `nix.settings.substituters` to include the local Garage bucket (e.g., `s3://loft-test-bucket?endpoint=http://localhost:3900...`).
    *   Delete the test path from the local store (`nix store delete ...`).
    *   Run `nix build /nix/store/...` (the path we just deleted).
*   **Verification**: Nix should successfully fetch the path from the Garage S3 bucket (cache hit).

### Implementation Plan

1.  **Add `nixos/tests/integration.nix`**: The NixOS test definition.
2.  **Update `flake.nix`**: Add the test to the `checks` output.
    ```nix
    checks.x86_64-linux.integration = pkgs.nixosTest (import ./nixos/tests/integration.nix);
    ```
3.  **Test Script Logic**:
    The test script (Python) will look roughly like this:
    ```python
    start_all()

    # 1. Setup Garage
    machine.wait_for_unit("garage.service")
    machine.succeed("aws --endpoint-url http://localhost:3900 s3 mb s3://test-bucket")

    # 2. Start Loft
    machine.systemctl("start loft.service")
    machine.wait_for_unit("loft.service")

    # 3. Create Data
    out_path = machine.succeed("nix-build -E '(import <nixpkgs> {}).runCommand \"test\" {} \"echo hi > $out\"'").strip()

    # 4. Wait for Upload
    machine.wait_until_succeeds(f"aws --endpoint-url http://localhost:3900 s3 ls s3://test-bucket | grep {hash_from_path(out_path)}")

    # ... further scenarios
    ```

## Future Considerations

*   **Load Testing**: Spinning up many small paths to test concurrency limits.
*   **Large File Testing**: Verifying multipart upload for >1GB NARs.
