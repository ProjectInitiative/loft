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

use aws_sdk_s3::types::CompletedPart;
use chrono::{DateTime, Utc};
use tokio::fs::File;
use tokio::io::AsyncReadExt;

use crate::config::S3Config;
use attic::nix_store::NixStore;


const MIN_MULTIPART_UPLOAD_SIZE: u64 = 8 * 1024 * 1024; // 8 MB

/// A client for uploading files to an S3 bucket.
pub struct S3Uploader {
    client: Client,
    bucket: String,
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

        Ok(S3Uploader {
            client,
            bucket: config.bucket.clone(),
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

                let key = crate::nix_utils::get_narinfo_key(&store_path);
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

    

    /// Uploads a file to S3, using multipart upload for large files.
    pub async fn upload_file(&self, file_path: &Path, key: &str) -> Result<()> {
        let file_size = tokio::fs::metadata(file_path).await?.len();

        if file_size < MIN_MULTIPART_UPLOAD_SIZE {
            // Use single PUT for small files
            let stream = ByteStream::from_path(file_path).await?;
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(stream)
                .send()
                .await?;
            info!(
                "Successfully uploaded '{}' to '{}' using single PUT.",
                file_path.display(),
                key
            );
        } else {
            // Use multipart upload for large files
            info!(
                "Initiating multipart upload for '{}' ({} bytes).",
                file_path.display(),
                file_size
            );

            let multipart_upload_res = self
                .client
                .create_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await?;

            let upload_id = multipart_upload_res
                .upload_id
                .ok_or_else(|| anyhow::anyhow!("Failed to get upload ID"))?;

            let mut parts: Vec<CompletedPart> = Vec::new();
            let mut file = File::open(file_path).await?;
            let mut part_number = 1;
            let mut bytes_read = 0;

            while bytes_read < file_size {
                let mut buffer = vec![0; MIN_MULTIPART_UPLOAD_SIZE as usize];
                let read_len = file.read(&mut buffer).await?;

                if read_len == 0 {
                    break; // End of file
                }

                let chunk = &buffer[..read_len];
                let stream = ByteStream::from(chunk.to_vec());

                let upload_part_res = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .part_number(part_number)
                    .body(stream)
                    .send()
                    .await?;

                parts.push(
                    CompletedPart::builder()
                        .part_number(part_number)
                        .e_tag(upload_part_res.e_tag.unwrap_or_default())
                        .build(),
                );

                bytes_read += read_len as u64;
                part_number += 1;
            }

            self.client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(
                    aws_sdk_s3::types::CompletedMultipartUpload::builder()
                        .set_parts(Some(parts))
                        .build(),
                )
                .send()
                .await?;

            info!(
                "Successfully uploaded '{}' to '{}' using multipart upload.",
                file_path.display(),
                key
            );
        }
        Ok(())
    }

    /// Uploads bytes to the S3 bucket, using multipart upload for large byte arrays.
    pub async fn upload_bytes(&self, bytes: Vec<u8>, key: &str) -> Result<()> {
        let bytes_len = bytes.len() as u64;

        if bytes_len < MIN_MULTIPART_UPLOAD_SIZE {
            // Use single PUT for small byte arrays
            let stream = ByteStream::from(bytes);
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(key)
                .body(stream)
                .send()
                .await?;
            info!("Successfully uploaded bytes to '{}' using single PUT.", key);
        } else {
            // Use multipart upload for large byte arrays
            info!(
                "Initiating multipart upload for bytes ({} bytes).",
                bytes_len
            );

            let multipart_upload_res = self
                .client
                .create_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .send()
                .await?;

            let upload_id = multipart_upload_res
                .upload_id
                .ok_or_else(|| anyhow::anyhow!("Failed to get upload ID"))?;

            let mut parts: Vec<CompletedPart> = Vec::new();
            let mut current_pos = 0;
            let mut part_number = 1;

            while current_pos < bytes_len {
                let end_pos = (current_pos + MIN_MULTIPART_UPLOAD_SIZE).min(bytes_len);
                let chunk = &bytes[current_pos as usize..end_pos as usize];
                let stream = ByteStream::from(chunk.to_vec());

                let upload_part_res = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .part_number(part_number)
                    .body(stream)
                    .send()
                    .await?;

                parts.push(
                    CompletedPart::builder()
                        .part_number(part_number)
                        .e_tag(upload_part_res.e_tag.unwrap_or_default())
                        .build(),
                );

                current_pos = end_pos;
                part_number += 1;
            }

            self.client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(
                    aws_sdk_s3::types::CompletedMultipartUpload::builder()
                        .set_parts(Some(parts))
                        .build(),
                )
                .send()
                .await?;

            info!(
                "Successfully uploaded bytes to '{}' using multipart upload.",
                key
            );
        }
        Ok(())
    }

    /// Uploads the nix-cache-info file to the root of the bucket.
    pub async fn upload_nix_cache_info(&self, config: &crate::config::Config) -> Result<()> {
        let compression_str = match config.loft.compression {
            crate::config::Compression::Xz => "xz",
            crate::config::Compression::Zstd => "zstd",
        };
        let content = format!(
            "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 30\nCompression: {}\n",
            compression_str
        );
        let key = "nix-cache-info";
        self.upload_bytes(content.into_bytes(), key).await?;
        info!("Successfully uploaded 'nix-cache-info' to '{}'.", key);
        Ok(())
    }

    /// Lists objects in the S3 bucket.
    pub async fn list_objects(
        &self,
        continuation_token: Option<String>,
    ) -> Result<aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output> {
        let mut request = self.client.list_objects_v2().bucket(&self.bucket);

        if let Some(token) = continuation_token {
            request = request.continuation_token(token);
        }

        let output = request.send().await?;
        Ok(output)
    }

    /// Deletes an object from the S3 bucket.
    pub async fn delete_object(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await?;
        debug!("Successfully deleted object: {}", key);
        Ok(())
    }

    /// Gets the total size of all objects in the S3 bucket in bytes.
    /// This can be an expensive operation for large buckets.
    pub async fn get_bucket_size(&self) -> Result<u64> {
        info!("Calculating current bucket size...");
        let mut total_size: u64 = 0;
        let mut continuation_token: Option<String> = None;

        loop {
            let output = self.list_objects(continuation_token.clone()).await?;

            if let Some(contents) = output.contents {
                for object in contents {
                    if let Some(size) = object.size {
                        total_size += size as u64;
                    }
                }
            }

            continuation_token = output.next_continuation_token;

            if continuation_token.is_none() {
                break;
            }
        }
        info!("Current bucket size: {} bytes", total_size);
        Ok(total_size)
    }
}

// Helper trait to convert aws_sdk_s3::types::DateTime to chrono::DateTime<Utc>
pub trait AwsDateTimeExt {
    fn to_chrono_utc(&self) -> DateTime<Utc>;
}

impl AwsDateTimeExt for aws_sdk_s3::primitives::DateTime {
    fn to_chrono_utc(&self) -> DateTime<Utc> {
        let secs = self.secs();
        let nanos = self.as_nanos();

        DateTime::<Utc>::from_timestamp(secs, nanos as u32).unwrap_or_else(|| {
            warn!("Invalid timestamp from S3, falling back to Utc::now()");
            Utc::now()
        })
    }
}
