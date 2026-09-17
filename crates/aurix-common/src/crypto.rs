use ring::rand::SecureRandom;
use ring::{aead, rand as ring_rand};
use sha2::{Sha256, Digest};
use base64::Engine;
use crate::error::{AurixError, Result};

pub struct CryptoProvider {
    rng: ring_rand::SystemRandom,
}

impl CryptoProvider {
    pub fn new() -> Self {
        Self { rng: ring_rand::SystemRandom::new() }
    }

    pub fn generate_random_bytes(&self, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        self.rng.fill(&mut buf)
            .map_err(|_| AurixError::Encryption("Failed to generate random bytes".into()))?;
        Ok(buf)
    }

    pub fn generate_session_key(&self) -> Result<Vec<u8>> {
        self.generate_random_bytes(32)
    }

    pub fn generate_srtp_key(&self) -> Result<SrtpKeyMaterial> {
        Ok(SrtpKeyMaterial {
            master_key: self.generate_random_bytes(16)?,
            master_salt: self.generate_random_bytes(14)?,
        })
    }

    pub fn encrypt_aes256gcm(&self, key: &[u8], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        let unbound_key = aead::UnboundKey::new(&aead::AES_256_GCM, key)
            .map_err(|_| AurixError::Encryption("Invalid AES key".into()))?;
        let sealing_key = aead::LessSafeKey::new(unbound_key);
        let mut nonce_bytes = [0u8; 12];
        self.rng.fill(&mut nonce_bytes)
            .map_err(|_| AurixError::Encryption("Failed to generate nonce".into()))?;
        let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
        let mut in_out = plaintext.to_vec();
        sealing_key.seal_in_place_append_tag(nonce, aead::Aad::from(aad), &mut in_out)
            .map_err(|_| AurixError::Encryption("Encryption failed".into()))?;
        let mut result = Vec::with_capacity(12 + in_out.len());
        result.extend_from_slice(&nonce_bytes);
        result.extend_from_slice(&in_out);
        Ok(result)
    }

    pub fn decrypt_aes256gcm(&self, key: &[u8], ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
        if ciphertext.len() < 12 {
            return Err(AurixError::Encryption("Ciphertext too short".into()));
        }
        let (nonce_bytes, encrypted) = ciphertext.split_at(12);
        let unbound_key = aead::UnboundKey::new(&aead::AES_256_GCM, key)
            .map_err(|_| AurixError::Encryption("Invalid AES key".into()))?;
        let opening_key = aead::LessSafeKey::new(unbound_key);
        let mut nonce_arr = [0u8; 12];
        nonce_arr.copy_from_slice(nonce_bytes);
        let nonce = aead::Nonce::assume_unique_for_key(nonce_arr);
        let mut in_out = encrypted.to_vec();
        let plaintext = opening_key.open_in_place(nonce, aead::Aad::from(aad), &mut in_out)
            .map_err(|_| AurixError::Encryption("Decryption failed".into()))?;
        Ok(plaintext.to_vec())
    }

    pub fn generate_ssrc(&self) -> u32 {
        let mut buf = [0u8; 4];
        let _ = self.rng.fill(&mut buf);
        u32::from_be_bytes(buf) & 0x7FFFFFFF
    }
}

impl Default for CryptoProvider {
    fn default() -> Self { Self::new() }
}

pub struct SrtpKeyMaterial {
    pub master_key: Vec<u8>,
    pub master_salt: Vec<u8>,
}

pub fn hash_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let result = hasher.finalize();
    base64::engine::general_purpose::STANDARD.encode(result)
}

pub fn compute_audit_hash(previous_hash: &str, data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(previous_hash.as_bytes());
    hasher.update(data);
    let result = hasher.finalize();
    base64::engine::general_purpose::STANDARD.encode(result)
}