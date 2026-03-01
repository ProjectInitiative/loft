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
    dry_run: bool,
) -> Result<()> {
    info!("Starting scan of existing store paths...");
    let nix_store = Arc::new(NixStore::connect()?);

    // 1. Gather paths that are not signed by skipped keys
    let keys_to_skip = config.loft.skip_signed_by_keys.clone().unwrap_or_default();
    let mut all_sigs_map = nix::get_all_path_signatures().await?;
    let initial_scanned_count = all_sigs_map.len();

    // 2. Identify "root" paths (paths we built or aren't signed by skip-keys)
    let root_paths_map = nix::filter_out_sig_keys(all_sigs_map.clone(), keys_to_skip.clone()).await?;
    let root_paths_vec: Vec<String> = root_paths_map.keys().cloned().collect();

    // 3. Expand closures of those root paths to ensure coverage
    let closure_paths: HashSet<String> = match nix::get_store_paths_closure(&root_paths_vec).await {
        Ok(paths) => paths.into_iter().collect(),
        Err(e) => {
            error!("Failed to get closures: {:?}", e);
            HashSet::new()
        }
    };

    // 4. Merge initial paths and closure paths into a master set
    let mut master_path_set: HashSet<String> = all_sigs_map.keys().cloned().collect();
    master_path_set.extend(closure_paths);

    // 5. Ensure we have signatures for EVERY path in the master set
    let mut missing_sigs_paths = Vec::new();
    for p in &master_path_set {
        if !all_sigs_map.contains_key(p) {
            missing_sigs_paths.push(p.clone());
        }
    }

    if !missing_sigs_paths.is_empty() {
        info!("Fetching signatures for {} newly discovered closure paths...", missing_sigs_paths.len());
        let extra_sigs = nix::get_path_signatures_bulk(&missing_sigs_paths).await?;
        all_sigs_map.extend(extra_sigs);
    }

    // 6. Master Filter: Filter the entire master set by signatures
    let filtered_master_map = nix::filter_out_sig_keys(all_sigs_map, keys_to_skip).await?;
    let num_filtered = filtered_master_map.len();
    let num_skipped_by_key = master_path_set.len() - num_filtered;

    info!(
        initial_scanned_count,
        master_set_size = master_path_set.len(),
        num_filtered,
        num_skipped_by_key,
        "Path discovery and filtering complete."
    );

    if filtered_master_map.is_empty() {
        info!("No paths kept after filtering. Nothing to do.");
        return Ok(());
    }

    let filtered_paths_vec: Vec<String> = filtered_master_map.keys().cloned().collect();

    // 7. Check caches (local + remote) BEFORE fetching signatures for the whole closure
    let checker = CacheChecker::new(uploader.clone(), local_cache.clone(), config.clone());
    let result = checker
        .check_paths(nix_store.as_ref(), &filtered_paths_vec, force_scan)
        .await?;

    info!(
        "Cache check complete: {} local cache hits, {} remote cache hits, {} paths missing from cache.",
        result.local_hits,
        result.remote_hits,
        result.to_upload.len()
    );

    if result.to_upload.is_empty() {
        info!("No missing paths to upload.");
        return Ok(());
    }

    if dry_run {
        info!("DRY RUN: The following {} paths would be uploaded:", result.to_upload.len());
        for path in &result.to_upload {
            info!("  DRY RUN: Would upload {}", path);
        }
        return Ok(());
    }

    // 8. Upload missing
    info!("Found {} paths to upload.", result.to_upload.len());
    let semaphore = Arc::new(Semaphore::new(config.loft.upload_threads));
    let mut tasks = Vec::new();

    for path_str in result.to_upload {
        let uploader_clone = uploader.clone();
        let config_clone = config.clone();
        let semaphore_clone = semaphore.clone();
        let permit = semaphore_clone.acquire_owned().await.unwrap();
        tasks.push(tokio::spawn(async move {
            let p = Path::new(&path_str).to_path_buf();
            let result = nix::upload_nar_for_path(uploader_clone, &p, &config_clone).await;

            let res = match result {
                Ok(_) => {
                    let path_hash =
                        crate::local_cache::LocalCache::extract_hash_from_path(&path_str).unwrap();
                    Some(path_hash)
                }
                Err(e) => {
                    error!("Failed to upload path {}: {:?}", path_str, e);
                    None
                }
            };
            drop(permit);
            res
        }));
    }

    let uploaded_hashes: Vec<String> = join_all(tasks)
        .await
        .into_iter()
        .filter_map(|res| res.ok().flatten())
        .collect();

    if !uploaded_hashes.is_empty() {
        if let Err(e) = local_cache.add_many_path_hashes(&uploaded_hashes) {
            error!("Failed to batch add paths to local cache: {:?}", e);
        } else {
            info!(
                "Successfully added {} paths to local cache.",
                uploaded_hashes.len()
            );
        }
    }
    Ok(())
}

use tokio_util::sync::CancellationToken;

/// Watches the Nix store and uploads new paths.
pub async fn watch_store(
    uploader: Arc<S3Uploader>,
    local_cache: Arc<LocalCache>,
    config: &Config,
    force_scan: bool,
    dry_run: bool,
    cancel_token: CancellationToken,
) -> Result<()> {
    let nix_store = NixStore::connect()?;
    let (tx, mut rx) = mpsc::channel(100);
    let semaphore = Arc::new(Semaphore::new(config.loft.upload_threads));
    let mut join_set = tokio::task::JoinSet::new();

    let mut watcher = notify::recommended_watcher(move |res: Result<Event, NotifyError>| {
        if let Ok(event) = res {
            for path in event.paths {
                if event.kind.is_remove() && path.extension().is_some_and(|e| e == "lock") {
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

    loop {
        tokio::select! {
            _ = cancel_token.cancelled() => {
                info!("Watcher received cancellation signal. Shutting down.");
                break;
            }
            path_opt = rx.recv() => {
                if let Some(path) = path_opt {
                    let uploader_clone = uploader.clone();
                    let local_cache_clone = local_cache.clone();
                    let semaphore_clone = semaphore.clone();
                    let config_for_task = config.clone(); // Clone config for the spawned task
                    join_set.spawn(async move {
                        let permit = semaphore_clone.acquire_owned().await.unwrap();
                        if let Err(e) = process_path(
                            uploader_clone,
                            local_cache_clone,
                            &path,
                            &config_for_task,
                            force_scan,
                            dry_run,
                        )
                        .await
                        {
                            error!("Failed to process '{}': {:?}", path.display(), e);
                        }
                        drop(permit);
                    });
                } else {
                    break;
                }
            }
        }
    }

    info!("Waiting for active uploads to finish...");
    while (join_set.join_next().await).is_some() {}

    Ok(())
}

/// Processes a single store path for upload.
pub async fn process_path(
    uploader: Arc<S3Uploader>,
    local_cache: Arc<LocalCache>,
    path: &Path,
    config: &Config,
    force_scan: bool,
    dry_run: bool,
) -> Result<()> {
    // Add a small delay to allow for follow-up operations like signing
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let path_str = path.to_str().unwrap_or("").to_string();

    // 1. Filter the path
    let keys_to_skip = config.loft.skip_signed_by_keys.clone().unwrap_or_default();
    let sigs_map = nix::get_path_signatures_bulk(std::slice::from_ref(&path_str)).await?;
    let filtered_paths = nix::filter_out_sig_keys(sigs_map, keys_to_skip.clone()).await?;

    if filtered_paths.is_empty() {
        info!("Skipping path: {}", path.display());
        return Ok(());
    }

    // 2. Get closure
    let closure = nix::get_store_path_closure(&path_str).await?;

    // 3. Check caches BEFORE fetching signatures
    let nix_store = Arc::new(NixStore::connect()?);
    let checker = CacheChecker::new(uploader.clone(), local_cache.clone(), config.clone());
    let mut result = checker
        .check_paths(nix_store.as_ref(), &closure, force_scan)
        .await?;

    if result.to_upload.is_empty() {
        info!("No missing paths to upload for {}.", path.display());
        return Ok(());
    }

    // 4. Get signatures for only the MISSING closure paths and filter again
    let closure_signatures = nix::get_path_signatures_bulk(&result.to_upload).await?;
    let filtered_closure_paths = nix::filter_out_sig_keys(closure_signatures, keys_to_skip).await?;
    let filtered_closure_vec: Vec<String> = filtered_closure_paths.keys().cloned().collect();
    info!(
        "Total paths after filtering missing closure for {}: {}",
        path.display(),
        filtered_closure_vec.len()
    );

    result.to_upload = filtered_closure_vec;

    if dry_run {
        info!("DRY RUN: The following {} paths would be uploaded for {}:", result.to_upload.len(), path.display());
        for p in result.to_upload {
            info!("  DRY RUN: Would upload {}", p);
        }
        return Ok(());
    }

    // 5. Upload missing
    let semaphore = Arc::new(Semaphore::new(config.loft.upload_threads));
    let mut tasks = Vec::new();

    for path_str in result.to_upload {
        let uploader_clone = uploader.clone();
        let config_clone = config.clone();
        let semaphore_clone = semaphore.clone();
        let path_str_clone = path_str.clone();

        tasks.push(tokio::spawn(async move {
            let _permit = semaphore_clone.acquire().await.unwrap();
            let p = Path::new(&path_str_clone).to_path_buf();
            let res = match nix::upload_nar_for_path(uploader_clone, &p, &config_clone).await {
                Ok(_) => {
                    let path_hash =
                        crate::local_cache::LocalCache::extract_hash_from_path(&path_str_clone)
                            .unwrap();
                    Some(path_hash)
                }
                Err(e) => {
                    error!("Failed to upload path {}: {:?}", path_str_clone, e);
                    None
                }
            };
            res
        }));
    }

    let uploaded_hashes: Vec<String> = futures::future::join_all(tasks)
        .await
        .into_iter()
        .filter_map(|res| res.ok().flatten())
        .collect();

    if !uploaded_hashes.is_empty() {
        if let Err(e) = local_cache.add_many_path_hashes(&uploaded_hashes) {
            error!(
                "Failed to batch add paths to local cache for {}: {:?}",
                path.display(),
                e
            );
        } else {
            info!(
                "Successfully added {} paths to local cache for {}.",
                uploaded_hashes.len(),
                path.display()
            );
        }
    }

    Ok(())
}
