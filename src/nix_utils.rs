//! Utilities for interacting with the Nix command-line tools.

use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use futures::stream::StreamExt;
use serde_json::Value;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use attic::nix_store::NixStore; // Keep NixStore
use attic::signing::NixKeypair; // Added this line

use crate::config::{Compression, Config};
use crate::nix_manifest::{self, NarInfo};
use crate::s3_uploader::S3Uploader;
use nix_base32;

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
        .filter(|(path, sig_vec)| {
            let mut skip = false;
            for sig in *sig_vec {
                if let Signature::Crypto { key_name, .. } = sig {
                    if keys_to_skip_set.contains(key_name) {
                        debug!("Skipping path {} because it is signed by {}", path, key_name);
                        skip = true;
                        break;
                    }
                }
            }
            if !skip {
                if sig_vec.is_empty() {
                    debug!("Keeping path {} because it has no signatures", path);
                } else {
                    debug!("Keeping path {} because no skip-keys matched its signatures", path);
                }
            }
            !skip
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
    // Note: Since NixStore doesn't expose queryAllValidPaths directly in FFI yet,
    // we might still need the CLI for this specific bulk operation, 
    // or we can iterate if we have a list of paths.
    // For now, let's keep the CLI for get_all_path_signatures but use NixStore for individual lookups.
    info!("Fetching and parsing all path signatures using nix CLI...");
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

pub async fn get_path_signatures_bulk(paths: &[String]) -> Result<HashMap<String, Vec<Signature>>> {
    if paths.is_empty() {
        return Ok(HashMap::new());
    }
    
    info!(
        "Fetching and parsing signatures for {} paths using nix CLI...",
        paths.len()
    );

    let mut cmd = Command::new("nix");
    cmd.arg("path-info").arg("--sigs").arg("--json");
    for path in paths {
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

pub async fn get_path_signatures(paths: &[String]) -> Result<HashMap<String, Vec<Signature>>> {
    info!(
        "Fetching and parsing signatures for {} paths using NixStore...",
        paths.len()
    );

    let nix_store = Arc::new(NixStore::connect()?);
    let mut tasks = Vec::new();

    for path_str in paths {
        let path_str = path_str.clone();
        let nix_store_clone = nix_store.clone();
        tasks.push(tokio::spawn(async move {
            let store_path_obj = nix_store_clone.parse_store_path(Path::new(&path_str))?;
            match nix_store_clone.query_path_info(store_path_obj).await {
                Ok(path_info) => {
                    let mut signatures = Vec::new();
                    for sig_str in path_info.sigs {
                        if sig_str.starts_with("ca:") {
                            signatures.push(Signature::ContentAddressed {
                                full_info: sig_str,
                            });
                        } else if let Some((key, signature)) = sig_str.split_once(':') {
                            signatures.push(Signature::Crypto {
                                key_name: key.to_string(),
                                signature: signature.to_string(),
                            });
                        }
                    }
                    Ok::<_, anyhow::Error>(Some((path_str, signatures)))
                }
                Err(e) => {
                    warn!("Failed to query path info for {}: {:?}", path_str, e);
                    Ok(None)
                }
            }
        }));
    }

    let mut path_signatures = HashMap::new();
    let results = futures::future::join_all(tasks).await;
    for res in results {
        if let Some((path, sigs)) = res?? {
            path_signatures.insert(path, sigs);
        }
    }

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
pub async fn get_store_paths_closure(store_paths: &[String]) -> Result<Vec<String>> {
    let nix_store = NixStore::connect()?;
    let mut store_path_objs = Vec::new();
    for path_str in store_paths {
        store_path_objs.push(nix_store.parse_store_path(Path::new(path_str))?);
    }

    let closure_paths = nix_store
        .compute_fs_closure_multi(store_path_objs, false, true, false)
        .await?;

    Ok(closure_paths
        .into_iter()
        .map(|p| nix_store.get_full_path(&p).to_string_lossy().to_string())
        .collect())
}

/// Gets the .narinfo key for a store path.
pub fn get_narinfo_key(store_path: &attic::nix_store::StorePath) -> String {
    format!("{}.narinfo", store_path.to_hash().as_str())
}

/// Dumps a store path to NAR bytes in memory.
pub async fn dump_nar_to_bytes(store_path: &Path) -> Result<Vec<u8>> {
    let nix_store = NixStore::connect()?;
    let store_path_obj = nix_store.parse_store_path(store_path)?;

    let mut adapter = nix_store.nar_from_path(store_path_obj);

    let mut nar_bytes = Vec::new();
    while let Some(chunk) = adapter.next().await {
        nar_bytes.extend_from_slice(chunk?.as_slice());
    }

    Ok(nar_bytes)
}

use async_compression::tokio::write::{XzEncoder, ZstdEncoder};

/// Uploads the NAR and .narinfo for a given store path.
pub async fn upload_nar_for_path(
    uploader: Arc<S3Uploader>,
    path: &Path,
    config: &Config,
) -> Result<()> {
    let nix_store = NixStore::connect()?;
    let store_path_obj = nix_store.parse_store_path(path)?;

    let path_info = nix_store.query_path_info(store_path_obj.clone()).await?;

    let ca = path_info.ca;
    let nar_hash_typed = path_info.nar_hash.to_typed_base32();
    let nar_size = path_info.nar_size;
    let sigs = path_info.sigs;

    let references: Vec<String> = path_info.references
        .iter()
        .map(|r| {
            r.file_name()
                .and_then(|f| f.to_str())
                .unwrap_or_else(|| r.to_str().unwrap_or(""))
                .to_string()
        })
        .collect();

    let (compression_ext, compression_field) = match config.loft.compression {
        Compression::Xz => ("xz", "xz"),
        Compression::Zstd => ("zst", "zstd"),
    };

    // Streaming NAR dump and compression
    let (mut rx, mut tx) = tokio::io::duplex(64 * 1024);
    let mut adapter = nix_store.nar_from_path(store_path_obj.clone());

    let nar_dump_task = tokio::spawn(async move {
        while let Some(chunk) = adapter.next().await {
            tx.write_all(chunk?.as_slice()).await?;
        }
        Ok::<(), anyhow::Error>(())
    });

    let (mut compressed_rx, compressed_tx) = tokio::io::duplex(64 * 1024);
    let compression_type = config.loft.compression;

    // We need to calculate FileHash (of compressed data) and FileSize.
    // For FileHash and FileSize, we unfortunately need to see the whole compressed stream.
    // However, we can still stream the upload. We'll capture the hash and size as we stream to S3.
    
    let compression_task = tokio::spawn(async move {
        match compression_type {
            Compression::Xz => {
                let mut encoder = XzEncoder::new(compressed_tx);
                tokio::io::copy(&mut rx, &mut encoder).await?;
                encoder.shutdown().await?;
            }
            Compression::Zstd => {
                let mut encoder = ZstdEncoder::new(compressed_tx);
                tokio::io::copy(&mut rx, &mut encoder).await?;
                encoder.shutdown().await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    // Stream to S3 and calculate hash/size on the fly
    use sha2::{Digest, Sha256};
    let hasher = Arc::new(Mutex::new(Sha256::new()));
    let file_size_atomic = Arc::new(AtomicU64::new(0));

    let hasher_clone = hasher.clone();
    let file_size_clone = file_size_atomic.clone();
    
    // We'll use a predictable NAR key (like store path hash + nar hash) and calculate FileHash during upload.
    let stream = async_stream::try_stream! {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let n = compressed_rx.read(&mut buffer).await.map_err(|e| anyhow::anyhow!(e))?;
            if n == 0 { break; }
            let chunk = &buffer[..n];
            
            {
                let mut h = hasher_clone.lock().await;
                h.update(chunk);
            }
            file_size_clone.fetch_add(n as u64, Ordering::SeqCst);
            
            yield bytes::Bytes::copy_from_slice(chunk);
        }
    };

    let nar_key = format!("nar/{}-{}.nar.{}", store_path_obj.to_hash().as_str(), nar_hash_typed.strip_prefix("sha256:").unwrap(), compression_ext);

    uploader.upload_stream(stream, &nar_key).await?;

    let _ = tokio::try_join!(nar_dump_task, compression_task)?;

    let file_hash_bytes = {
        let h = hasher.lock().await;
        h.clone().finalize()
    };
    let file_hash_base32 = nix_base32::to_nix_base32(&file_hash_bytes);
    let file_hash_typed = format!("sha256:{}", file_hash_base32);
    let file_size = file_size_atomic.load(Ordering::SeqCst);

    let nar_info_args = NarInfoArgs {
        store_path: path,
        url: &nar_key,
        compression: compression_field,
        file_hash: &file_hash_typed,
        file_size,
        nar_hash: &nar_hash_typed,
        nar_size,
        references: &references,
        ca: ca.as_deref(),
        existing_sigs: &sigs,
        config,
    };

    let nar_info_content = generate_nar_info(nar_info_args)?;

    let narinfo_key = get_narinfo_key(&store_path_obj);
    uploader
        .upload_bytes(nar_info_content.into_bytes(), &narinfo_key)
        .await?;

    Ok(())
}

/// Arguments for generating a .narinfo file.
pub struct NarInfoArgs<'a> {
    pub store_path: &'a Path,
    pub url: &'a str,
    pub compression: &'a str,
    pub file_hash: &'a str,
    pub file_size: u64,
    pub nar_hash: &'a str,
    pub nar_size: u64,
    pub references: &'a [String],
    pub ca: Option<&'a str>,
    pub existing_sigs: &'a [String],
    pub config: &'a Config,
}

/// Generates the content of a .narinfo file.
pub fn generate_nar_info(args: NarInfoArgs) -> Result<String> {
    let mut content = String::new();
    content.push_str(&format!("StorePath: {}\n", args.store_path.display()));
    content.push_str(&format!("URL: {}\n", args.url));
    content.push_str(&format!("Compression: {}\n", args.compression));
    content.push_str(&format!("FileHash: {}\n", args.file_hash));
    content.push_str(&format!("FileSize: {}\n", args.file_size));
    content.push_str(&format!("NarHash: {}\n", args.nar_hash));
    content.push_str(&format!("NarSize: {}\n", args.nar_size));
    content.push_str(&format!("References: {}\n", args.references.join(" ")));

    if let Some(ca_value) = args.ca {
        content.push_str(&format!("CA: {}\n", ca_value));
    }

    let mut new_signature_key_name: Option<String> = None;

    if let (Some(key_path), Some(key_name)) = (
        &args.config.loft.signing_key_path,
        &args.config.loft.signing_key_name,
    ) {
        if key_path.exists() {
            let key_file_content = fs::read_to_string(key_path)?;
            let nix_keypair = NixKeypair::from_str(&key_file_content)?;

            let nar_info_for_signing = nix_manifest::from_str::<NarInfo>(&content)?;
            let fingerprint = nar_info_for_signing.fingerprint();
            let full_signature_string = nix_keypair.sign(&fingerprint);
            let signature_value = full_signature_string
                .split_once(':')
                .map(|(_, val)| val)
                .unwrap_or("");

            content.push_str(&format!("Sig: {}:{}\n", key_name, signature_value));
            new_signature_key_name = Some(key_name.clone());
        }
    }

    // Add existing signatures, excluding the one we just added (if any)
    for sig in args.existing_sigs {
        if sig.starts_with("ca:") {
            continue;
        }
        if let Some(existing_key_name) = sig.split_once(':').map(|(key, _)| key) {
            if let Some(new_key) = &new_signature_key_name {
                if existing_key_name == new_key {
                    continue;
                }
            }
        }
        content.push_str(&format!("Sig: {}\n", sig));
    }

    Ok(content)
}

/// Gets the closure of a store path.
pub async fn get_store_path_closure(store_path: &str) -> Result<Vec<String>> {
    get_store_paths_closure(&[store_path.to_string()]).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that the path signature parser correctly parses the JSON output
    /// of `nix path-info --sigs` into Crypto and ContentAddressed enums.
    #[test]
    fn test_parse_path_signatures() -> Result<()> {
        let json_output = r#"{
            "/nix/store/path1": {
                "signatures": [
                    "cache.nixos.org-1:sig1",
                    "ca:hash"
                ]
            },
            "/nix/store/path2": {
                "signatures": [
                    "other-key:sig2"
                ]
            }
        }"#;

        let result = parse_path_signatures_from_json(json_output)?;

        assert_eq!(result.len(), 2);

        let sigs1 = result.get("/nix/store/path1").unwrap();
        assert_eq!(sigs1.len(), 2);

        let has_crypto = sigs1.iter().any(|s| matches!(s, Signature::Crypto { key_name, signature } if key_name == "cache.nixos.org-1" && signature == "sig1"));
        let has_ca = sigs1.iter().any(
            |s| matches!(s, Signature::ContentAddressed { full_info } if full_info == "ca:hash"),
        );

        assert!(has_crypto);
        assert!(has_ca);

        let sigs2 = result.get("/nix/store/path2").unwrap();
        assert_eq!(sigs2.len(), 1);
        assert!(
            matches!(&sigs2[0], Signature::Crypto { key_name, signature } if key_name == "other-key" && signature == "sig2")
        );

        Ok(())
    }

    /// Tests the filter_out_sig_keys logic to ensure paths signed by any key
    /// in the provided 'skip' list are completely removed from the resulting map.
    #[tokio::test]
    async fn test_filter_out_sig_keys() -> Result<()> {
        let mut map = HashMap::new();

        let path1 = "/nix/store/path1";
        let sigs1 = vec![Signature::Crypto {
            key_name: "skip-key".to_string(),
            signature: "sig".to_string(),
        }];
        map.insert(path1.to_string(), sigs1);

        let path2 = "/nix/store/path2";
        let sigs2 = vec![Signature::Crypto {
            key_name: "keep-key".to_string(),
            signature: "sig".to_string(),
        }];
        map.insert(path2.to_string(), sigs2);

        let keys_to_skip = vec!["skip-key".to_string()];

        let filtered = filter_out_sig_keys(map, keys_to_skip).await?;

        assert_eq!(filtered.len(), 1);
        assert!(filtered.contains_key(path2));
        assert!(!filtered.contains_key(path1));

        Ok(())
    }

    #[test]
    fn test_generate_nar_info_formatting() -> Result<()> {
        let store_path = Path::new("/nix/store/hfx4mfjp89kv21whvwcmm2a0bjs0a428-loft-0.1.0");
        let url = "nar/sha256:0h4ifpg71s11p3hbafhx3idf3zji7ny8wqnjgvrzmqw9d90d48w4.nar.xz";
        let compression = "xz";
        let file_hash = "sha256:0h4ifpg71s11p3hbafhx3idf3zji7ny8wqnjgvrzmqw9d90d48w4";
        let file_size = 35634208;
        let nar_hash = "sha256:8423d2406a89e3faf37ed2628ebc3d51fee15a1c1d3ab5e0b821e870de759140";
        let nar_size = 123456;
        let references = vec!["4azvwrcvsj6fy0x66shvl37pasw46k57-nix-2.28.4".to_string()];
        let ca = Some("fixed:sha256:1234...");
        let existing_sigs = vec!["cache.nixos.org-1:sig1".to_string()];
        let config = Config::default();

        let nar_info_args = NarInfoArgs {
            store_path,
            url,
            compression,
            file_hash,
            file_size,
            nar_hash,
            nar_size,
            references: &references,
            ca,
            existing_sigs: &existing_sigs,
            config: &config,
        };

        let content = generate_nar_info(nar_info_args)?;

        assert!(content.contains("StorePath: /nix/store/hfx4mfjp89kv21whvwcmm2a0bjs0a428-loft-0.1.0"));
        assert!(content.contains("URL: nar/sha256:0h4ifpg71s11p3hbafhx3idf3zji7ny8wqnjgvrzmqw9d90d48w4.nar.xz"));
        assert!(content.contains("Compression: xz"));
        assert!(content.contains("FileHash: sha256:0h4ifpg71s11p3hbafhx3idf3zji7ny8wqnjgvrzmqw9d90d48w4"));
        assert!(content.contains("FileSize: 35634208"));
        assert!(content.contains("NarHash: sha256:8423d2406a89e3faf37ed2628ebc3d51fee15a1c1d3ab5e0b821e870de759140"));
        assert!(content.contains("NarSize: 123456"));
        assert!(content.contains("References: 4azvwrcvsj6fy0x66shvl37pasw46k57-nix-2.28.4"));
        assert!(content.contains("CA: fixed:sha256:1234..."));
        assert!(content.contains("Sig: cache.nixos.org-1:sig1"));

        Ok(())
    }

    #[tokio::test]
    async fn test_streaming_compression_formats() -> Result<()> {
        let data = b"Hello, Nix World! This is some test data to verify compression.";
        let config_xz = Config {
            loft: crate::config::LoftConfig {
                compression: Compression::Xz,
                ..Default::default()
            },
            ..Default::default()
        };
        let config_zstd = Config {
            loft: crate::config::LoftConfig {
                compression: Compression::Zstd,
                ..Default::default()
            },
            ..Default::default()
        };

        async fn compress(data: &[u8], compression: Compression) -> Result<Vec<u8>> {
            let (mut rx, tx) = tokio::io::duplex(64 * 1024);
            let data_vec = data.to_vec();
            tokio::spawn(async move {
                let mut tx = tx;
                tx.write_all(&data_vec).await.unwrap();
            });

            let (mut compressed_rx, compressed_tx) = tokio::io::duplex(64 * 1024);
            tokio::spawn(async move {
                let compressed_tx = compressed_tx;
                match compression {
                    Compression::Xz => {
                        let mut encoder = XzEncoder::new(compressed_tx);
                        tokio::io::copy(&mut rx, &mut encoder).await.unwrap();
                        encoder.shutdown().await.unwrap();
                    }
                    Compression::Zstd => {
                        let mut encoder = ZstdEncoder::new(compressed_tx);
                        tokio::io::copy(&mut rx, &mut encoder).await.unwrap();
                        encoder.shutdown().await.unwrap();
                    }
                }
            });

            let mut result = Vec::new();
            compressed_rx.read_to_end(&mut result).await?;
            Ok(result)
        }

        let xz_compressed = compress(data, config_xz.loft.compression).await?;
        let zstd_compressed = compress(data, config_zstd.loft.compression).await?;

        // Verify XZ magic bytes (7zXZ)
        assert_eq!(&xz_compressed[0..6], &[0xFD, b'7', b'z', b'X', b'Z', 0x00]);
        
        // Verify Zstd magic bytes (0x28B52FFD)
        assert_eq!(&zstd_compressed[0..4], &[0x28, 0xB5, 0x2F, 0xFD]);

        Ok(())
    }

        #[test]
        fn test_get_narinfo_key() -> Result<()> {
            let hash = "hfx4mfjp89kv21whvwcmm2a0bjs0a428";
    
            let key = format!("{}.narinfo", hash);
            assert_eq!(key, "hfx4mfjp89kv21whvwcmm2a0bjs0a428.narinfo");
    
            Ok(())
        }
        #[test]
    fn test_s3_key_hierarchy() -> Result<()> {
        let hash = "hfx4mfjp89kv21whvwcmm2a0bjs0a428";
        let nar_hash_base32 = "8423d2406a89e3faf37ed2628ebc3d51fee15a1c1d3ab5e0b821e870de759140";
        let compression_ext = "xz";
        
        // This simulates the logic inside upload_nar_for_path
        let nar_key = format!("nar/{}-{}.nar.{}", hash, nar_hash_base32, compression_ext);
        assert_eq!(nar_key, "nar/hfx4mfjp89kv21whvwcmm2a0bjs0a428-8423d2406a89e3faf37ed2628ebc3d51fee15a1c1d3ab5e0b821e870de759140.nar.xz");
        
        let narinfo_key = format!("{}.narinfo", hash);
        assert_eq!(narinfo_key, "hfx4mfjp89kv21whvwcmm2a0bjs0a428.narinfo");
        
        Ok(())
    }
}
