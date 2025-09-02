//! Utilities for interacting with the Nix command-line tools.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command; // Keep for get_store_path_closure for now
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn, debug};
use futures::stream::StreamExt;

use attic::nix_store::NixStore; // Keep NixStore
use attic::signing::NixKeypair; // Added this line

use crate::config::Config;
use crate::s3_uploader::S3Uploader;
use crate::nix_manifest::{self, NarInfo};

/// Gets the signature key name for a given store path, if it exists.
pub async fn get_path_signature_key(store_path: &str) -> Result<Option<String>> {
    let nix_store = NixStore::connect()?;
    let store_path_obj = nix_store.parse_store_path(Path::new(store_path))?;

    let path_info = nix_store.query_path_info(store_path_obj).await?;

    if let Some(sig) = path_info.sigs.into_iter().next() {
        // A signature string looks like "key_name:signature_value"
        return Ok(sig.split(':').next().map(|s| s.to_string()));
    }
    Ok(None)
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
pub async fn get_nar_info(store_path: &Path) -> Result<String> {
    let nix_store = NixStore::connect()?;
    let store_path_obj = nix_store.parse_store_path(store_path)?;

    let path_info = nix_store.query_path_info(store_path_obj).await?;

    let mut nar_info_content = String::new();
    nar_info_content.push_str(&format!("StorePath: {}
", path_info.path.as_os_str().to_string_lossy().to_string()));
    nar_info_content.push_str(&format!("URL: nar/{}.nar
", path_info.nar_hash.to_typed_base32()));
    nar_info_content.push_str("Compression: xz
"); // Assuming xz compression
    nar_info_content.push_str(&format!("NarHash: {}
", path_info.nar_hash.to_typed_base32()));
    nar_info_content.push_str(&format!("NarSize: {}
", path_info.nar_size));
    nar_info_content.push_str(&format!("References: {}
", path_info.references.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(" ")));
    if !path_info.sigs.is_empty() {
        nar_info_content.push_str(&format!("Sig: {}
", path_info.sigs.join(" ")));
    }
    if let Some(ca) = path_info.ca {
        nar_info_content.push_str(&format!("CA: {}
", ca));
    }

    Ok(nar_info_content)
}

/// Dumps a store path to NAR bytes in memory.
pub async fn dump_nar_to_bytes(store_path: &Path) -> Result<Vec<u8>> {
    let nix_store = NixStore::connect()?;
    let store_path_obj = nix_store.parse_store_path(store_path)?;

    let mut adapter = nix_store.nar_from_path(store_path_obj);

    let mut nar_bytes = Vec::new();
    while let Some(chunk) = adapter.next().await {
        nar_bytes.extend_from_slice(chunk?.as_slice()); // Fixed this line
    }

    Ok(nar_bytes)
}

/// Uploads the NAR and .narinfo for a given store path.
pub async fn upload_nar_for_path(uploader: Arc<S3Uploader>, path: &Path, config: &Config) -> Result<()> {
    let store_hash = path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.split('-').next().unwrap_or(""))
        .unwrap_or("");

    // 1. Dump the NAR to bytes in memory.
    let nar_bytes = dump_nar_to_bytes(path).await?;
    info!("Created NAR for '{}' in memory.", path.display());

    // 2. Generate and upload the .narinfo with retry logic.
    let mut nar_info_content = get_nar_info(path).await?;

    // Sign the path if signing is enabled.
    if let (Some(key_path), Some(_key_name)) = (&config.loft.signing_key_path, &config.loft.signing_key_name) {
        info!("Signing path '{}' with key from file '{}'.", path.display(), key_path.display());
        let key_file_content = fs::read_to_string(key_path)?;
        debug!("Read key file from '{}', content length: {}", key_path.display(), key_file_content.len());
        let nix_keypair = NixKeypair::from_str(&key_file_content)?;

        let mut nar_info = nix_manifest::from_str::<NarInfo>(&nar_info_content)?;
        nar_info.sign(&nix_keypair);
        nar_info_content = nix_manifest::to_string(&nar_info)?;
    }

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
                let _ = sleep(Duration::from_secs(5));
            }
        }
    }

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
                let _ = sleep(Duration::from_secs(5));
            }
        }
    }

    Ok(())
}