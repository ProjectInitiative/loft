//! Utilities for interacting with the Nix command-line tools.

use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use tracing::info;

use crate::s3_uploader::S3Uploader;

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
pub async fn upload_nar_for_path(uploader: Arc<S3Uploader>, path: &Path) -> Result<()> {
    let store_hash = path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.split('-').next().unwrap_or(""))
        .unwrap_or("");

    // 1. Dump the NAR to bytes in memory.
    let nar_bytes = dump_nar_to_bytes(path)?;
    info!("Created NAR for '{}' in memory.", path.display());

    // 2. Upload the NAR.
    let nar_key = format!("{}.nar", store_hash);
    uploader.upload_bytes(nar_bytes, &nar_key).await?;

    // 3. Generate and upload the .narinfo.
    let nar_info_content = get_nar_info(path)?;
    let narinfo_key = format!("{}.narinfo", store_hash);
    uploader
        .upload_bytes(nar_info_content.into_bytes(), &narinfo_key)
        .await?;

    Ok(())
}