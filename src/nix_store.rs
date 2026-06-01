use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use futures::stream::Stream;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::warn;

use crate::hash::Hash;

pub const STORE_PATH_HASH_LEN: usize = 32;
const NIX_BASE32_CHARS: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

#[derive(Debug, Clone)]
pub struct StorePath {
    base_name: PathBuf,
}

#[derive(Debug, Clone)]
pub struct StorePathHash(String);

#[derive(Debug)]
pub struct ValidPathInfo {
    pub path: StorePath,
    pub nar_hash: Hash,
    pub nar_size: u64,
    pub references: Vec<PathBuf>,
    pub sigs: Vec<String>,
    pub ca: Option<String>,
}

pub struct NixStore {
    store_dir: PathBuf,
}

impl NixStore {
    pub fn connect() -> Result<Self> {
        let output = std::process::Command::new("nix")
            .arg("eval")
            .arg("--expr")
            .arg("builtins.storeDir")
            .arg("--raw")
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("Failed to get store dir: {}", stderr));
        }

        let store_dir = String::from_utf8(output.stdout)?.trim().to_string();
        if store_dir.is_empty() {
            return Err(anyhow::anyhow!("Empty store directory"));
        }

        Ok(Self {
            store_dir: PathBuf::from(store_dir),
        })
    }

    pub fn store_dir(&self) -> &Path {
        &self.store_dir
    }

    pub fn follow_store_path<P: AsRef<Path>>(&self, path: P) -> Result<StorePath> {
        let path = path.as_ref();
        if path.strip_prefix(&self.store_dir).is_ok() {
            self.parse_store_path(path)
        } else {
            let canon = std::fs::canonicalize(path)?;
            self.parse_store_path(canon)
        }
    }

    pub fn parse_store_path<P: AsRef<Path>>(&self, path: P) -> Result<StorePath> {
        let base_name = to_base_name(&self.store_dir, path.as_ref())?;
        StorePath::from_base_name(base_name)
    }

    pub fn get_full_path(&self, store_path: &StorePath) -> PathBuf {
        self.store_dir.join(&store_path.base_name)
    }

    // TODO: Replace CLI with nix C API when nix_api_store.h exposes
    // nix_store_query_path_info and nix_store_nar_from_path.
    // Tracked at: https://github.com/NixOS/nix/issues (C API expansion)
    pub async fn query_path_info(&self, store_path: StorePath) -> Result<ValidPathInfo> {
        let full_path = self.get_full_path(&store_path);
        let path_str = full_path.to_str().unwrap_or("");

        let output = Command::new("nix")
            .arg("path-info")
            .arg("--json")
            .arg(path_str)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("nix path-info failed: {}", stderr));
        }

        let stdout = String::from_utf8(output.stdout)?;
        let json: Value = serde_json::from_str(&stdout)?;

        let path_info = json
            .get(path_str)
            .or_else(|| json.as_object().and_then(|m| m.values().next()))
            .ok_or_else(|| anyhow::anyhow!("No path info in JSON output"))?;

        let nar_hash_str = path_info
            .get("narHash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing narHash"))?;

        let nar_hash = Hash::from_typed(nar_hash_str)?;

        let nar_size = path_info
            .get("narSize")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow::anyhow!("Missing narSize"))?;

        let references = path_info
            .get("references")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| PathBuf::from(s))
                    .collect()
            })
            .unwrap_or_default();

        let sigs = path_info
            .get("signatures")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();

        let ca = path_info
            .get("ca")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(ValidPathInfo {
            path: store_path,
            nar_hash,
            nar_size,
            references,
            sigs,
            ca,
        })
    }

    pub fn nar_from_path(&self, store_path: StorePath) -> impl Stream<Item = Result<Vec<u8>>> {
        let full_path = self.get_full_path(&store_path);
        let path_str = full_path.to_str().unwrap_or("").to_string();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<Vec<u8>>>();

        tokio::spawn(async move {
            let mut child = match Command::new("nix")
                .arg("nar")
                .arg("dump-path")
                .arg(&path_str)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx.send(Err(anyhow::anyhow!("Failed to spawn nix nar: {}", e)));
                    return;
                }
            };

            let mut stdout = match child.stdout.take() {
                Some(s) => s,
                None => {
                    let _ = tx.send(Err(anyhow::anyhow!("No stdout from nix nar")));
                    return;
                }
            };

            let mut buf = vec![0u8; 65536];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(anyhow::anyhow!("NAR read error: {}", e)));
                        return;
                    }
                }
            }

            let status = match child.wait().await {
                Ok(s) => s,
                Err(e) => {
                    let _ = tx.send(Err(anyhow::anyhow!("Failed to wait for nix nar: {}", e)));
                    return;
                }
            };

            if !status.success() {
                let stderr = match child.stderr.take() {
                    Some(mut s) => {
                        let mut buf = String::new();
                        s.read_to_string(&mut buf).await.unwrap_or(0);
                        buf
                    }
                    None => String::new(),
                };
                let _ = tx.send(Err(anyhow::anyhow!(
                    "nix nar dump-path failed ({}): {}",
                    status,
                    stderr
                )));
            }
        });

        tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
    }

    pub async fn compute_fs_closure_multi(
        &self,
        store_paths: Vec<StorePath>,
        flip_directions: bool,
        _include_outputs: bool,
        _include_derivers: bool,
    ) -> Result<Vec<StorePath>> {
        let mut paths = Vec::new();
        for sp in &store_paths {
            let full = self.get_full_path(sp);
            paths.push(full.to_str().unwrap_or("").to_string());
        }

        let mut cmd = Command::new("nix-store");
        cmd.arg("--query");
        if flip_directions {
            cmd.arg("--referrers");
        } else {
            cmd.arg("--requisites");
        }
        for p in &paths {
            cmd.arg(p);
        }

        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("nix-store --query failed: {}", stderr));
        }

        let output_str = String::from_utf8(output.stdout)?;
        let mut result = Vec::new();
        for line in output_str.lines() {
            if !line.is_empty() {
                match self.parse_store_path(line) {
                    Ok(sp) => result.push(sp),
                    Err(e) => warn!("Failed to parse closure path {}: {}", line, e),
                }
            }
        }

        Ok(result)
    }
}

impl StorePath {
    fn from_base_name(base_name: PathBuf) -> Result<Self> {
        let s = base_name.as_os_str().to_str().ok_or_else(|| {
            anyhow::anyhow!(
                "Store path name contains non-UTF-8 characters: {:?}",
                base_name
            )
        })?;

        if s.len() < 33 || !s.is_char_boundary(STORE_PATH_HASH_LEN) {
            return Err(anyhow::anyhow!("Store path too short: {}", s));
        }

        let hash = &s[..STORE_PATH_HASH_LEN];
        if !hash.bytes().all(|b| NIX_BASE32_CHARS.contains(&b)) {
            return Err(anyhow::anyhow!("Invalid store path hash: {}", s));
        }

        let name = &s[STORE_PATH_HASH_LEN + 1..];
        if name.is_empty() {
            return Err(anyhow::anyhow!("Store path missing name part: {}", s));
        }

        Ok(Self { base_name })
    }

    pub fn to_hash(&self) -> StorePathHash {
        let s = unsafe { std::str::from_utf8_unchecked(self.base_name.as_os_str().as_bytes()) };
        let hash = s[..STORE_PATH_HASH_LEN].to_string();
        StorePathHash(hash)
    }

    pub fn name(&self) -> String {
        let s = unsafe { std::str::from_utf8_unchecked(self.base_name.as_os_str().as_bytes()) };
        s[STORE_PATH_HASH_LEN + 1..].to_string()
    }

    pub fn as_os_str(&self) -> &OsStr {
        self.base_name.as_os_str()
    }

    pub fn as_base_name_bytes(&self) -> &[u8] {
        self.base_name.as_os_str().as_bytes()
    }
}

impl StorePathHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn to_string(&self) -> String {
        self.0.clone()
    }
}

fn to_base_name(store_dir: &Path, path: &Path) -> Result<PathBuf> {
    let remaining = path.strip_prefix(store_dir).map_err(|_| {
        anyhow::anyhow!(
            "Path '{}' is not in store directory '{}'",
            path.display(),
            store_dir.display()
        )
    })?;

    let first = remaining
        .iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Path is store directory itself"))?;

    if first.len() < STORE_PATH_HASH_LEN {
        return Err(anyhow::anyhow!("Path is too short: {:?}", first));
    }

    Ok(PathBuf::from(first))
}
