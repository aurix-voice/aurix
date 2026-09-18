use crate::error::{AurixError, Result};
use aes::cipher::{InnerIvInit, StreamCipher, StreamCipherCoreWrapper};
use base64::Engine;
use hmac::{Hmac, Mac};
use ring::rand::SecureRandom;
use ring::{aead, rand as ring_rand};
use sha1::Sha1;
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;
type HmacSha1 = Hmac<Sha1>;
type Aes256CtrCore = ctr::CtrCore<aes::Aes256, ctr::flavors::Ctr128BE>;

/// Per-session AURX media keys derived from the 32-byte master key handed out in
/// `SessionInitAck` (or from the cluster cascade secret for node-to-node relay).
///
/// ```text
/// auth = HMAC-SHA256(master, "AURXv2 auth")        -- packet authentication (truncated tag)
/// enc  = HMAC-SHA256(master, "AURXv2 enc")         -- AES-256-CTR payload encryption
/// salt = HMAC-SHA256(master, "AURXv2 salt")[0..16] -- XORed into the per-packet IV
/// ```
///
/// The IV of a packet is `salt XOR (type(1) | 0(1) | ssrc(4) | seq(4) | ts(4) | 0(2))`;
/// the trailing 16 zero bits are the CTR block counter, so a sender must never reuse
/// `(type, ssrc, sequence, timestamp)` under one key. Clients satisfy this with a single
/// monotonic sequence counter (which the server's anti-replay window already requires).
#[derive(Clone)]
pub struct MediaKeys {
    auth: [u8; 32],
    cipher: aes::Aes256,
    salt: [u8; 16],
}

impl std::fmt::Debug for MediaKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MediaKeys(..)")
    }
}

impl MediaKeys {
    pub const IV_SIZE: usize = 16;

    pub fn derive(master: &[u8]) -> Self {
        let auth = hmac_sha256(master, &[b"AURXv2 auth"]);
        let enc = hmac_sha256(master, &[b"AURXv2 enc"]);
        let salt_full = hmac_sha256(master, &[b"AURXv2 salt"]);
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&salt_full[..16]);
        Self {
            auth,
            cipher: <aes::Aes256 as aes::cipher::KeyInit>::new((&enc).into()),
            salt,
        }
    }

    pub fn auth_key(&self) -> &[u8; 32] {
        &self.auth
    }

    pub fn iv(&self, packet_type: u8, ssrc: u32, sequence: u32, timestamp: u32) -> [u8; 16] {
        let mut iv = [0u8; 16];
        iv[0] = packet_type;
        iv[2..6].copy_from_slice(&ssrc.to_be_bytes());
        iv[6..10].copy_from_slice(&sequence.to_be_bytes());
        iv[10..14].copy_from_slice(&timestamp.to_be_bytes());
        for (b, s) in iv.iter_mut().zip(self.salt.iter()) {
            *b ^= s;
        }
        iv
    }

    /// AES-256-CTR keystream application (encryption and decryption are the same operation).
    pub fn apply_ctr(&self, iv: &[u8; 16], data: &mut [u8]) {
        if data.is_empty() {
            return;
        }
        let core = Aes256CtrCore::inner_iv_init(self.cipher.clone(), iv.into());
        let mut ctr = StreamCipherCoreWrapper::from_core(core);
        ctr.apply_keystream(data);
    }
}

pub struct CryptoProvider {
    rng: ring_rand::SystemRandom,
}

impl CryptoProvider {
    pub fn new() -> Self {
        Self {
            rng: ring_rand::SystemRandom::new(),
        }
    }

    pub fn generate_random_bytes(&self, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        self.rng
            .fill(&mut buf)
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
        self.rng
            .fill(&mut nonce_bytes)
            .map_err(|_| AurixError::Encryption("Failed to generate nonce".into()))?;
        let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
        let mut in_out = plaintext.to_vec();
        sealing_key
            .seal_in_place_append_tag(nonce, aead::Aad::from(aad), &mut in_out)
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
        let plaintext = opening_key
            .open_in_place(nonce, aead::Aad::from(aad), &mut in_out)
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
    fn default() -> Self {
        Self::new()
    }
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

/// Constant-time equality check for secrets, hashes and authentication tags.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    for p in parts {
        mac.update(p);
    }
    let out = mac.finalize().into_bytes();
    let mut tag = [0u8; 32];
    tag.copy_from_slice(&out);
    tag
}

pub fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    let mut mac = HmacSha1::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    let out = mac.finalize().into_bytes();
    let mut tag = [0u8; 20];
    tag.copy_from_slice(&out);
    tag
}

/// Time-limited TURN credentials (coturn `use-auth-secret` compatible):
/// `username = "<expiry_unix>:<user>"`, `password = base64(HMAC-SHA1(secret, username))`.
#[derive(Debug, Clone)]
pub struct TurnCredentials {
    pub username: String,
    pub password: String,
    pub expires_at: i64,
}

pub fn generate_turn_credentials(
    secret: &str,
    user: &str,
    ttl_secs: i64,
    now_unix: i64,
) -> TurnCredentials {
    let expires_at = now_unix + ttl_secs;
    let username = format!("{expires_at}:{user}");
    let password = turn_password_for_username(secret, &username);
    TurnCredentials {
        username,
        password,
        expires_at,
    }
}

pub fn turn_password_for_username(secret: &str, username: &str) -> String {
    base64::engine::general_purpose::STANDARD
        .encode(hmac_sha1(secret.as_bytes(), username.as_bytes()))
}

/// Parse a time-limited TURN username, returning `(expiry_unix, user)` if well-formed.
pub fn parse_turn_username(username: &str) -> Option<(i64, &str)> {
    let (ts, user) = username.split_once(':')?;
    let ts = ts.parse::<i64>().ok()?;
    Some((ts, user))
}

/// RFC 5389 long-term credential key: MD5(username ":" realm ":" password).
pub fn stun_long_term_key(username: &str, realm: &str, password: &str) -> [u8; 16] {
    let digest = md5::Md5::digest(format!("{username}:{realm}:{password}").as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stun_long_term_key_is_md5_of_user_realm_pass() {
        let key = stun_long_term_key("user", "realm", "pass");
        let hex = key.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex, "8493fbc53ba582fb4c044c456bdc40eb");
    }

    #[test]
    fn turn_credentials_roundtrip() {
        let c = generate_turn_credentials("s3cret", "user-1", 600, 1_700_000_000);
        assert_eq!(c.username, "1700000600:user-1");
        assert_eq!(
            turn_password_for_username("s3cret", &c.username),
            c.password
        );
        assert_eq!(
            parse_turn_username(&c.username),
            Some((1_700_000_600, "user-1"))
        );
        assert!(parse_turn_username("garbage").is_none());
    }

    #[test]
    fn constant_time_eq_behaves() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
