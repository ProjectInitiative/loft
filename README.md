# Loft

A lightweight, client-only Nix binary cache uploader for S3-compatible storage like Garage or MinIO.

## Features

*  Direct S3 Upload: Uploads NARs directly to your S3 bucket.
* Nix Store Watcher: Watches the /nix/store for new paths and automatically uploads them.
* Multi-threaded Uploads: Uploads multiple NARs in parallel to speed up the process.
* Closure Deduplication: Before uploading, it checks which paths in a closure already exist in the cache to avoid redundant work.

## Configuration

Create a loft.toml file with the following content:
```toml
[s3]
bucket = "your-nix-cache-bucket"
region = "us-east-1"
endpoint = "http://localhost:9000"
access_key = "your-access-key"
secret_key = "your-secret-key"

# Optional: Number of concurrent uploads
upload_threads = 8
```

## Usage

```bash
cargo run -- --config /path/to/your/loft.toml
```

