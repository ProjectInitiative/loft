use displaydoc::Display;
use serde::{de, ser, Deserialize, Serialize};
use sha2::{Digest, Sha256};

use anyhow::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hash {
    Sha256([u8; 32]),
}

#[derive(Debug, Display)]
pub enum Error {
    /// The string lacks a colon separator.
    NoColonSeparator,
    /// Hash algorithm {0} is not supported.
    UnsupportedHashAlgorithm(String),
    /// Invalid base16 hash: {0}.
    InvalidBase16Hash(hex::FromHexError),
    /// Invalid base32 hash.
    InvalidBase32Hash,
    /// Invalid length for {typ} string: Must be either {base16_len} (hexadecimal) or {base32_len} (base32), got {actual}.
    InvalidHashStringLength {
        typ: &'static str,
        base16_len: usize,
        base32_len: usize,
        actual: usize,
    },
}

impl Hash {
    pub fn sha256_from_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self::Sha256(hasher.finalize().into())
    }

    pub fn from_typed(s: &str) -> Result<Self> {
        // Handle both "sha256:<base32>" (typed narinfo) and
        // "sha256-<base64>" (nix path-info --json output) formats.
        let (typ, hash) = if let Some(pos) = s.find(':') {
            let (t, h) = s.split_at(pos);
            (t, &h[1..])
        } else if let Some(pos) = s.find('-') {
            let (t, h) = s.split_at(pos);
            (t, &h[1..])
        } else {
            return Err(Error::NoColonSeparator.into());
        };
        match typ {
            "sha256" => {
                let v = decode_hash(hash, "SHA-256", 32)?;
                Ok(Self::Sha256(v.try_into().unwrap()))
            }
            _ => Err(Error::UnsupportedHashAlgorithm(typ.to_owned()).into()),
        }
    }

    pub fn to_typed_base32(&self) -> String {
        format!("{}:{}", self.hash_type(), self.to_base32())
    }

    pub fn to_typed_base16(&self) -> String {
        format!("{}:{}", self.hash_type(), hex::encode(self.data()))
    }

    fn data(&self) -> &[u8] {
        match self {
            Self::Sha256(d) => d,
        }
    }

    fn hash_type(&self) -> &'static str {
        match self {
            Self::Sha256(_) => "sha256",
        }
    }

    fn to_base32(&self) -> String {
        nix_base32::to_nix_base32(self.data())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        use de::Error;
        String::deserialize(deserializer)
            .and_then(|s| Self::from_typed(&s).map_err(|e| Error::custom(e.to_string())))
    }
}

impl std::error::Error for Error {}

impl Serialize for Hash {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        serializer.serialize_str(&self.to_typed_base16())
    }
}

fn decode_hash(s: &str, typ: &'static str, expected_bytes: usize) -> Result<Vec<u8>> {
    let base16_len = expected_bytes * 2;
    let base32_len = (expected_bytes * 8 - 1) / 5 + 1;
    let base64_len = (expected_bytes + 2) / 3 * 4;
    let v = if s.len() == base16_len {
        hex::decode(s).map_err(Error::InvalidBase16Hash)?
    } else if s.len() == base32_len {
        nix_base32::from_nix_base32(s).ok_or(Error::InvalidBase32Hash)?
    } else if s.len() == base64_len || (s.len() > 0 && s.len() <= base64_len && s.ends_with('=')) {
        use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
        let mut buf = vec![0u8; expected_bytes];
        let written =
            BASE64
                .decode_slice(s, &mut buf)
                .map_err(|e| Error::InvalidHashStringLength {
                    typ,
                    base16_len,
                    base32_len,
                    actual: s.len(),
                })?;
        buf.truncate(written);
        buf
    } else {
        return Err(Error::InvalidHashStringLength {
            typ,
            base16_len,
            base32_len,
            actual: s.len(),
        }
        .into());
    };
    assert!(v.len() == expected_bytes);
    Ok(v)
}
