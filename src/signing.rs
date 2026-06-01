use std::convert::TryInto;

use serde::{de, ser, Deserialize, Serialize};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine};
use displaydoc::Display;
use ed25519_compact::{KeyPair, PublicKey, Signature};

use anyhow::Result;

#[derive(Debug)]
pub struct NixKeypair {
    name: String,
    keypair: KeyPair,
}

#[derive(Debug, Clone)]
pub struct NixPublicKey {
    name: String,
    public: PublicKey,
}

#[derive(Debug, Display)]
pub enum Error {
    /// Signature error: {0}.
    SignatureError(ed25519_compact::Error),
    /// The string has a wrong key name attached to it: Our name is "{our_name}" and the string has "{string_name}".
    WrongKeyName {
        our_name: String,
        string_name: String,
    },
    /// The string lacks a colon separator.
    NoColonSeparator,
    /// The name portion of the string is blank.
    BlankKeyName,
    /// The payload portion of the string is blank.
    BlankPayload,
    /// Base64 decode error: {0}.
    Base64DecodeError(base64::DecodeError),
    /// Invalid base64 payload length: Expected {expected} ({usage}), got {actual}.
    InvalidPayloadLength {
        expected: usize,
        actual: usize,
        usage: &'static str,
    },
    /// Invalid signing key name "{0}".
    InvalidSigningKeyName(String),
}

impl std::error::Error for Error {}

impl NixKeypair {
    pub fn generate(name: &str) -> Result<Self> {
        let keypair = KeyPair::generate();
        validate_name(name)?;
        Ok(Self {
            name: name.to_string(),
            keypair,
        })
    }

    pub fn from_str(keypair: &str) -> Result<Self> {
        let (name, bytes) = decode_string(keypair, "keypair", KeyPair::BYTES, None)?;
        let keypair = KeyPair::from_slice(&bytes).map_err(Error::SignatureError)?;
        Ok(Self {
            name: name.to_string(),
            keypair,
        })
    }

    pub fn export_keypair(&self) -> String {
        format!("{}:{}", self.name, BASE64_STANDARD.encode(*self.keypair))
    }

    pub fn export_public_key(&self) -> String {
        format!("{}:{}", self.name, BASE64_STANDARD.encode(*self.keypair.pk))
    }

    pub fn to_public_key(&self) -> NixPublicKey {
        NixPublicKey {
            name: self.name.clone(),
            public: self.keypair.pk,
        }
    }

    pub fn sign(&self, message: &[u8]) -> String {
        let bytes = self.keypair.sk.sign(message, None);
        format!("{}:{}", self.name, BASE64_STANDARD.encode(bytes))
    }

    pub fn verify(&self, message: &[u8], signature: &str) -> Result<()> {
        let (_, bytes) = decode_string(signature, "signature", Signature::BYTES, Some(&self.name))?;
        let bytes: [u8; Signature::BYTES] = bytes.try_into().unwrap();
        let signature = Signature::from_slice(&bytes).map_err(Error::SignatureError)?;
        self.keypair
            .pk
            .verify(message, &signature)
            .map_err(|e| Error::SignatureError(e).into())
    }
}

impl<'de> Deserialize<'de> for NixKeypair {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        use de::Error;
        String::deserialize(deserializer)
            .and_then(|s| Self::from_str(&s).map_err(|e| Error::custom(e.to_string())))
    }
}

impl Serialize for NixKeypair {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: ser::Serializer,
    {
        serializer.serialize_str(&self.export_keypair())
    }
}

impl NixPublicKey {
    pub fn from_str(public_key: &str) -> Result<Self> {
        let (name, bytes) = decode_string(public_key, "public key", PublicKey::BYTES, None)?;
        let public = PublicKey::from_slice(&bytes).map_err(Error::SignatureError)?;
        Ok(Self {
            name: name.to_string(),
            public,
        })
    }

    pub fn export(&self) -> String {
        format!("{}:{}", self.name, BASE64_STANDARD.encode(*self.public))
    }

    pub fn verify(&self, message: &[u8], signature: &str) -> Result<()> {
        let (_, bytes) = decode_string(signature, "signature", Signature::BYTES, Some(&self.name))?;
        let bytes: [u8; Signature::BYTES] = bytes.try_into().unwrap();
        let signature = Signature::from_slice(&bytes).map_err(Error::SignatureError)?;
        self.public
            .verify(message, &signature)
            .map_err(|e| Error::SignatureError(e).into())
    }
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.find(':').is_some() {
        Err(Error::InvalidSigningKeyName(name.to_string()).into())
    } else {
        Ok(())
    }
}

fn decode_string<'s>(
    s: &'s str,
    usage: &'static str,
    expected_payload_length: usize,
    expected_name: Option<&str>,
) -> Result<(&'s str, Vec<u8>)> {
    let colon = s.find(':').ok_or(Error::NoColonSeparator)?;
    let (name, colon_and_payload) = s.split_at(colon);
    validate_name(name)?;
    if let Some(expected_name) = expected_name {
        if expected_name != name {
            return Err(Error::WrongKeyName {
                our_name: expected_name.to_string(),
                string_name: name.to_string(),
            }
            .into());
        }
    }
    let bytes = BASE64_STANDARD
        .decode(&colon_and_payload[1..])
        .map_err(Error::Base64DecodeError)?;
    if bytes.len() != expected_payload_length {
        return Err(Error::InvalidPayloadLength {
            actual: bytes.len(),
            expected: expected_payload_length,
            usage,
        }
        .into());
    }
    Ok((name, bytes))
}
