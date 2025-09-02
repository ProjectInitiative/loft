//! Watches the Nix store for changes and triggers uploads.

use anyhow::Result;
use futures::future::join_all;
use notify::{Event, RecursiveMode, Error as NotifyError, Watcher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tracing::{error, info, debug};

use attic::nix_store::NixStore;
use crate::config::Config;
use crate::local_cache::LocalCache;
use crate::nix_utils as nix;
use crate::s3_uploader::S3Uploader;

const NIX_STORE_DIR: &str = "/nix/store";

/// Scans the Nix store for existing paths and uploads them.
pub async fn scan_and_process_existing_paths(
    uploader: Arc<S3Uploader>,
    local_cache: Arc<LocalCache>,
    config: &Config,
    force_scan: bool,
) -> Result<()> {
    let semaphore = Arc::new(Semaphore::new(config.loft.upload_threads));
    let store_dir = Path::new(NIX_STORE_DIR);

    let mut tasks = Vec::new();

    for entry in std::fs::read_dir(store_dir)? {
        let entry = entry?;
        let path = entry.path();
        // We are only interested in store paths, which are directories.
        if path.is_dir() {
            let uploader_clone = uploader.clone();
            let local_cache_clone = local_cache.clone();
            let semaphore_clone = semaphore.clone();
            let config_for_task = config.clone(); // Clone config for the spawned task
            tasks.push(tokio::spawn(async move {
                let permit = semaphore_clone.acquire_owned().await.unwrap();
                if let Err(e) = process_path(uploader_clone, local_cache_clone, &path, &config_for_task, force_scan).await {
                    error!("Failed to process '{}': {:?}", path.display(), e);
                }
                drop(permit);
            }));
        }
    }

    join_all(tasks).await;

    Ok(())
}

/// Watches the Nix store and uploads new paths.
pub async fn watch_store(
    uploader: Arc<S3Uploader>,
    local_cache: Arc<LocalCache>,
    config: &Config,
    force_scan: bool,
) -> Result<()> {
    let (tx, mut rx) = mpsc::channel(100);
    let semaphore = Arc::new(Semaphore::new(config.loft.upload_threads));

    

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, NotifyError>| {
        if let Ok(event) = res {
            for path in event.paths {
                if event.kind.is_remove() && path.extension().map_or(false, |e| e == "lock") {
                    if let Some(path_str) = path.to_str() {
                        let store_path = path_str.trim_end_matches(".lock");
                        info!("Detected new store path: {}", store_path);
                        if let Err(e) = tx.blocking_send(PathBuf::from(store_path)) {
                            error!("Failed to send path for processing: {}", e);
                        }
                    }
                }
            }
        }
    })?;

    watcher.watch(Path::new(NIX_STORE_DIR), RecursiveMode::NonRecursive)?;
    info!("Watching {} for new store paths...", NIX_STORE_DIR);

    while let Some(path) = rx.recv().await {
        let uploader_clone = uploader.clone();
        let local_cache_clone = local_cache.clone();
        let semaphore_clone = semaphore.clone();
        let config_for_task = config.clone(); // Clone config for the spawned task
        tokio::spawn(async move {
            let permit = semaphore_clone.acquire_owned().await.unwrap();
            if let Err(e) = process_path(uploader_clone, local_cache_clone, &path, &config_for_task, force_scan).await {
                error!("Failed to process '{}': {:?}", path.display(), e);
            }
            drop(permit);
        });
    }

    Ok(())
}

/// Processes a single store path for upload.
async fn process_path(
    uploader: Arc<S3Uploader>,
    local_cache: Arc<LocalCache>,
    path: &Path,
    config: &Config,
    force_scan: bool,
) -> Result<()> {
    let path_str = path.to_str().unwrap_or("").to_string();
    let nix_store = NixStore::connect()?;

    debug!("Processing path: {}", path.display());

    // 1. Get the closure of the path.
    let closure = nix::get_store_path_closure(&path_str)?;
    debug!("Closure for {}: {:?}", path.display(), closure);

    // Check if the path is signed by a key that should be skipped.
    if let Some(keys_to_skip) = &config.loft.skip_signed_by_keys {
        if let Some(path_signature_key) = nix::get_path_signature_key(&path_str).await? {
            if keys_to_skip.contains(&path_signature_key) {
                info!(
                    "Skipping '{}' as it was signed by key '{}', which is in the skip list.",
                    path.display(),
                    path_signature_key
                );
                return Ok(());
            }
        }
    }

    // 2. Check which paths are missing from the local cache.
    let mut closure_hashes = Vec::new();
    let mut closure_path_infos = std::collections::HashMap::new();
    for p_str in &closure {
        let p = Path::new(p_str);
        let store_path = nix_store.parse_store_path(p)?;
        let path_info = nix_store.query_path_info(store_path).await?;
        // Store plain hash in closure_hashes
        let plain_hash = path_info.nar_hash.to_typed_base32().strip_prefix("sha256:").unwrap_or_default().to_string();
        closure_hashes.push(plain_hash.clone());
        closure_path_infos.insert(p_str.clone(), path_info);
    }
    debug!("Closure hashes for {}: {:?}", path.display(), closure_hashes);

    let missing_paths_from_local: Vec<String> = if force_scan {
        debug!("Force scan enabled, bypassing local cache check for {}", path.display());
        closure.clone()
    } else {
        let existing_hashes = local_cache.find_existing_hashes(&closure_hashes)?;
        debug!("Existing hashes in local cache for {}: {:?}", path.display(), existing_hashes);
        closure
            .into_iter()
            .filter(|p| {
                let path_info = closure_path_infos.get(p).unwrap();
                // Compare with plain hash
                let plain_hash = path_info.nar_hash.to_typed_base32().strip_prefix("sha256:").unwrap_or_default().to_string();
                let is_missing = !existing_hashes.contains(&plain_hash);
                if !is_missing {
                    debug!("Path {} (hash {}) found in local cache.", p, plain_hash);
                }
                is_missing
            })
            .collect()
    };

    if missing_paths_from_local.is_empty() && !force_scan {
        info!(
            "All paths in the closure of '{}' are already in the local cache.",
            path.display()
        );
        return Ok(());
    }

    // 3. Check which of the remaining paths are missing from the remote cache.
    debug!("Checking remote cache for missing paths from local for {}: {:?}", path.display(), missing_paths_from_local);
    let (missing_paths_from_remote, found_paths_from_remote) = uploader
        .check_paths_exist(&missing_paths_from_local, config)
        .await?;
    debug!("Missing paths from remote for {}: {:?}", path.display(), missing_paths_from_remote);
    debug!("Found paths from remote for {}: {:?}", path.display(), found_paths_from_remote);

    // Add the paths found in the remote cache to the local cache.
    for p_str in found_paths_from_remote {
        let path_info = closure_path_infos.get(&p_str).unwrap();
        // Store plain hash in local cache
        let plain_hash = path_info.nar_hash.to_typed_base32().strip_prefix("sha256:").unwrap_or_default().to_string();
        debug!("Adding remotely found path {} (hash {}) to local cache.", p_str, plain_hash);
        local_cache.add_path_hash(&plain_hash)?;
    }

    if missing_paths_from_remote.is_empty() {
        info!(
            "All paths in the closure of '{}' are already in the remote cache.",
            path.display()
        );
        // Add the paths to the local cache so we don't check them again.
        debug!("Adding all closure hashes to local cache for {}: {:?}", path.display(), closure_hashes);
        local_cache.add_many_path_hashes(&closure_hashes)?;
        return Ok(());
    }

    info!(
        "Found {} missing paths to upload.",
        missing_paths_from_remote.len()
    );

    // 4. Upload each missing path.
    for p_str in missing_paths_from_remote {
        let p = Path::new(&p_str);
        debug!("Uploading path: {}", p.display());
        // Upload NAR, then .narinfo
        nix::upload_nar_for_path(uploader.clone(), p, config).await?;
        // Add the path to the local cache.
        let path_info = closure_path_infos.get(&p_str).unwrap();
        // Store plain hash in local cache
        let plain_hash = path_info.nar_hash.to_typed_base32().strip_prefix("sha256:").unwrap_or_default().to_string();
        debug!("Adding uploaded path {} (hash {}) to local cache.", p.display(), plain_hash);
        local_cache.add_path_hash(&plain_hash)?;
    }

    Ok(())
}
