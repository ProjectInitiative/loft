use anyhow::Result;
use futures::future::BoxFuture;
use std::{collections::{HashMap, HashSet}, path::Path, sync::Arc};
use tracing::{debug, info};

use attic::nix_store::NixStore;

/// Trait for local cache storage operations.
pub trait LocalCacheStorage: Send + Sync {
    fn find_existing_hashes(&self, hashes: &[String]) -> Result<HashSet<String>>;
    fn add_many_path_hashes(&self, hashes: &[String]) -> Result<()>;
}

/// Trait for remote cache storage operations.
pub trait RemoteCacheStorage: Send + Sync {
    fn check_paths_exist<'a>(
        &'a self,
        store_paths: &'a [String],
        max_concurrency: usize,
    ) -> BoxFuture<'a, Result<(Vec<String>, Vec<String>)>>;
}

/// Trait for providing Nix path hashes.
pub trait NixHashProvider: Send + Sync {
    fn get_hashes<'a>(
        &'a self,
        paths: &'a [String],
    ) -> BoxFuture<'a, Result<HashMap<String, String>>>;
}

impl NixHashProvider for NixStore {
    fn get_hashes<'a>(
        &'a self,
        paths: &'a [String],
    ) -> BoxFuture<'a, Result<HashMap<String, String>>> {
        Box::pin(async move {
            let mut map = HashMap::new();
            for p in paths {
                let store_path = self.parse_store_path(Path::new(p))?;
                let info = self.query_path_info(store_path).await?;
                let hash = info
                    .nar_hash
                    .to_typed_base32()
                    .strip_prefix("sha256:")
                    .unwrap_or_default()
                    .to_string();
                map.insert(p.clone(), hash);
            }
            Ok(map)
        })
    }
}

/// The result of checking paths against local + remote caches.
pub struct CacheCheckResult {
    /// Paths that must still be uploaded.
    pub to_upload: Vec<String>,
    /// Hashes that were already cached locally (for bookkeeping).
    pub already_cached: Vec<String>,
}

pub struct CacheChecker {
    uploader: Arc<dyn RemoteCacheStorage>,
    local_cache: Arc<dyn LocalCacheStorage>,
    config: crate::config::Config,
}

impl CacheChecker {
    pub fn new(
        uploader: Arc<dyn RemoteCacheStorage>,
        local_cache: Arc<dyn LocalCacheStorage>,
        config: crate::config::Config,
    ) -> Self {
        Self {
            uploader,
            local_cache,
            config,
        }
    }

    /// Full check: local → remote → upload candidates.
    pub async fn check_paths(
        &self,
        nix_provider: &dyn NixHashProvider,
        paths: Vec<String>,
        force_scan: bool,
    ) -> Result<CacheCheckResult> {
        if paths.is_empty() {
            return Ok(CacheCheckResult {
                to_upload: vec![],
                already_cached: vec![],
            });
        }

        let hashes_map = nix_provider.get_hashes(&paths).await?;
        let hashes: Vec<String> = paths
            .iter()
            .map(|p| hashes_map.get(p).cloned().unwrap_or_default())
            .collect();

        // 1. Local cache check
        let missing_paths: Vec<String> = if force_scan {
            paths.clone()
        } else {
            let existing = self.local_cache.find_existing_hashes(&hashes)?;
            info!("Local cache already has {} entries", existing.len());

            paths
                .into_iter()
                .filter(|p| {
                    let h = hashes_map.get(p).unwrap();
                    !existing.contains(h)
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
                if let Some(h) = hashes_map.get(p) {
                    to_add.push(h.clone());
                }
            }
            self.local_cache.add_many_path_hashes(&to_add)?;
        }

        // 3. Prepare upload list
        // missing_remote contains paths that are not in remote cache.
        // We just return them.
        Ok(CacheCheckResult {
            to_upload: missing_remote,
            already_cached: hashes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MockLocalCache {
        existing_hashes: Mutex<HashSet<String>>,
    }

    impl MockLocalCache {
        fn new(hashes: Vec<String>) -> Self {
            Self {
                existing_hashes: Mutex::new(hashes.into_iter().collect()),
            }
        }
    }

    impl LocalCacheStorage for MockLocalCache {
        fn find_existing_hashes(&self, hashes: &[String]) -> Result<HashSet<String>> {
            let store = self.existing_hashes.lock().unwrap();
            let mut result = HashSet::new();
            for h in hashes {
                if store.contains(h) {
                    result.insert(h.clone());
                }
            }
            Ok(result)
        }

        fn add_many_path_hashes(&self, hashes: &[String]) -> Result<()> {
            let mut store = self.existing_hashes.lock().unwrap();
            for h in hashes {
                store.insert(h.clone());
            }
            Ok(())
        }
    }

    struct MockRemoteCache {
        existing_paths: Mutex<HashSet<String>>,
    }

    impl MockRemoteCache {
        fn new(paths: Vec<String>) -> Self {
            Self {
                existing_paths: Mutex::new(paths.into_iter().collect()),
            }
        }
    }

    impl RemoteCacheStorage for MockRemoteCache {
        fn check_paths_exist<'a>(
            &'a self,
            store_paths: &'a [String],
            _max_concurrency: usize,
        ) -> BoxFuture<'a, Result<(Vec<String>, Vec<String>)>> {
            Box::pin(async move {
                let store = self.existing_paths.lock().unwrap();
                let mut missing = Vec::new();
                let mut found = Vec::new();
                for p in store_paths {
                    if store.contains(p) {
                        found.push(p.clone());
                    } else {
                        missing.push(p.clone());
                    }
                }
                Ok((missing, found))
            })
        }
    }

    struct MockNixHashProvider {
        hashes: HashMap<String, String>,
    }

    impl MockNixHashProvider {
        fn new(hashes: HashMap<String, String>) -> Self {
            Self { hashes }
        }
    }

    impl NixHashProvider for MockNixHashProvider {
        fn get_hashes<'a>(
            &'a self,
            paths: &'a [String],
        ) -> BoxFuture<'a, Result<HashMap<String, String>>> {
            Box::pin(async move {
                let mut map = HashMap::new();
                for p in paths {
                    if let Some(h) = self.hashes.get(p) {
                        map.insert(p.clone(), h.clone());
                    }
                }
                Ok(map)
            })
        }
    }

    #[tokio::test]
    async fn test_check_paths_logic() -> Result<()> {
        let path1 = "/nix/store/path1";
        let hash1 = "hash1";
        let path2 = "/nix/store/path2";
        let hash2 = "hash2"; // cached locally
        let path3 = "/nix/store/path3";
        let hash3 = "hash3"; // cached remotely

        let mut hashes = HashMap::new();
        hashes.insert(path1.to_string(), hash1.to_string());
        hashes.insert(path2.to_string(), hash2.to_string());
        hashes.insert(path3.to_string(), hash3.to_string());

        let local_cache = Arc::new(MockLocalCache::new(vec![hash2.to_string()]));
        let remote_cache = Arc::new(MockRemoteCache::new(vec![path3.to_string()]));
        let nix_provider = MockNixHashProvider::new(hashes);
        // We use a blank config here since we don't need real configuration
        // for this test, just some default values to satisfy CacheChecker::new
        let config = crate::config::Config {
            s3: crate::config::S3Config {
                endpoint: "".to_string(),
                region: "".to_string(),
                bucket: "".to_string(),
                access_key: None,
                secret_key: None,
            },
            loft: crate::config::LoftConfig {
                local_cache_path: std::path::PathBuf::from(""),
                signing_key_path: None,
                signing_key_name: None,
                upload_threads: 1,
                skip_signed_by_keys: None,
                large_nar_threshold_mb: 0,
                use_disk_for_large_nars: false,
                compression: crate::config::Compression::Zstd,
                prune_enabled: false,
                prune_schedule: None,
                prune_retention_days: None,
                prune_max_size_gb: None,
                prune_target_percentage: 90,
                scan_on_startup: false,
                populate_cache_on_startup: false,
            },
        };

        let checker = CacheChecker::new(remote_cache, local_cache.clone(), config);

        let paths = vec![path1.to_string(), path2.to_string(), path3.to_string()];

        let result = checker.check_paths(&nix_provider, paths, false).await?;

        // path2 is locally cached -> ignored
        // path3 is remotely cached -> should be added to local cache, not uploaded
        // path1 is nowhere -> should be uploaded

        assert!(result.to_upload.contains(&path1.to_string()));
        assert!(!result.to_upload.contains(&path2.to_string()));
        assert!(!result.to_upload.contains(&path3.to_string()));
        assert_eq!(result.to_upload.len(), 1);

        // Verify local cache updated for path3
        let local_hashes = local_cache.find_existing_hashes(&vec![hash3.to_string()])?;
        assert!(local_hashes.contains(hash3));

        Ok(())
    }
}
