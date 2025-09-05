//! Watches the Nix store for changes and triggers uploads.

use anyhow::Result;
use futures::future::join_all;
use notify::{Error as NotifyError, Event, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tracing::{error, info};

use crate::cache_checker::CacheChecker;
use crate::config::Config;
use crate::local_cache::LocalCache;
use crate::nix_utils as nix;
use crate::s3_uploader::S3Uploader;
use attic::nix_store::NixStore;

// fn strip_lock_file(p: &Path) -> Option<PathBuf> {
//     p.to_str()
//         .and_then(|p| p.strip_suffix(".lock"))
//         .filter(|t| !t.ends_with(".drv") && !t.ends_with("-source"))
//         .map(PathBuf::from)
// }

/// Scans the Nix store for existing paths and uploads them.
pub async fn scan_and_process_existing_paths(
    uploader: Arc<S3Uploader>,
    local_cache: Arc<LocalCache>,
    config: &Config,
    force_scan: bool,
) -> Result<()> {
    info!("Starting scan of existing store paths...");
    let nix_store = NixStore::connect()?;

    // 1. Gather paths that are not signed by skipped keys
    let keys_to_skip = config.loft.skip_signed_by_keys.clone().unwrap_or_default();
    let all_sigs_map = nix::get_all_path_signatures().await?;
    let filtered_paths = nix::filter_out_sig_keys(all_sigs_map, keys_to_skip.clone()).await?;
    let filtered_paths_vec: Vec<String> = filtered_paths.keys().cloned().collect();

    // 2. Get the closure of the filtered paths
    let all_closure_paths: HashSet<String> =
        match nix::get_store_paths_closure(filtered_paths_vec).await {
            Ok(paths) => paths.into_iter().collect(),
            Err(e) => {
                error!("Failed to get closures: {:?}", e);
                HashSet::new()
            }
        };
    info!("Total unique closure paths: {}", all_closure_paths.len());

    // 3. Get signatures for the closure and filter again
    let closure_signatures =
        nix::get_path_signatures(all_closure_paths.into_iter().collect()).await?;
    let filtered_closure_paths =
        nix::filter_out_sig_keys(closure_signatures, keys_to_skip).await?;
    let filtered_closure_vec: Vec<String> = filtered_closure_paths.keys().cloned().collect();
    info!(
        "Total paths after filtering closure: {}",
        filtered_closure_vec.len()
    );

    // 4. Check caches (local + remote)
    let checker = CacheChecker::new(uploader.clone(), local_cache.clone(), config.clone());
    let result = checker
        .check_paths(&nix_store, filtered_closure_vec, force_scan)
        .await?;

    // 3. Upload missing
    if result.to_upload.is_empty() {
        info!("No missing paths to upload.");
        return Ok(());
    }

    info!("Found {} paths to upload.", result.to_upload.len());
    let semaphore = Arc::new(Semaphore::new(config.loft.upload_threads));
    let mut tasks = Vec::new();

    for (path_str, path_info) in result.to_upload {
        let uploader_clone = uploader.clone();
        let local_cache_clone = local_cache.clone();
        let config_clone = config.clone();
        let semaphore_clone = semaphore.clone();
        let plain_hash = path_info
            .nar_hash
            .to_typed_base32()
            .strip_prefix("sha256:")
            .unwrap_or_default()
            .to_string();

        tasks.push(tokio::spawn(async move {
            let permit = semaphore_clone.acquire_owned().await.unwrap();
            let p = Path::new(&path_str);

            if let Err(e) =
                nix::upload_nar_for_path(uploader_clone, p, &config_clone, &plain_hash).await
            {
                error!("Failed to upload path {}: {:?}", path_str, e);
            } else if let Err(e) = local_cache_clone.add_path_hash(&plain_hash) {
                error!("Failed to add path {} to local cache: {:?}", path_str, e);
            }
            drop(permit);
        }));
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
    let nix_store = NixStore::connect()?;
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

    watcher.watch(nix_store.store_dir(), RecursiveMode::NonRecursive)?;
    info!(
        "Watching {:?} for new store paths...",
        nix_store.store_dir()
    );

    while let Some(path) = rx.recv().await {
        let uploader_clone = uploader.clone();
        let local_cache_clone = local_cache.clone();
        let semaphore_clone = semaphore.clone();
        let config_for_task = config.clone(); // Clone config for the spawned task
        tokio::spawn(async move {
            let permit = semaphore_clone.acquire_owned().await.unwrap();
            if let Err(e) = process_path(
                uploader_clone,
                local_cache_clone,
                &path,
                &config_for_task,
                force_scan,
            )
            .await
            {
                error!("Failed to process '{}': {:?}", path.display(), e);
            }
            drop(permit);
        });
    }

    Ok(())
}

/// Processes a single store path for upload.
pub async fn process_path(
    uploader: Arc<S3Uploader>,
    local_cache: Arc<LocalCache>,
    path: &Path,
    config: &Config,
    force_scan: bool,
) -> Result<()> {
    let path_str = path.to_str().unwrap_or("").to_string();
    let nix_store = NixStore::connect()?;

    // 1. Get closure
    let closure = nix::get_store_path_closure(&path_str).await?;

    // 2. Skip signed-by keys if necessary
    if let Some(keys_to_skip) = &config.loft.skip_signed_by_keys {
        if let Some(sig_key) = nix::get_path_signature_key(&path_str).await? {
            if keys_to_skip.contains(&sig_key) {
                info!("Skipping {} (signed by {})", path.display(), sig_key);
                return Ok(());
            }
        }
    }

    // 3. Check caches
    let checker = CacheChecker::new(uploader.clone(), local_cache.clone(), config.clone());
    let result = checker.check_paths(&nix_store, closure, force_scan).await?;

    // 4. Upload missing
    for (path_str, path_info) in result.to_upload {
        let plain_hash = path_info
            .nar_hash
            .to_typed_base32()
            .strip_prefix("sha256:")
            .unwrap_or_default()
            .to_string();
        nix::upload_nar_for_path(uploader.clone(), Path::new(&path_str), config, &plain_hash)
            .await?;
        local_cache.add_path_hash(&plain_hash)?;
    }

    Ok(())
}
