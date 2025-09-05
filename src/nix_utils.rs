//! Utilities for interacting with the Nix command-line tools.

use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use futures::stream::StreamExt;
use serde_json::Value;

use std::io::{Read, Write};
use tokio::io::AsyncWriteExt;
use tempfile::NamedTempFile;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, info, warn};
use xz2::read::XzEncoder;
use zstd::stream::write::Encoder as ZstdEncoder;

use attic::nix_store::NixStore; // Keep NixStore
use attic::signing::NixKeypair; // Added this line

use crate::config::{Config, Compression};
use crate::nix_manifest::{self, NarInfo};
use crate::s3_uploader::S3Uploader;

#[derive(Debug, Clone)]
pub enum Signature {
    Crypto { key_name: String, signature: String },
    ContentAddressed { full_info: String },
}

pub async fn filter_out_sig_keys(
    sigs_map: HashMap<String, Vec<Signature>>,
    keys_to_skip: Vec<String>,
) -> Result<HashMap<String, Vec<Signature>>> {
    let keys_to_skip_set: HashSet<String> = keys_to_skip.into_iter().collect();

    // Find all paths that are NOT signed by any key in our skip set.
    let paths_to_process: HashMap<String, Vec<Signature>> = sigs_map
        .iter()
        .filter(|(path, _sig_vec)| !path.ends_with(".drv"))
        .filter(|(_path, sig_vec)| {
            // 3. ✅ Update the filter logic to check if the set contains the key.
            !sig_vec.iter().any(|sig| {
                if let Signature::Crypto { key_name, .. } = sig {
                    keys_to_skip_set.contains(key_name)
                } else {
                    false
                }
            })
        })
        .map(|(path, sig_vec)| (path.clone(), sig_vec.clone()))
        .collect();
    Ok(paths_to_process)
}

fn parse_path_signatures_from_json(json_str: &str) -> Result<HashMap<String, Vec<Signature>>> {
    let json: Value = serde_json::from_str(json_str)?;
    let mut path_signatures = HashMap::new();

    if let Value::Object(paths) = json {
        for (path, path_info) in paths {
            let mut signatures = Vec::new();

            if let Some(Value::Array(sigs)) = path_info.get("signatures") {
                for sig in sigs {
                    if let Value::String(sig_str) = sig {
                        if sig_str.starts_with("ca:") {
                            signatures.push(Signature::ContentAddressed {
                                full_info: sig_str.clone(),
                            });
                        } else if let Some((key, signature)) = sig_str.split_once(':') {
                            signatures.push(Signature::Crypto {
                                key_name: key.to_string(),
                                signature: signature.to_string(),
                            });
                        }
                    }
                }
            }

            path_signatures.insert(path, signatures);
        }
    }

    Ok(path_signatures)
}

pub async fn get_all_path_signatures() -> Result<HashMap<String, Vec<Signature>>> {
    info!("Fetching and parsing all path signatures...");
    let output = Command::new("nix")
        .arg("path-info")
        .arg("--sigs")
        .arg("--json")
        .arg("--all")
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("nix path-info failed: {}", stderr));
    }

    let stdout = String::from_utf8(output.stdout)?;
    let path_signatures = parse_path_signatures_from_json(&stdout)?;

    info!("Done. Found info for {} paths.", path_signatures.len());
    Ok(path_signatures)
}

pub async fn get_path_signatures(paths: Vec<String>) -> Result<HashMap<String, Vec<Signature>>> {
    info!(
        "Fetching and parsing signatures for {} paths...",
        paths.len()
    );

    let mut cmd = Command::new("nix");
    cmd.arg("path-info").arg("--sigs").arg("--json");

    // Add each path as a separate argument
    for path in &paths {
        cmd.arg(path);
    }

    let output = cmd.output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("nix path-info failed: {}", stderr));
    }

    let stdout = String::from_utf8(output.stdout)?;
    let path_signatures = parse_path_signatures_from_json(&stdout)?;

    info!("Done. Found info for {} paths.", path_signatures.len());
    Ok(path_signatures)
}

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

/// Gets the closure of a set of store paths.
pub async fn get_store_paths_closure(store_paths: Vec<String>) -> Result<Vec<String>> {
    let nix_store = NixStore::connect()?;
    let mut store_path_objs = Vec::new();
    for path_str in store_paths {
        store_path_objs.push(nix_store.parse_store_path(Path::new(&path_str))?);
    }

    let closure_paths = nix_store
        .compute_fs_closure_multi(store_path_objs, false, true, false)
        .await?;

    Ok(closure_paths
        .into_iter()
        .map(|p| nix_store.get_full_path(&p).to_string_lossy().to_string())
        .collect())
}

/// Gets the .narinfo for a store path.
pub async fn get_nar_info(store_path: &Path, config: &Config) -> Result<String> {
    let nix_store = NixStore::connect()?;
    let store_path_obj = nix_store.parse_store_path(store_path)?;

    let path_info = nix_store.query_path_info(store_path_obj).await?;

    let mut nar_info_content = String::new();
    nar_info_content.push_str(&format!(
        "StorePath: {}\n",
        path_info.path.as_os_str().to_string_lossy().to_string()
    ));
    // Update URL to reflect xz compression and nar/ subdirectory
    let compression_str = match config.loft.compression {
        Compression::Xz => "xz",
        Compression::Zstd => "zstd",
    };
    nar_info_content.push_str(&format!(
        "URL: nar/{}.nar.{}\n",
        path_info.nar_hash.to_typed_base32(),
        compression_str
    ));
    nar_info_content.push_str(&format!("Compression: {}\n", compression_str));
    nar_info_content.push_str(&format!(
        "NarHash: {}\n",
        path_info.nar_hash.to_typed_base32()
    ));
    nar_info_content.push_str(&format!("NarSize: {}\n", path_info.nar_size));
    nar_info_content.push_str(&format!(
        "References: {}\n",
        path_info
            .references
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    ));
    if !path_info.sigs.is_empty() {
        nar_info_content.push_str(&format!("Sig: {}\n", path_info.sigs.join(" ")));
    }
    if let Some(ca) = path_info.ca {
        nar_info_content.push_str(&format!("CA: {}\n", ca));
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
pub async fn upload_nar_for_path(
    uploader: Arc<S3Uploader>,
    path: &Path,
    config: &Config,
    nar_hash_str: &str,
) -> Result<()> {
    let nix_store = NixStore::connect()?;
    let store_path_obj = nix_store.parse_store_path(path)?;
    let path_info = nix_store.query_path_info(store_path_obj.clone()).await?;

    let use_disk = config.loft.use_disk_for_large_nars
        && (path_info.nar_size / 1024 / 1024) >= config.loft.large_nar_threshold_mb;

    let nar_key = format!("nar/{}.nar.xz", nar_hash_str);

    if use_disk {
        info!("Path '{}' is large, using on-disk NAR creation.", path.display());

        // 1. Create a temporary file for the NAR
        let nar_temp_file = NamedTempFile::new()?;
        let mut nar_file = tokio::fs::File::create(nar_temp_file.path()).await?;
        let mut adapter = nix_store.nar_from_path(store_path_obj);
        while let Some(chunk) = adapter.next().await {
            nar_file.write_all(chunk?.as_slice()).await?;
        }
        info!("Created NAR for '{}' on disk.", path.display());

        // 2. Create a temporary file for the compressed NAR
        let compressed_temp_file = NamedTempFile::new()?;
        let compressed_path = compressed_temp_file.path().to_path_buf();
        let nar_path = nar_temp_file.path().to_path_buf();
        let compression_type = config.loft.compression; // Capture compression type

        let nar_path_clone = nar_path.clone(); // Clone here
        tokio::task::spawn_blocking(move || {
            
            let compressed_file = std::fs::File::create(compressed_path)?;
            let mut compressed_writer = std::io::BufWriter::new(compressed_file);

            match compression_type {
                Compression::Xz => {
                    let nar_file = std::fs::File::open(&nar_path_clone)?; // Use clone here
                    let mut encoder = XzEncoder::new(nar_file, 9);
                    std::io::copy(&mut encoder, &mut compressed_writer)?;
                }
                Compression::Zstd => {
                    let mut nar_file = std::fs::File::open(&nar_path_clone)?; // Use clone here
                    let mut encoder = ZstdEncoder::new(compressed_writer, 0)?; // Write to the output file
                    std::io::copy(&mut nar_file, &mut encoder)?;
                    encoder.finish()?; // Flush remaining compressed data
                }
            }
            Ok::<(), anyhow::Error>(())
        }).await??;
        info!("Compressed NAR for '{}' on disk with {:?}.", path.display(), config.loft.compression);

        // 3. Upload the compressed NAR from disk
        let mut attempts = 0;
        loop {
            attempts += 1;
            match uploader.upload_file(compressed_temp_file.path(), &nar_key).await {
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
    } else {
        // 1. Dump the NAR to bytes in memory.
        let nar_bytes = dump_nar_to_bytes(path).await?;
        info!("Created NAR for '{}' in memory.", path.display());

        // 2. Compress the NAR bytes in a blocking thread.
        let config_clone = config.clone();
        let compressed_nar_bytes = tokio::task::spawn_blocking(move || {
            let compressed_bytes = match config_clone.loft.compression {
                Compression::Xz => {
                    let mut encoder = XzEncoder::new(&nar_bytes[..], 9);
                    let mut compressed = Vec::new();
                    encoder.read_to_end(&mut compressed)?;
                    compressed
                }
                Compression::Zstd => {
                    let compressed = Vec::new();
                    let mut encoder = ZstdEncoder::new(compressed, 0)?;
                    encoder.write_all(&nar_bytes[..])?;
                    encoder.finish()? // This returns the Vec<u8>
                }
            };
            Ok::<_, anyhow::Error>(compressed_bytes)
        }).await??;
        info!("Compressed NAR for '{}' with {:?}.", path.display(), config.loft.compression);

        // 3. Upload the compressed NAR from memory
        let mut attempts = 0;
        loop {
            attempts += 1;
            match uploader
                .upload_bytes(compressed_nar_bytes.clone(), &nar_key)
                .await
            {
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
    }

    // 4. Generate and upload the .narinfo with retry logic.
    let mut nar_info_content = get_nar_info(path, config).await?;

    // Sign the path if signing is enabled.
    if let (Some(key_path), Some(_key_name)) =
        (&config.loft.signing_key_path, &config.loft.signing_key_name)
    {
        if !key_path.exists() {
            warn!(
                "Signing key file '{}' not found. Skipping signing.",
                key_path.display()
            );
        } else {
            info!(
                "Signing path '{}' with key from file '{}'.",
                path.display(),
                key_path.display()
            );
            let key_file_content = fs::read_to_string(key_path)?;
            debug!(
                "Read key file from '{}', content length: {}",
                key_path.display(),
                key_file_content.len()
            );
            let nix_keypair = NixKeypair::from_str(&key_file_content)?;

            let mut nar_info = nix_manifest::from_str::<NarInfo>(&nar_info_content)?;
            nar_info.sign(&nix_keypair);
            nar_info_content = nix_manifest::to_string(&nar_info)?;
        }
    }

    // The .narinfo filename should be the hash of the store path it describes
    let narinfo_key = format!("{}.narinfo", nar_hash_str);
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

/// Gets the closure of a store path.
pub async fn get_store_path_closure(store_path: &str) -> Result<Vec<String>> {
    get_store_paths_closure(vec![store_path.to_string()]).await
}
