// Copyright 2024 RustFS Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Data Encryption Key (DEK) encryption interface and implementations
//!
//! This module provides a unified interface for encrypting and decrypting
//! data encryption keys using master keys. It abstracts the encryption
//! operations so that different backends can share the same encryption logic.

#![allow(dead_code)] // Trait methods may be used by implementations

use crate::error::{KmsError, Result};
use async_trait::async_trait;
use jiff::{Timestamp, Zoned};
use rand::Rng;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::time::SystemTime;

/// Converts a serde_json Value (string or [secs, nsecs] array) into Zoned.
fn zoned_from_value(v: serde_json::Value) -> std::result::Result<Zoned, String> {
    match v {
        serde_json::Value::String(s) => {
            let normalized = normalize_zoned_str(&s);
            Zoned::from_str(&normalized).map_err(|e| e.to_string())
        }
        serde_json::Value::Array(arr) => {
            if arr.len() >= 2 {
                let secs = arr[0].as_i64().ok_or("zoned array[0]: expected integer seconds")?;
                let nsecs = arr[1].as_i64().ok_or("zoned array[1]: expected integer nanoseconds")? as i32;
                let ts = Timestamp::new(secs, nsecs).map_err(|e| e.to_string())?;
                Ok(Zoned::new(ts, jiff::tz::TimeZone::UTC))
            } else {
                Err("zoned array: expected [secs, nsecs] with 2 elements".to_string())
            }
        }
        _ => Err("created_at: expected string or [secs, nsecs] array".to_string()),
    }
}

/// Normalizes a zoned datetime string for parsing: if it lacks the RFC 8536
/// bracket time zone (e.g. `[UTC]`), appends or rewrites so the parser accepts it.
fn normalize_zoned_str(s: &str) -> String {
    let s = s.trim();
    if s.contains('[') {
        return s.to_string();
    }
    if s.ends_with('Z') {
        let base = s.trim_end_matches('Z');
        return format!("{}+00:00[UTC]", base);
    }
    if s.ends_with("+0000") {
        let base = s.trim_end_matches("+0000");
        return format!("{}+00:00[UTC]", base);
    }
    if s.ends_with("+00:00") {
        let base = s.trim_end_matches("+00:00");
        return format!("{}+00:00[UTC]", base);
    }
    format!("{}+00:00[UTC]", s)
}

/// Deserializes a `Zoned` from JSON, accepting: (1) RFC 8536 string with bracket,
/// (2) legacy string formats (e.g. trailing `Z` or `+00:00` without `[UTC]`),
/// (3) a two-element array `[secs, nsecs]` (e.g. from jiff's default serde).
/// Uses Value deserialization so both string and array are accepted regardless of format.
pub fn deserialize_zoned_utc_compatible<'de, D>(d: D) -> std::result::Result<Zoned, D::Error>
where
    D: Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    zoned_from_value(v).map_err(serde::de::Error::custom)
}

/// Deserializes `Option<Zoned>` with the same lenient rules as `deserialize_zoned_utc_compatible`.
pub fn deserialize_opt_zoned_utc_compatible<'de, D>(d: D) -> std::result::Result<Option<Zoned>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt: Option<serde_json::Value> = Option::deserialize(d)?;
    match opt {
        None => Ok(None),
        Some(v) => zoned_from_value(v).map(Some).map_err(serde::de::Error::custom),
    }
}

/// Newtype that deserializes from either string or `[secs, nsecs]` array.
/// Use this as the field type so the type system always uses our Deserialize impl.
#[derive(Debug, Clone, Serialize)]
pub struct ZonedUtcCompatible(#[serde(serialize_with = "serialize_zoned")] pub Zoned);

fn serialize_zoned<S>(z: &Zoned, s: S) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    z.serialize(s)
}

impl<'de> Deserialize<'de> for ZonedUtcCompatible {
    fn deserialize<D>(d: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let v = serde_json::Value::deserialize(d)?;
        zoned_from_value(v).map(ZonedUtcCompatible).map_err(serde::de::Error::custom)
    }
}

impl std::ops::Deref for ZonedUtcCompatible {
    type Target = Zoned;
    fn deref(&self) -> &Zoned {
        &self.0
    }
}

/// Current time in UTC for use in serialized envelopes.
/// Using UTC ensures the string always includes the RFC 8536 time zone annotation
/// (e.g. `+00:00[UTC]`), so deserialization works regardless of system timezone.
#[inline]
pub fn zoned_now_utc() -> Zoned {
    Zoned::new(
        Timestamp::try_from(SystemTime::now()).expect("system time valid"),
        jiff::tz::TimeZone::UTC,
    )
}

/// Data key envelope for encrypting/decrypting data keys
///
/// This structure stores the encrypted DEK along with metadata needed for decryption.
/// The `master_key_version` field records which version of the KEK (Key Encryption Key)
/// was used to encrypt this DEK, enabling proper key rotation support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataKeyEnvelope {
    pub key_id: String,
    pub master_key_id: String,
    pub key_spec: String,
    pub encrypted_key: Vec<u8>,
    pub nonce: Vec<u8>,
    pub encryption_context: HashMap<String, String>,
    pub created_at: ZonedUtcCompatible,
}

/// Trait for encrypting and decrypting data encryption keys (DEK)
///
/// This trait abstracts the encryption operations used to protect
/// data encryption keys with master keys. Different implementations
/// can use different encryption algorithms (e.g., AES-256-GCM).
#[async_trait]
pub trait DekCrypto: Send + Sync {
    /// Encrypt plaintext data using a master key material
    ///
    /// # Arguments
    /// * `key_material` - The master key material (raw bytes)
    /// * `plaintext` - The data to encrypt
    ///
    /// # Returns
    /// A tuple of (ciphertext, nonce) where:
    /// - `ciphertext` - The encrypted data
    /// - `nonce` - The nonce used for encryption (should be stored with ciphertext)
    async fn encrypt(&self, key_material: &[u8], plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>)>;

    /// Decrypt ciphertext data using a master key material
    ///
    /// # Arguments
    /// * `key_material` - The master key material (raw bytes)
    /// * `ciphertext` - The encrypted data
    /// * `nonce` - The nonce used for encryption
    ///
    /// # Returns
    /// The decrypted plaintext data
    async fn decrypt(&self, key_material: &[u8], ciphertext: &[u8], nonce: &[u8]) -> Result<Vec<u8>>;

    /// Get the algorithm name used by this implementation
    #[allow(dead_code)] // May be used by implementations or for debugging
    fn algorithm(&self) -> &'static str;

    /// Get the required key material size in bytes
    #[allow(dead_code)] // May be used by implementations or for debugging
    fn key_size(&self) -> usize;
}

/// AES-256-GCM implementation of DEK encryption
pub struct AesDekCrypto;

impl AesDekCrypto {
    /// Create a new AES-256-GCM DEK crypto instance
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl DekCrypto for AesDekCrypto {
    async fn encrypt(&self, key_material: &[u8], plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        use aes_gcm::{
            Aes256Gcm, Key, Nonce,
            aead::{Aead, KeyInit},
        };

        // Validate key material length
        if key_material.len() != 32 {
            return Err(KmsError::cryptographic_error(
                "key",
                format!("Invalid key length: expected 32 bytes, got {}", key_material.len()),
            ));
        }

        // Create cipher from key material
        let key =
            Key::<Aes256Gcm>::try_from(key_material).map_err(|_| KmsError::cryptographic_error("key", "Invalid key length"))?;
        let cipher = Aes256Gcm::new(&key);

        // Generate random nonce (12 bytes for GCM)
        let mut nonce_bytes = [0u8; 12];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from(nonce_bytes);

        // Encrypt plaintext
        let ciphertext = cipher
            .encrypt(&nonce, plaintext)
            .map_err(|e| KmsError::cryptographic_error("encrypt", e.to_string()))?;

        Ok((ciphertext, nonce_bytes.to_vec()))
    }

    async fn decrypt(&self, key_material: &[u8], ciphertext: &[u8], nonce: &[u8]) -> Result<Vec<u8>> {
        use aes_gcm::{
            Aes256Gcm, Key, Nonce,
            aead::{Aead, KeyInit},
        };

        // Validate nonce length
        if nonce.len() != 12 {
            return Err(KmsError::cryptographic_error("nonce", "Invalid nonce length: expected 12 bytes"));
        }

        // Validate key material length
        if key_material.len() != 32 {
            return Err(KmsError::cryptographic_error(
                "key",
                format!("Invalid key length: expected 32 bytes, got {}", key_material.len()),
            ));
        }

        // Create cipher from key material
        let key =
            Key::<Aes256Gcm>::try_from(key_material).map_err(|_| KmsError::cryptographic_error("key", "Invalid key length"))?;
        let cipher = Aes256Gcm::new(&key);

        // Convert nonce
        let mut nonce_array = [0u8; 12];
        nonce_array.copy_from_slice(nonce);
        let nonce_ref = Nonce::from(nonce_array);

        // Decrypt ciphertext
        let plaintext = cipher
            .decrypt(&nonce_ref, ciphertext)
            .map_err(|e| KmsError::cryptographic_error("decrypt", e.to_string()))?;

        Ok(plaintext)
    }

    #[allow(dead_code)] // Trait method, may be used by implementations
    fn algorithm(&self) -> &'static str {
        "AES-256-GCM"
    }

    #[allow(dead_code)] // Trait method, may be used by implementations
    fn key_size(&self) -> usize {
        32 // 256 bits
    }
}

impl Default for AesDekCrypto {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate random key material for the given algorithm
///
/// # Arguments
/// * `algorithm` - The key algorithm (e.g., "AES_256", "AES_128")
///
/// # Returns
/// A vector containing the generated key material
pub fn generate_key_material(algorithm: &str) -> Result<Vec<u8>> {
    let key_size = match algorithm {
        "AES_256" => 32,
        "AES_128" => 16,
        _ => return Err(KmsError::unsupported_algorithm(algorithm)),
    };

    let mut key_material = vec![0u8; key_size];
    rand::rng().fill_bytes(&mut key_material);
    Ok(key_material)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_aes_dek_crypto_encrypt_decrypt() {
        let crypto = AesDekCrypto::new();

        // Generate test key material
        let key_material = generate_key_material("AES_256").expect("Failed to generate key material");
        let plaintext = b"Hello, World! This is a test message.";

        // Test encryption
        let (ciphertext, nonce) = crypto
            .encrypt(&key_material, plaintext)
            .await
            .expect("Encryption should succeed");

        assert!(!ciphertext.is_empty());
        assert_eq!(nonce.len(), 12);
        assert_ne!(ciphertext, plaintext);

        // Test decryption
        let decrypted = crypto
            .decrypt(&key_material, &ciphertext, &nonce)
            .await
            .expect("Decryption should succeed");

        assert_eq!(decrypted, plaintext);
    }

    #[tokio::test]
    async fn test_aes_dek_crypto_invalid_key_size() {
        let crypto = AesDekCrypto::new();
        let invalid_key = vec![0u8; 16]; // Too short
        let plaintext = b"test";

        let result = crypto.encrypt(&invalid_key, plaintext).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_aes_dek_crypto_invalid_nonce() {
        let crypto = AesDekCrypto::new();
        let key_material = generate_key_material("AES_256").expect("Failed to generate key material");
        let ciphertext = vec![0u8; 16];
        let invalid_nonce = vec![0u8; 8]; // Too short

        let result = crypto.decrypt(&key_material, &ciphertext, &invalid_nonce).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_generate_key_material() {
        let key_256 = generate_key_material("AES_256").expect("Should generate AES_256 key");
        assert_eq!(key_256.len(), 32);

        let key_128 = generate_key_material("AES_128").expect("Should generate AES_128 key");
        assert_eq!(key_128.len(), 16);

        // Keys should be different
        let key_256_2 = generate_key_material("AES_256").expect("Should generate AES_256 key");
        assert_ne!(key_256, key_256_2);

        // Invalid algorithm
        assert!(generate_key_material("INVALID").is_err());
    }

    #[tokio::test]
    async fn test_data_key_envelope_serialization() {
        let envelope = DataKeyEnvelope {
            key_id: "test-key-id".to_string(),
            master_key_id: "master-key-id".to_string(),
            key_spec: "AES_256".to_string(),
            encrypted_key: vec![1, 2, 3, 4],
            nonce: vec![5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            encryption_context: {
                let mut map = HashMap::new();
                map.insert("bucket".to_string(), "test-bucket".to_string());
                map
            },
            created_at: ZonedUtcCompatible(zoned_now_utc()),
        };

        // Test serialization
        let serialized = serde_json::to_vec(&envelope).expect("Serialization should succeed");
        assert!(!serialized.is_empty());

        // Test deserialization
        let deserialized: DataKeyEnvelope = serde_json::from_slice(&serialized).expect("Deserialization should succeed");
        assert_eq!(deserialized.key_id, envelope.key_id);
        assert_eq!(deserialized.master_key_id, envelope.master_key_id);
        assert_eq!(deserialized.encrypted_key, envelope.encrypted_key);
    }

    #[tokio::test]
    async fn test_data_key_envelope_backward_compatibility() {
        // Test deserialization with current Zoned format (with timezone annotation)
        let envelope_json = r#"{
            "key_id": "test-key-id",
            "master_key_id": "master-key-id",
            "key_spec": "AES_256",
            "encrypted_key": [1, 2, 3, 4],
            "nonce": [5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            "encryption_context": {"bucket": "test-bucket"},
            "created_at": "2024-01-01T00:00:00+00:00[UTC]"
        }"#;

        let deserialized: DataKeyEnvelope = serde_json::from_str(envelope_json).expect("Should deserialize current format");
        assert_eq!(deserialized.key_id, "test-key-id");
        assert_eq!(deserialized.master_key_id, "master-key-id");
    }
}
