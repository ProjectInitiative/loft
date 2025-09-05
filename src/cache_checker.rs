use anyhow::Result;

use std::{collections::HashMap, path::Path, sync::Arc};
use tracing::{debug, info};

use crate::local_cache::LocalCache;
use crate::s3_uploader::S3Uploader;
use attic::nix_store::{NixStore, ValidPathInfo};

/// The result of checking paths against local + remote caches.
pub struct CacheCheckResult {
    /// Paths that must still be uploaded, with their `ValidPathInfo`.
    pub to_upload: Vec<(String, Arc<ValidPathInfo>)>,
    /// Hashes that were already cached locally (for bookkeeping).
    pub already_cached: Vec<String>,
}

pub struct CacheChecker {
    uploader: Arc<S3Uploader>,
    local_cache: Arc<LocalCache>,
    config: crate::config::Config,
}

impl CacheChecker {
    pub fn new(
        uploader: Arc<S3Uploader>,
        local_cache: Arc<LocalCache>,
        config: crate::config::Config,
    ) -> Self {
        Self {
            uploader,
            local_cache,
            config,
        }
    }

    /// Normalize paths → hashes + infos.
    async fn get_infos_and_hashes(
        &self,
        nix_store: &NixStore,
        paths: &[String],
    ) -> Result<(Vec<String>, HashMap<String, Arc<ValidPathInfo>>)> {
        let mut hashes = Vec::new();
        let mut infos = HashMap::new();

        for p in paths {
            let store_path = nix_store.parse_store_path(Path::new(p))?;
            let info = nix_store.query_path_info(store_path).await?;
            let plain_hash = info
                .nar_hash
                .to_typed_base32()
                .strip_prefix("sha256:")
                .unwrap_or_default()
                .to_string();

            hashes.push(plain_hash.clone());
            infos.insert(p.clone(), Arc::new(info));
        }

        Ok((hashes, infos))
    }

    /// Full check: local → remote → upload candidates.
    pub async fn check_paths(
        &self,
        nix_store: &NixStore,
        paths: Vec<String>,
        force_scan: bool,
    ) -> Result<CacheCheckResult> {
        if paths.is_empty() {
            return Ok(CacheCheckResult {
                to_upload: vec![],
                already_cached: vec![],
            });
        }

        let (hashes, infos) = self.get_infos_and_hashes(nix_store, &paths).await?;

        // 1. Local cache check
        let missing_paths: Vec<String> = if force_scan {
            paths.clone()
        } else {
            let existing = self.local_cache.find_existing_hashes(&hashes)?;
            info!("Local cache already has {} entries", existing.len());

            paths
                .into_iter()
                .filter(|p| {
                    let h = infos[p]
                        .nar_hash
                        .to_typed_base32()
                        .strip_prefix("sha256:")
                        .unwrap_or_default()
                        .to_string();
                    !existing.contains(&h)
                })
                .collect()
        };

        if missing_paths.is_empty() && !force_scan {
            debug!("All paths already in local cache");
            return Ok(CacheCheckResult {
                to_upload: vec![],
                already_cached: hashes,
            });
        }

        // 2. Remote cache check (pass concurrency from config)
        let (missing_remote, found_remote) = self
            .uploader
            .check_paths_exist(&missing_paths, self.config.loft.upload_threads)
            .await?;
        debug!("{} found on remote", found_remote.len());

        // Add found-on-remote → local cache
        if !found_remote.is_empty() {
            let mut to_add = Vec::new();
            for p in &found_remote {
                let h = infos[p]
                    .nar_hash
                    .to_typed_base32()
                    .strip_prefix("sha256:")
                    .unwrap_or_default()
                    .to_string();
                to_add.push(h);
            }
            self.local_cache.add_many_path_hashes(&to_add)?;
        }

        // 3. Prepare upload list
        let mut to_upload = Vec::new();
        for p in missing_remote {
            let info = infos.get(&p).unwrap().clone();
            to_upload.push((p, info));
        }

        Ok(CacheCheckResult {
            to_upload,
            already_cached: hashes,
        })
    }
}
