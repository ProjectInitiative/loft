//! Watches the Nix store for changes and triggers uploads.

use anyhow::Result;
use futures::stream::StreamExt;
use notify::{RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};
use tracing::{error, info, warn};

use crate::nix;
use crate::s3::S3Uploader;

const NIX_STORE_DIR: &str = "/nix/store";

/// Watches the Nix store and uploads new paths.
pub async fn watch_store(uploader: S3Uploader, max_concurrency: usize) -> Result<()> {
    let (tx, mut rx) = mpsc::channel(100);
    let uploader = Arc::new(uploader);
    let semaphore = Arc::new(Semaphore::new(max_concurrency));

    let mut watcher = notify::recommended_watcher(move |res| {
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

        tokio::spawn(async move {
            let permit = semaphore_clone.acquire_owned().await.unwrap();
            if let Err(e) = process_path(uploader_clone, &path).await {
                error!("Failed to process '{}': {}", path.display(), e);
            }
            drop(permit);
        });
    }

    Ok(())
}

/// Processes a single store path for upload.
async fn process_path(uploader: Arc<S3Uploader>, path: &Path) -> Result<()> {
    let path_str = path.to_str().unwrap_or("").to_string();

    // 1. Get the closure of the path.
    let closure = nix::get_store_path_closure(&path_str)?;

    // 2. Check which paths are missing from the cache.
    let missing_paths = uploader.check_paths_exist(&closure).await?;

    if missing_paths.is_empty() {
        info!("All paths in the closure of '{}' are already cached.", path.display());
        return Ok(());
    }

    info!("Found {} missing paths to upload.", missing_paths.len());

    // 3. Upload each missing path.
    for p_str in missing_paths {
        let p = Path::new(&p_str);
        // Upload NAR, then .narinfo
        nix::upload_nar_for_path(uploader.clone(), p).await?;
    }

    Ok(())
}

