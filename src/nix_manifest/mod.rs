//! The Nix manifest format.
//!
//! Nix uses a simple format in binary cache manifests (`.narinfo`,
//! `/nix-cache-info`). It consists of a single, flat KV map with
//! colon (`:`) as the delimiter.
//!
//! It's not well-defined and the official implementation performs
//! serialization and deserialization by hand [1]. Here we implement
//! a deserializer and a serializer using the serde framework.
//!
//! An example of a `/nix-cache-info` file:
//!
//! ```text
//! StoreDir: /nix/store
//! WantMassQuery: 1
//! Priority: 40
//! ```
//!
//! [1] <https://github.com/NixOS/nix/blob/d581129ef9ef5d7d65e676f6a7bfe36c82f6ea6e/src/libstore/nar-info.cc#L28>



// #[cfg(test)]
// mod tests;

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::os::unix::ffi::OsStrExt;

use displaydoc::Display;
use serde::{de, ser, Deserialize, Serialize};
use serde_with::{formats::SpaceSeparator, serde_as, StringWithSeparator};

mod deserializer;
mod serializer;

use anyhow::Result;
use self::deserializer::Deserializer;
use self::serializer::Serializer;

use attic::hash::Hash;
use attic::signing::NixKeypair;
use itoa;

pub fn from_str<T>(s: &str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let mut deserializer = Deserializer::from_str(s);
    T::deserialize(&mut deserializer).map_err(|e| anyhow::anyhow!("Manifest deserialization error: {}", e))

    // FIXME: Reject extra output??
}

pub fn to_string<T>(value: &T) -> Result<String>
where
    T: Serialize,
{
    let mut serializer = Serializer::new();
    value
        .serialize(&mut serializer)
        .map_err(|e| anyhow::anyhow!("Manifest serialization error: {}", e))?;

    Ok(serializer.into_output())
}

/// An error during (de)serialization.
#[derive(Debug, Display)]
pub enum Error {
    /// Unexpected {0}.
    Unexpected(&'static str),

    /// Unexpected EOF.
    UnexpectedEof,

    /// Expected a colon.
    ExpectedColon,

    /// Expected a boolean.
    ExpectedBoolean,

    /// Expected an integer.
    ExpectedInteger,

    /// "{0}" values are unsupported.
    Unsupported(&'static str),

    /// Not possible to auto-determine the type.
    AnyUnsupported,

    /// None is unsupported. Add #[serde(skip_serializing_if = "Option::is_none")]
    NoneUnsupported,

    /// Nested maps are unsupported.
    NestedMapUnsupported,

    /// Floating point numbers are unsupported.
    FloatUnsupported,

    /// Custom error: {0}
    Custom(String),
}

/// Custom (de)serializer for a space-delimited list.
pub type SpaceDelimitedList = StringWithSeparator<SpaceSeparator, String>;

impl std::error::Error for Error {}

impl de::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        let f = format!("{}", msg);
        Self::Custom(f)
    }
}

impl ser::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        let f = format!("{}", msg);
        Self::Custom(f)
    }
}

/// NAR information.
#[serde_as]
#[derive(Debug, Serialize, Deserialize)]
pub struct NarInfo {
    /// The full store path being cached, including the store directory.
    ///
    /// Part of the fingerprint.
    ///
    /// Example: `/nix/store/p4pclmv1gyja5kzc26npqpia1qqxrf0l-ruby-2.7.3`.
    #[serde(rename = "StorePath")]
    pub store_path: PathBuf,

    /// The URL to fetch the object.
    ///
    /// This can either be relative to the base cache URL (`cacheUri`),
    /// or be an full, absolute URL.
    ///
    /// Example: `nar/1w1fff338fvdw53sqgamddn1b2xgds473pv6y13gizdbqjv4i5p3.nar.xz`
    /// Example: `https://cache.nixos.org/nar/1w1fff338fvdw53sqgamddn1b2xgds473pv6y13gizdbqjv4i5p3.nar.xz`
    ///
    /// Nix implementation: <https://github.com/NixOS/nix/blob/af553b20902b8b8efbccab5f880879b09e95eb32/src/libstore/http-binary-cache-store.cc#L138-L145>
    #[serde(rename = "URL")]
    pub url: String,

    /// Compression in use.
    #[serde(rename = "Compression")]
    pub compression: Compression,

    /// The hash of the compressed file.
    ///
    /// We don't know the file hash if it's chunked.
    #[serde(rename = "FileHash")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_hash: Option<Hash>,

    /// The size of the compressed file.
    ///
    /// We may not know the file size if it's chunked.
    #[serde(rename = "FileSize")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size: Option<usize>,

    /// The hash of the NAR archive.
    ///
    /// Part of the fingerprint.
    #[serde(rename = "NarHash")]
    pub nar_hash: Hash,

    /// The size of the NAR archive.
    ///
    /// Part of the fingerprint.
    #[serde(rename = "NarSize")]
    pub nar_size: usize,

    /// Other store paths this object directly refereces.
    ///
    /// This only includes the base paths, not the store directory itself.
    ///
    /// Part of the fingerprint.
    ///
    /// Example element: `j5p0j1w27aqdzncpw73k95byvhh5prw2-glibc-2.33-47`
    #[serde(rename = "References")]
    #[serde_as(as = "SpaceDelimitedList")]
    pub references: Vec<String>,

    /// The system this derivation is built for.
    #[serde(rename = "System")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,

    /// The derivation that produced this object.
    #[serde(rename = "Deriver")]
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_deriver")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deriver: Option<String>,

    /// The signature of the object.
    ///
    /// The `Sig` field can be duplicated to include multiple
    /// signatures, but we only support one for now.
    #[serde(rename = "Sig")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,

    /// The content address of the object.
    #[serde(rename = "CA")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ca: Option<String>,
}

/// NAR compression type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Compression {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "xz")]
    Xz,
    #[serde(rename = "bzip2")]
    Bzip2,
    #[serde(rename = "br")]
    Brotli,
    #[serde(rename = "zstd")]
    Zstd,
}

impl NarInfo {
    /// Parses a narinfo from a string.
    pub fn from_str(manifest: &str) -> Result<Self> {
        from_str(manifest)
    }

    /// Returns the serialized representation of the narinfo.
    pub fn to_string(&self) -> Result<String> {
        to_string(self)
    }

    /// Returns the signature of this object, if it exists.
    pub fn signature(&self) -> Option<&String> {
        self.signature.as_ref()
    }

    /// Returns the store directory of this object.
    pub fn store_dir(&self) -> &Path {
        // FIXME: Validate store_path
        self.store_path.parent().unwrap()
    }

    /// Signs the narinfo and adds the signature to the narinfo.
    pub fn sign(&mut self, keypair: &NixKeypair) {
        let signature = self.sign_readonly(keypair);
        self.signature = Some(signature);
    }

    /// Returns the fingerprint of the object.
    pub fn fingerprint(&self) -> Vec<u8> {
        let store_dir = self.store_dir();
        let mut fingerprint = b"1;".to_vec();

        // 1;{storePath};{narHash};{narSize};{commaDelimitedReferences}

        // storePath
        fingerprint.extend(self.store_path.as_os_str().as_bytes());
        fingerprint.extend(b";");

        // narHash
        fingerprint.extend(self.nar_hash.to_typed_base32().as_bytes());
        fingerprint.extend(b";");

        // narSize
        let mut buf = itoa::Buffer::new();
        let nar_size = buf.format(self.nar_size);
        fingerprint.extend(nar_size.as_bytes());
        fingerprint.extend(b";");

        // commaDelimitedReferences
        let mut iter = self.references.iter().peekable();
        while let Some(reference) = iter.next() {
            fingerprint.extend(store_dir.as_os_str().as_bytes());
            fingerprint.extend(b"/");
            fingerprint.extend(reference.as_bytes());

            if iter.peek().is_some() {
                fingerprint.extend(b",");
            }
        }

        fingerprint
    }

    /// Signs the narinfo with a keypair, returning the signature.
    fn sign_readonly(&self, keypair: &NixKeypair) -> String {
        let fingerprint = self.fingerprint();
        keypair.sign(&fingerprint)
    }
}

pub fn deserialize_deriver<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: de::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    match s.as_str() {
        "unknown-deriver" => Ok(None),
        _ => Ok(Some(s)),
    }
}
