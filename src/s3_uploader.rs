//! Handles all interactions with S3-compatible storage.

use anyhow::Result;
use aws_config::defaults;
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use std::path::Path;
use tracing::{debug, info};

use crate::config::S3Config;
use attic::nix_store::NixStore;

/// A client for uploading files to an S3 bucket.
pub struct S3Uploader {
    client: Client,
    bucket: String,
    nix_store: NixStore,
}

impl S3Uploader {
    /// Creates a new S3 uploader from the given configuration.
    pub async fn new(config: &S3Config) -> Result<Self> {
        let access_key = config.access_key.clone().or_else(|| std::env::var("AWS_ACCESS_KEY_ID").ok());
        let secret_key = config.secret_key.clone().or_else(|| std::env::var("AWS_SECRET_ACCESS_KEY").ok());

        let (access_key, secret_key) = match (access_key, secret_key) {
            (Some(ak), Some(sk)) => (ak, sk),
            _ => return Err(anyhow::anyhow!("AWS access key and secret key must be provided either in config or as environment variables (AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY)")),
        };

        let config_loader = defaults(BehaviorVersion::latest())
            .endpoint_url(config.endpoint.as_str())
            .region(Region::new(config.region.clone()))
            .credentials_provider(Credentials::new(
                &access_key,
                &secret_key,
                None,
                None,
                "default",
            ));

        let sdk_config = config_loader.load().await;

        let s3_config = aws_sdk_s3::config::Builder::from(&sdk_config)
            .force_path_style(true)
            .build();

        let client = Client::from_conf(s3_config);

        let nix_store = NixStore::connect()?;

        Ok(S3Uploader {
            client,
            bucket: config.bucket.clone(),
            nix_store,
        })
    }

    /// Checks if a list of store paths already exist in the cache.
    ///
    /// This is a client-side adaptation of the server's "get-missing-paths" endpoint.
    pub async fn check_paths_exist(&self, store_paths: &[String], config: &crate::config::Config) -> Result<Vec<String>> {
        let mut missing_paths = Vec::new();

        let skip_signed_by_keys: Vec<String> = config.loft.skip_signed_by_keys.clone().unwrap_or_default();

        for path_str in store_paths {
            let store_path = match self.nix_store.parse_store_path(Path::new(path_str)) {
                Ok(sp) => sp,
                Err(e) => {
                    debug!("Failed to parse store path '{}': {}", path_str, e);
                    missing_paths.push(path_str.clone());
                    continue;
                }
            };

            let path_info = match self.nix_store.query_path_info(store_path.clone()).await {
                Ok(pi) => pi,
                Err(e) => {
                    debug!("Failed to query path info for '{}': {}", path_str, e);
                    missing_paths.push(path_str.clone());
                    continue;
                }
            };

            // Check if the path is signed by any of the keys to skip.
            let mut should_skip_due_to_signature = false;
            for sig in &path_info.sigs {
                if let Some((name, _)) = sig.split_once(':') {
                    if skip_signed_by_keys.iter().any(|u| name == u) {
                        info!("Path '{}' is signed by upstream key '{}'. Skipping upload.", path_str, name);
                        should_skip_due_to_signature = true;
                        break;
                    }
                }
            }

            if should_skip_due_to_signature {
                continue;
            }

            // Log signatures if any, for debugging/information.
            if !path_info.sigs.is_empty() {
                info!("Path '{}' has signatures: {:?}", path_str, path_info.sigs);
            }

            let key = format!("{}.narinfo", path_info.path.to_hash().to_string());

            match self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(&key)
                .send()
                .await
            {
                Ok(_) => {
                    info!("'{}' already exists in the cache. Skipping.", path_str);
                }
                Err(e) => {
                    info!("'{}' not found in cache. Will upload.", path_str);
                    debug!("Cache check failed for path '{}' with key '{}': {}", path_str, key, e);
                    missing_paths.push(path_str.clone());
                }
            }
        }
        Ok(missing_paths)
    }

    /// Uploads a file to the S3 bucket.
    pub async fn upload_file(&self, file_path: &Path, key: &str) -> Result<()> {
        let stream = ByteStream::from_path(file_path).await?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(stream)
            .send()
            .await?;
        info!("Successfully uploaded '{}' to '{}'.", file_path.display(), key);
        Ok(())
    }

    /// Uploads bytes to the S3 bucket.
    pub async fn upload_bytes(&self, bytes: Vec<u8>, key: &str) -> Result<()> {
        let stream = ByteStream::from(bytes);
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(stream)
            .send()
            .await?;
        info!("Successfully uploaded bytes to '{}'.", key);
        Ok(())
    }
}

