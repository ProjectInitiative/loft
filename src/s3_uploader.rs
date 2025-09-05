use anyhow::Result;
use aws_config::defaults;
use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use futures::StreamExt;
use http::StatusCode;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::config::S3Config;
use attic::nix_store::NixStore;
use attic::nix_store::StorePath;

/// A client for uploading files to an S3 bucket.
pub struct S3Uploader {
    client: Client,
    bucket: String,
    nix_store: NixStore,
}

impl S3Uploader {
    /// Creates a new S3 uploader from the given configuration.
    pub async fn new(config: &S3Config) -> Result<Self> {
        let access_key = config
            .access_key
            .clone()
            .or_else(|| std::env::var("AWS_ACCESS_KEY_ID").ok());
        let secret_key = config
            .secret_key
            .clone()
            .or_else(|| std::env::var("AWS_SECRET_ACCESS_KEY").ok());

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

        let s3_config = aws_sdk_s3::config::Builder::from(&sdk_config).build();

        let client = Client::from_conf(s3_config);

        let nix_store = NixStore::connect()?;

        Ok(S3Uploader {
            client,
            bucket: config.bucket.clone(),
            nix_store,
        })
    }

    /// Checks if a list of store paths already exist in the cache.
    pub async fn check_paths_exist(
        &self,
        store_paths: &[String],
        max_concurrency: usize,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let semaphore = Arc::new(Semaphore::new(max_concurrency));

        let mut missing_paths = Vec::new();
        let mut found_paths = Vec::new();

        let results = futures::stream::iter(store_paths.iter().cloned().map(|path_str| {
            let client = self.client.clone();
            let bucket = self.bucket.clone();
            let nix_store = NixStore::connect().unwrap();
            let semaphore = semaphore.clone();

            async move {
                let _permit = semaphore.acquire().await.expect("semaphore closed");

                let store_path = match nix_store.parse_store_path(Path::new(&path_str)) {
                    Ok(sp) => sp,
                    Err(e) => {
                        debug!("Failed to parse store path '{}': {}", path_str, e);
                        return Err((path_str, e.to_string()));
                    }
                };

                let path_info = match nix_store.query_path_info(store_path.clone()).await {
                    Ok(pi) => pi,
                    Err(e) => {
                        debug!("Failed to query path info for '{}': {}", path_str, e);
                        return Err((path_str, e.to_string()));
                    }
                };

                if !path_info.sigs.is_empty() {
                    info!("Path '{}' has signatures: {:?}", path_str, path_info.sigs);
                }

                let key = format!(
                    "{}.narinfo",
                    path_info
                        .nar_hash
                        .to_typed_base32()
                        .strip_prefix("sha256:")
                        .unwrap_or_default()
                );
                debug!("Checking S3 for key: {}", key);

                match client.head_object().bucket(&bucket).key(&key).send().await {
                    Ok(_) => {
                        info!("'{}' already exists in the cache. Skipping.", path_str);
                        Ok::<_, (String, String)>((path_str, true))
                    }
                    Err(e) => {
                        debug!("HeadObject for key '{}' failed: {:?}", key, e);

                        let is_not_found =
                            if let aws_sdk_s3::error::SdkError::ServiceError(service_error) = &e {
                                service_error.raw().status() == StatusCode::NOT_FOUND.into()
                            } else {
                                false
                            };

                        if is_not_found {
                            info!("'{}' not found in cache. Will upload.", path_str);
                        } else {
                            warn!(
                            "Unexpected error during cache check for path '{}' with key '{}': {:?}",
                            path_str, key, e
                        );
                        }

                        Ok::<_, (String, String)>((path_str, false))
                    }
                }
            }
        }))
        .buffer_unordered(max_concurrency)
        .collect::<Vec<_>>()
        .await;

        for res in results {
            match res {
                Ok((path, true)) => found_paths.push(path),
                Ok((path, false)) | Err((path, _)) => missing_paths.push(path),
            }
        }

        Ok((missing_paths, found_paths))
    }

    /// Lists all .narinfo keys in the S3 bucket.
    pub async fn list_all_narinfo_keys(&self) -> Result<Vec<String>> {
        let mut all_keys = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut request = self.client.list_objects_v2().bucket(&self.bucket);

            if let Some(token) = continuation_token {
                request = request.continuation_token(token);
            }

            let output = request.send().await?;

            if let Some(contents) = output.contents {
                for object in contents {
                    if let Some(key) = object.key {
                        if key.ends_with(".narinfo") {
                            let processed_key = if key.starts_with("sha256:") {
                                key[7..].to_string()
                            } else {
                                key
                            };
                            all_keys.push(processed_key);
                        }
                    }
                }
            }

            continuation_token = output.next_continuation_token;

            if continuation_token.is_none() {
                break;
            }
        }

        Ok(all_keys)
    }

    /// Uploads a Nix store path to S3 as both NAR and narinfo files.
    /// This handles both directories and individual files correctly.
    pub async fn upload_store_path(&self, store_path: &StorePath) -> Result<()> {
        info!("Uploading store path: {:?}", store_path);

        // 1. Query path info to get metadata
        let path_info = self.nix_store.query_path_info(store_path.clone()).await?;

        // 2. Generate NAR from the store path (works for both files and directories)
        let nar_stream = self.nix_store.nar_from_path(store_path.clone());

        // Collect the NAR stream into bytes
        // Note: You might want to stream this directly to S3 for large NARs
        let mut nar_bytes = Vec::new();
        let mut stream = nar_stream;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            nar_bytes.extend_from_slice(&chunk);
        }

        // 3. Upload NAR file
        let nar_hash_b32_full = path_info.nar_hash.to_typed_base32();
        let nar_hash_b32 = nar_hash_b32_full
            .strip_prefix("sha256:")
            .unwrap_or_default();

        let nar_key = format!("nar/{}.nar.xz", nar_hash_b32);

        // For production, you'd want to compress the NAR with xz here
        // For now, let's upload uncompressed (you can add compression later)
        self.upload_bytes(nar_bytes, &nar_key).await?;

        // 4. Generate and upload .narinfo file
        let narinfo_content = self.generate_narinfo_content(&path_info, &nar_key).await?;
        let narinfo_key = format!("{}.narinfo", nar_hash_b32);

        self.upload_bytes(narinfo_content.into_bytes(), &narinfo_key)
            .await?;

        info!("Successfully uploaded store path: {:?}", store_path);
        Ok(())
    }

    /// Generates the content for a .narinfo file
    async fn generate_narinfo_content(
        &self,
        path_info: &attic::nix_store::ValidPathInfo,
        nar_key: &str,
    ) -> Result<String> {
        // Basic narinfo format - you may need to adjust based on your attic PathInfo structure
        let content = format!(
            "StorePath: {:?}\n\
             URL: {}\n\
             Compression: none\n\
             FileHash: {}\n\
             FileSize: {}\n\
             NarHash: {}\n\
             NarSize: {}\n\
             References: {}\n",
            path_info.path,
            nar_key,
            path_info.nar_hash.to_typed_base32(), // Assuming this is file hash for uncompressed
            path_info.nar_size,                   // File size
            path_info.nar_hash.to_typed_base32(),
            path_info.nar_size,
            path_info
                .references
                .iter()
                .map(|r| r.to_string_lossy().to_string())
                .collect::<Vec<_>>()
                .join(" ")
        );

        // Add signatures if present
        let mut final_content = content;
        for sig in &path_info.sigs {
            final_content.push_str(&format!("Sig: {}\n", sig));
        }

        Ok(final_content)
    }

    /// Legacy method for uploading regular files (not store paths)
    pub async fn upload_file(&self, file_path: &Path, key: &str) -> Result<()> {
        let stream = ByteStream::from_path(file_path).await?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(stream)
            .send()
            .await?;
        info!(
            "Successfully uploaded '{}' to '{}'.",
            file_path.display(),
            key
        );
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

    /// Uploads the nix-cache-info file to the root of the bucket.
    pub async fn upload_nix_cache_info(&self) -> Result<()> {
        let content =
            format!("StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 30\nCompression: xz\n");
        let key = "nix-cache-info";
        self.upload_bytes(content.into_bytes(), key).await?;
        info!("Successfully uploaded 'nix-cache-info' to '{}'.", key);
        Ok(())
    }
}
