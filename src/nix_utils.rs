//! Utilities for interacting with the Nix command-line tools.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::time::sleep;
use tracing::{info, warn};
use reqwest;

use crate::config::Config;
use crate::s3_uploader::S3Uploader;

/// Checks if a store path exists in an upstream binary cache.
pub async fn check_path_in_upstream_cache(cache_url: &str, store_path: &str) -> Result<bool> {
    let store_hash = Path::new(store_path)
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.split('-').next().unwrap_or(""))
        .unwrap_or("");

    let narinfo_url = format!("{}/narinfo/{}", cache_url, store_hash);

    let client = reqwest::Client::new();
    let response = client.get(&narinfo_url).send().await?;

    match response.status() {
        reqwest::StatusCode::OK => Ok(true),
        reqwest::StatusCode::NOT_FOUND => Ok(false),
        _ => Err(anyhow::anyhow!(
            "Failed to query upstream cache {}: Unexpected status code {}",
            cache_url,
            response.status()
        )),
    }
}

/// Gets the closure of a store path.
pub fn get_store_path_closure(store_path: &str) -> Result<Vec<String>> {
    let output = Command::new("nix-store")
        .arg("-qR")
        .arg(store_path)
        .output()
        .with_context(|| "Failed to execute nix-store to get path closure.")?;

    let output_str = String::from_utf8(output.stdout)?;
    Ok(output_str.lines().map(String::from).collect())
}

/// Gets the .narinfo for a store path.
pub fn get_nar_info(store_path: &Path) -> Result<String> {
    let output = Command::new("nix-store")
        .arg("--query")
        .arg("--get-nar-info")
        .arg(store_path)
        .output()
        .with_context(|| "Failed to get .narinfo")?;

    Ok(String::from_utf8(output.stdout)?)
}

/// Dumps a store path to NAR bytes in memory.
pub fn dump_nar_to_bytes(store_path: &Path) -> Result<Vec<u8>> {
    let output = Command::new("nix-store")
        .arg("--dump")
        .arg(store_path)
        .output()
        .with_context(|| "Failed to dump NAR from store path")?;
    Ok(output.stdout)
}

/// Uploads the NAR and .narinfo for a given store path.
pub async fn upload_nar_for_path(uploader: Arc<S3Uploader>, path: &Path, config: &Config) -> Result<()> {
    let store_hash = path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.split('-').next().unwrap_or(""))
        .unwrap_or("");

    // 1. Dump the NAR to bytes in memory.
    let nar_bytes = dump_nar_to_bytes(path)?;
    info!("Created NAR for '{}' in memory.", path.display());

    // 2. Sign the path if signing is enabled.
    if let (Some(key_path), Some(key_name)) = (&config.loft.signing_key_path, &config.loft.signing_key_name) {
        info!("Signing path '{}' with key '{}'.", path.display(), key_name);
        let temp_key_file = NamedTempFile::new()?;
        fs::write(temp_key_file.path(), fs::read_to_string(key_path)?)?;
        // Set permissions for the temporary file (e.g., chmod 600).
        // This is important for security.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(temp_key_file.path(), fs::Permissions::from_mode(0o600))?;
        }

        let status = Command::new("nix")
            .arg("store")
            .arg("sign")
            .arg("--key-file")
            .arg(temp_key_file.path())
            .arg(path)
            .status()
            .with_context(|| format!("Failed to execute nix store sign for '{}'", path.display()))?;

        if !status.success() {
            return Err(anyhow::anyhow!("nix store sign failed for '{}'", path.display()));
        }
    }

    // 3. Upload the NAR with retry logic.
    let nar_key = format!("{}.nar", store_hash);
    let mut attempts = 0;
    loop {
        attempts += 1;
        match uploader.upload_bytes(nar_bytes.clone(), &nar_key).await {
            Ok(_) => break,
            Err(e) => {
                if attempts >= 3 {
                    return Err(e);
                }
                warn!(
                    "Failed to upload NAR for '{}' (attempt {}/3): {:?}. Retrying in 5 seconds...",
                    path.display(),
                    attempts,
                    e
                );
                sleep(Duration::from_secs(5)).await;
            }
        }
    }

    // 4. Generate and upload the .narinfo with retry logic.
    let nar_info_content = get_nar_info(path)?;
    let narinfo_key = format!("{}.narinfo", store_hash);
    let mut attempts = 0;
    loop {
        attempts += 1;
        match uploader
            .upload_bytes(nar_info_content.clone().into_bytes(), &narinfo_key)
            .await
        {
            Ok(_) => break,
            Err(e) => {
                if attempts >= 3 {
                    return Err(e);
                }
                warn!(
                    "Failed to upload .narinfo for '{}' (attempt {}/3): {:?}. Retrying in 5 seconds...",
                    path.display(),
                    attempts,
                    e
                );
                sleep(Duration::from_secs(5)).await;
            }
        }
    }

    Ok(())
}