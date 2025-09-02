//! Watches the Nix store for changes and triggers uploads.

use anyhow::Result;
use futures::future::join_all;
use notify::{Event, RecursiveMode, Error as NotifyError, Watcher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tracing::{error, info};

use crate::config::Config;
use crate::nix_utils as nix;
use crate::s3_uploader::S3Uploader;

const NIX_STORE_DIR: &str = "/nix/store";

/// Scans the Nix store for existing paths and uploads them.
pub async fn scan_and_process_existing_paths(
    uploader: Arc<S3Uploader>,
    config: &Config,
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
            let semaphore_clone = semaphore.clone();
            let config_for_task = config.clone(); // Clone config for the spawned task
            tasks.push(tokio::spawn(async move {
                let permit = semaphore_clone.acquire_owned().await.unwrap();
                if let Err(e) = process_path(uploader_clone, &path, &config_for_task).await {
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
    config: &Config,
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

    watcher.watch(Path::new(NIX_STORE_DIR), RecursiveMode::Recursive)?;
    info!("Watching {} for new store paths...", NIX_STORE_DIR);

    while let Some(path) = rx.recv().await {
        let uploader_clone = uploader.clone();
        let semaphore_clone = semaphore.clone();
        let config_for_task = config.clone(); // Clone config for the spawned task
        tokio::spawn(async move {
            let permit = semaphore_clone.acquire_owned().await.unwrap();
            if let Err(e) = process_path(uploader_clone, &path, &config_for_task).await {
                error!("Failed to process '{}': {:?}", path.display(), e);
            }
            drop(permit);
        });
    }

    Ok(())
}

/// Processes a single store path for upload.
async fn process_path(uploader: Arc<S3Uploader>, path: &Path, config: &Config) -> Result<()> {
    let path_str = path.to_str().unwrap_or("").to_string();

    // 1. Get the closure of the path.
    let closure = nix::get_store_path_closure(&path_str)?;

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

    // 2. Check which paths are missing from the cache.
    let missing_paths = uploader.check_paths_exist(&closure, config).await?;

    if missing_paths.is_empty() {
        info!("All paths in the closure of '{}' are already cached.", path.display());
        return Ok(());
    }

    info!("Found {} missing paths to upload.", missing_paths.len());

    // 3. Upload each missing path.
    for p_str in missing_paths {
        let p = Path::new(&p_str);
        // Upload NAR, then .narinfo
        nix::upload_nar_for_path(uploader.clone(), p, config).await?;
    }

    Ok(())
}
