//! Group end-to-end encryption of voice frames ("Aurix E2EE v1").
//!
//! Every client holds an X25519 *identity key* for the lifetime of its session and a
//! symmetric *sender key* it encrypts its own frames with. The sender key is wrapped
//! individually for every peer (X25519 static-static agreement → HKDF-SHA256 → AES-256-CTR +
//! HMAC-SHA256) and delivered through the control plane, which the server relays without
//! being able to read it. Sender keys are per *sender*, not per channel: a native client
//! encodes and encrypts each frame once for all the encrypted channels it transmits into.
//!
//! ```text
//! frame  = generation(1) | counter(4, BE) | AES-256-CTR(enc, IV, opus) | HMAC-SHA256(auth, header|ct)[..10]
//! IV     = (salt(12) XOR (generation | counter | 0*7)) | 0*4         -- low 32 bits: block counter
//! enc    = HKDF-SHA256(secret, "aurix-e2ee-v1 enc", 32)
//! auth   = HKDF-SHA256(secret, "aurix-e2ee-v1 auth", 32)
//! salt   = HKDF-SHA256(secret, "aurix-e2ee-v1 salt", 12)
//!
//! wrap   = nonce(12) | AES-256-CTR(wk_enc, nonce|0*4, secret) | HMAC-SHA256(wk_auth, generation|nonce|ct)[..16]
//! shared = X25519(sender_sk, recipient_pk)
//! wk_*   = HKDF-SHA256(shared, salt = sender_pk|recipient_pk, "aurix-e2ee-v1 wrap enc" / "... wrap auth", 32)
//! ```
//!
//! Rotation: a sender picks a fresh secret (generation + 1) whenever a peer joins (so the
//! newcomer cannot read earlier frames) or leaves (so the leaver cannot read later ones).
//! Receivers keep the last few generations of every peer so frames in flight across a
//! rotation still decrypt. Identity keys are authenticated by the platform's control plane
//! only; applications that need protection against a malicious node compare
//! [`fingerprint`]s out of band.

use std::collections::{HashMap, HashSet, VecDeque};

use aes::cipher::{InnerIvInit, StreamCipher, StreamCipherCoreWrapper};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::{AurixError, Result};
use crate::types::{ChannelId, UserId};

type HmacSha256 = Hmac<Sha256>;
type Aes256CtrCore = ctr::CtrCore<aes::Aes256, ctr::flavors::Ctr128BE>;

/// Protocol version carried implicitly by `AudioPolicy::e2ee`; bumped on any format change.
pub const VERSION: u8 = 1;
pub const PUBLIC_KEY_LEN: usize = 32;
pub const SECRET_LEN: usize = 32;
pub const FRAME_HEADER_LEN: usize = 5;
pub const FRAME_TAG_LEN: usize = 10;
/// Bytes an encrypted frame is longer than the Opus frame inside it.
pub const FRAME_OVERHEAD: usize = FRAME_HEADER_LEN + FRAME_TAG_LEN;
pub const WRAP_NONCE_LEN: usize = 12;
pub const WRAP_TAG_LEN: usize = 16;
/// Length of a wrapped sender key.
pub const WRAPPED_KEY_LEN: usize = WRAP_NONCE_LEN + SECRET_LEN + WRAP_TAG_LEN;
/// Generations of one peer a receiver keeps decrypting.
pub const KEPT_GENERATIONS: usize = 4;
/// A sender rotates before its frame counter gets anywhere near wrapping.
pub const ROTATE_AT_COUNTER: u32 = 1 << 31;
/// Out-of-order tolerance of the per-generation replay window.
const REPLAY_WINDOW: u32 = 128;

const INFO_ENC: &[u8] = b"aurix-e2ee-v1 enc";
const INFO_AUTH: &[u8] = b"aurix-e2ee-v1 auth";
const INFO_SALT: &[u8] = b"aurix-e2ee-v1 salt";
const INFO_WRAP_ENC: &[u8] = b"aurix-e2ee-v1 wrap enc";
const INFO_WRAP_AUTH: &[u8] = b"aurix-e2ee-v1 wrap auth";

/// HKDF-SHA256 (RFC 5869), single-block-per-iteration expand.
pub fn hkdf_sha256(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let mut prk = HmacSha256::new_from_slice(salt).expect("HMAC accepts any key length");
    prk.update(ikm);
    let prk = prk.finalize().into_bytes();
    let mut out = Vec::with_capacity(len.div_ceil(32) * 32);
    let mut prev: Vec<u8> = Vec::new();
    let mut i = 1u8;
    while out.len() < len {
        let mut mac = HmacSha256::new_from_slice(&prk).expect("HMAC accepts any key length");
        mac.update(&prev);
        mac.update(info);
        mac.update(&[i]);
        prev = mac.finalize().into_bytes().to_vec();
        out.extend_from_slice(&prev);
        i += 1;
    }
    out.truncate(len);
    out
}

fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    for p in parts {
        mac.update(p);
    }
    mac.finalize().into_bytes().into()
}

fn aes256_ctr(key: &[u8; 32], iv: &[u8; 16], data: &mut [u8]) {
    if data.is_empty() {
        return;
    }
    let cipher = <aes::Aes256 as aes::cipher::KeyInit>::new(key.into());
    let core = Aes256CtrCore::inner_iv_init(cipher, iv.into());
    let mut ctr = StreamCipherCoreWrapper::from_core(core);
    ctr.apply_keystream(data);
}

/// Hex SHA-256 of an identity public key, the value users compare out of band.
pub fn fingerprint(public_key: &[u8; PUBLIC_KEY_LEN]) -> String {
    let digest = Sha256::digest(public_key);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Long-lived X25519 key of one client.
#[derive(Clone)]
pub struct IdentityKey {
    secret: StaticSecret,
    public: PublicKey,
}

impl std::fmt::Debug for IdentityKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IdentityKey({})", fingerprint(self.public.as_bytes()))
    }
}

impl IdentityKey {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self::from_bytes(bytes)
    }

    pub fn from_bytes(secret: [u8; 32]) -> Self {
        let secret = StaticSecret::from(secret);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    pub fn public_key(&self) -> &[u8; PUBLIC_KEY_LEN] {
        self.public.as_bytes()
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(self.public.as_bytes())
    }

    /// Wrapping keys shared with `peer`; `sender_pk`/`recipient_pk` fix the direction.
    fn wrap_keys(
        &self,
        peer: &[u8; PUBLIC_KEY_LEN],
        sender_pk: &[u8; PUBLIC_KEY_LEN],
        recipient_pk: &[u8; PUBLIC_KEY_LEN],
    ) -> Result<([u8; 32], [u8; 32])> {
        let shared = self.secret.diffie_hellman(&PublicKey::from(*peer));
        if !shared.was_contributory() {
            return Err(AurixError::Encryption("low-order X25519 public key".into()));
        }
        let mut salt = [0u8; 64];
        salt[..32].copy_from_slice(sender_pk);
        salt[32..].copy_from_slice(recipient_pk);
        let enc = hkdf_sha256(&salt, shared.as_bytes(), INFO_WRAP_ENC, 32);
        let auth = hkdf_sha256(&salt, shared.as_bytes(), INFO_WRAP_AUTH, 32);
        Ok((
            enc.try_into().expect("32 bytes"),
            auth.try_into().expect("32 bytes"),
        ))
    }

    /// Seals `secret` (generation `generation`) for the peer holding `recipient`.
    pub fn wrap(
        &self,
        recipient: &[u8; PUBLIC_KEY_LEN],
        generation: u8,
        secret: &[u8; SECRET_LEN],
    ) -> Result<Vec<u8>> {
        let (enc, auth) = self.wrap_keys(recipient, self.public.as_bytes(), recipient)?;
        let mut nonce = [0u8; WRAP_NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce);
        let mut iv = [0u8; 16];
        iv[..WRAP_NONCE_LEN].copy_from_slice(&nonce);
        let mut ct = *secret;
        aes256_ctr(&enc, &iv, &mut ct);
        let tag = hmac_sha256(&auth, &[&[generation], &nonce, &ct]);
        let mut out = Vec::with_capacity(WRAPPED_KEY_LEN);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ct);
        out.extend_from_slice(&tag[..WRAP_TAG_LEN]);
        Ok(out)
    }

    /// Opens a key wrapped by the peer holding `sender` for us.
    pub fn unwrap(
        &self,
        sender: &[u8; PUBLIC_KEY_LEN],
        generation: u8,
        wrapped: &[u8],
    ) -> Result<[u8; SECRET_LEN]> {
        if wrapped.len() != WRAPPED_KEY_LEN {
            return Err(AurixError::Encryption(
                "wrapped key has a wrong length".into(),
            ));
        }
        let (enc, auth) = self.wrap_keys(sender, sender, self.public.as_bytes())?;
        let (nonce, rest) = wrapped.split_at(WRAP_NONCE_LEN);
        let (ct, tag) = rest.split_at(SECRET_LEN);
        let expected = hmac_sha256(&auth, &[&[generation], nonce, ct]);
        if expected[..WRAP_TAG_LEN].ct_eq(tag).unwrap_u8() != 1 {
            return Err(AurixError::Encryption(
                "wrapped key failed authentication".into(),
            ));
        }
        let mut iv = [0u8; 16];
        iv[..WRAP_NONCE_LEN].copy_from_slice(nonce);
        let mut secret = [0u8; SECRET_LEN];
        secret.copy_from_slice(ct);
        aes256_ctr(&enc, &iv, &mut secret);
        Ok(secret)
    }
}

/// Frame keys of one sender generation.
#[derive(Clone)]
pub struct SenderKey {
    generation: u8,
    enc: [u8; 32],
    auth: [u8; 32],
    salt: [u8; 12],
}

impl std::fmt::Debug for SenderKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SenderKey(gen {})", self.generation)
    }
}

impl SenderKey {
    pub fn derive(generation: u8, secret: &[u8; SECRET_LEN]) -> Self {
        let enc = hkdf_sha256(&[], secret, INFO_ENC, 32);
        let auth = hkdf_sha256(&[], secret, INFO_AUTH, 32);
        let salt = hkdf_sha256(&[], secret, INFO_SALT, 12);
        Self {
            generation,
            enc: enc.try_into().expect("32 bytes"),
            auth: auth.try_into().expect("32 bytes"),
            salt: salt.try_into().expect("12 bytes"),
        }
    }

    pub fn generation(&self) -> u8 {
        self.generation
    }

    fn iv(&self, counter: u32) -> [u8; 16] {
        let mut iv = [0u8; 16];
        iv[0] = self.generation;
        iv[1..5].copy_from_slice(&counter.to_be_bytes());
        for (b, s) in iv.iter_mut().zip(self.salt.iter()) {
            *b ^= s;
        }
        iv
    }

    /// Encrypts `plain` as frame number `counter` of this generation.
    pub fn seal(&self, counter: u32, plain: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(plain.len() + FRAME_OVERHEAD);
        out.push(self.generation);
        out.extend_from_slice(&counter.to_be_bytes());
        out.extend_from_slice(plain);
        aes256_ctr(&self.enc, &self.iv(counter), &mut out[FRAME_HEADER_LEN..]);
        let tag = hmac_sha256(&self.auth, &[&out]);
        out.extend_from_slice(&tag[..FRAME_TAG_LEN]);
        out
    }

    /// Generation byte and counter of an encrypted frame (no authentication).
    pub fn peek(frame: &[u8]) -> Option<(u8, u32)> {
        if frame.len() < FRAME_OVERHEAD {
            return None;
        }
        Some((
            frame[0],
            u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]),
        ))
    }

    /// Authenticates and decrypts a frame of this generation.
    pub fn open(&self, frame: &[u8]) -> Result<Vec<u8>> {
        let (generation, counter) = Self::peek(frame)
            .ok_or_else(|| AurixError::Encryption("E2EE frame too short".into()))?;
        if generation != self.generation {
            return Err(AurixError::Encryption(
                "E2EE frame generation mismatch".into(),
            ));
        }
        let (body, tag) = frame.split_at(frame.len() - FRAME_TAG_LEN);
        let expected = hmac_sha256(&self.auth, &[body]);
        if expected[..FRAME_TAG_LEN].ct_eq(tag).unwrap_u8() != 1 {
            return Err(AurixError::Encryption(
                "E2EE frame failed authentication".into(),
            ));
        }
        let mut plain = body[FRAME_HEADER_LEN..].to_vec();
        aes256_ctr(&self.enc, &self.iv(counter), &mut plain);
        Ok(plain)
    }
}

/// Anti-replay state of one received generation.
#[derive(Clone, Debug)]
struct ReplayState {
    highest: Option<u32>,
    /// Bit `i` set: `highest - i` was seen.
    window: u128,
}

impl ReplayState {
    fn new() -> Self {
        Self {
            highest: None,
            window: 0,
        }
    }

    /// `true` (and records it) when `counter` was not seen before and is not too old.
    fn accept(&mut self, counter: u32) -> bool {
        let Some(h) = self.highest else {
            self.highest = Some(counter);
            self.window = 1;
            return true;
        };
        if counter > h {
            let shift = counter - h;
            self.window = if shift >= REPLAY_WINDOW {
                0
            } else {
                self.window << shift
            };
            self.window |= 1;
            self.highest = Some(counter);
            return true;
        }
        let age = h - counter;
        if age >= REPLAY_WINDOW {
            return false;
        }
        let bit = 1u128 << age;
        if self.window & bit != 0 {
            return false;
        }
        self.window |= bit;
        true
    }
}

/// Receiving side of one peer: its last [`KEPT_GENERATIONS`] sender keys.
#[derive(Clone, Debug, Default)]
pub struct PeerKeys {
    generations: VecDeque<(SenderKey, ReplayState)>,
}

impl PeerKeys {
    pub fn insert(&mut self, key: SenderKey) {
        self.generations
            .retain(|(k, _)| k.generation != key.generation);
        self.generations.push_back((key, ReplayState::new()));
        while self.generations.len() > KEPT_GENERATIONS {
            self.generations.pop_front();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.generations.is_empty()
    }

    pub fn has_generation(&self, generation: u8) -> bool {
        self.generations
            .iter()
            .any(|(k, _)| k.generation == generation)
    }

    /// Decrypts `frame`, rejecting unknown generations and replayed counters.
    pub fn open(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        let (generation, counter) = SenderKey::peek(frame)
            .ok_or_else(|| AurixError::Encryption("E2EE frame too short".into()))?;
        let (key, replay) = self
            .generations
            .iter_mut()
            .find(|(k, _)| k.generation == generation)
            .ok_or_else(|| AurixError::Encryption("no key for this E2EE generation".into()))?;
        let plain = key.open(frame)?;
        if !replay.accept(counter) {
            return Err(AurixError::Encryption("replayed E2EE frame".into()));
        }
        Ok(plain)
    }
}

/// A control-plane message the group wants sent; the server relays both without reading them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outgoing {
    /// "I am in `channel_id` with this identity key" — to every other member.
    Hello { channel_id: ChannelId },
    /// Our current sender key, wrapped for `to`.
    SenderKey {
        channel_id: ChannelId,
        to: UserId,
        generation: u8,
        wrapped: Vec<u8>,
    },
}

#[derive(Clone, Debug)]
struct Peer {
    public_key: [u8; PUBLIC_KEY_LEN],
    /// Encrypted channels we share with the peer (any one of them routes our messages).
    channels: HashSet<ChannelId>,
    keys: PeerKeys,
    /// Generation of our key we last wrapped for this peer.
    sent_generation: Option<u8>,
}

/// Why [`Group::apply`] changed a peer's trust state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerChange {
    /// First identity key seen for this peer.
    New,
    /// The peer's identity key differs from the one we knew (reconnect, or an impostor).
    KeyChanged { previous_fingerprint: String },
}

/// One client's view of the encrypted groups it belongs to: its identity, its sender key
/// and the peers (across all its encrypted channels) it exchanges keys with.
#[derive(Debug)]
pub struct Group {
    identity: IdentityKey,
    secret: [u8; SECRET_LEN],
    key: SenderKey,
    counter: u32,
    channels: HashSet<ChannelId>,
    peers: HashMap<UserId, Peer>,
    rotation_pending: bool,
}

impl Group {
    pub fn new(identity: IdentityKey) -> Self {
        let mut secret = [0u8; SECRET_LEN];
        rand::thread_rng().fill_bytes(&mut secret);
        let key = SenderKey::derive(0, &secret);
        Self {
            identity,
            secret,
            key,
            counter: 0,
            channels: HashSet::new(),
            peers: HashMap::new(),
            rotation_pending: false,
        }
    }

    pub fn identity(&self) -> &IdentityKey {
        &self.identity
    }

    pub fn generation(&self) -> u8 {
        self.key.generation
    }

    /// Encrypted channels we are currently in.
    pub fn channels(&self) -> &HashSet<ChannelId> {
        &self.channels
    }

    pub fn is_encrypted(&self, channel_id: &ChannelId) -> bool {
        self.channels.contains(channel_id)
    }

    /// Whether any encrypted channel is joined, i.e. our uplink must be encrypted.
    pub fn active(&self) -> bool {
        !self.channels.is_empty()
    }

    pub fn peer_fingerprint(&self, user_id: &UserId) -> Option<String> {
        self.peers.get(user_id).map(|p| fingerprint(&p.public_key))
    }

    /// Peers we hold a sender key of (we can decrypt their frames).
    pub fn decryptable_peers(&self) -> Vec<UserId> {
        self.peers
            .iter()
            .filter(|(_, p)| !p.keys.is_empty())
            .map(|(u, _)| *u)
            .collect()
    }

    pub fn has_key_for(&self, user_id: &UserId) -> bool {
        self.peers.get(user_id).is_some_and(|p| !p.keys.is_empty())
    }

    /// Whether `user_id` is a known peer whose key for `generation` has not arrived (yet).
    /// A frame of such a generation is not corrupt: its key is still in flight on the
    /// control plane, which media over UDP can overtake.
    pub fn awaiting_generation(&self, user_id: &UserId, generation: u8) -> bool {
        self.peers
            .get(user_id)
            .is_some_and(|p| !p.keys.has_generation(generation))
    }

    /// Whether a rotation is due ([`Group::rotate`] performs it).
    pub fn rotation_pending(&self) -> bool {
        self.rotation_pending
    }

    /// We joined an encrypted channel: announce ourselves so members send us their keys (and
    /// rotate for us).
    pub fn joined(&mut self, channel_id: ChannelId) -> Vec<Outgoing> {
        if !self.channels.insert(channel_id) {
            return Vec::new();
        }
        vec![Outgoing::Hello { channel_id }]
    }

    /// Our membership of `channel_id` was re-acknowledged (session resume): peers absent from
    /// the server's `members` roster left while we were away and are forgotten (rotating away
    /// from them); a fresh hello makes the remaining members re-send keys we may have missed.
    /// Returns the forgotten peers along with the messages to send.
    pub fn rejoined(
        &mut self,
        channel_id: ChannelId,
        members: &HashSet<UserId>,
    ) -> (Vec<Outgoing>, Vec<UserId>) {
        if !self.channels.contains(&channel_id) {
            return (self.joined(channel_id), Vec::new());
        }
        let absent: Vec<UserId> = self
            .peers
            .iter()
            .filter(|(u, p)| p.channels.contains(&channel_id) && !members.contains(u))
            .map(|(u, _)| *u)
            .collect();
        let mut gone = Vec::new();
        for user in absent {
            self.peer_left(&channel_id, &user);
            if !self.peers.contains_key(&user) {
                gone.push(user);
            }
        }
        (vec![Outgoing::Hello { channel_id }], gone)
    }

    /// We left an encrypted channel: peers we no longer share any channel with are dropped
    /// (and our key rotates away from them).
    pub fn left(&mut self, channel_id: &ChannelId) {
        if !self.channels.remove(channel_id) {
            return;
        }
        let mut gone = Vec::new();
        for (user, peer) in self.peers.iter_mut() {
            peer.channels.remove(channel_id);
            if peer.channels.is_empty() {
                gone.push(*user);
            }
        }
        for user in gone {
            self.peers.remove(&user);
            self.rotation_pending = true;
        }
    }

    /// Drops every channel and peer (session ended); the identity key stays.
    pub fn reset(&mut self) {
        self.channels.clear();
        self.peers.clear();
        self.rotation_pending = true;
    }

    /// A peer left `channel_id` (or the channel as a whole went away for them).
    pub fn peer_left(&mut self, channel_id: &ChannelId, user_id: &UserId) {
        let Some(peer) = self.peers.get_mut(user_id) else {
            return;
        };
        peer.channels.remove(channel_id);
        if peer.channels.is_empty() {
            self.peers.remove(user_id);
            self.rotation_pending = true;
        }
    }

    /// Records a peer's identity key from a `Hello` or `SenderKey`; returns what changed.
    fn learn_peer(
        &mut self,
        channel_id: ChannelId,
        user_id: UserId,
        public_key: [u8; PUBLIC_KEY_LEN],
    ) -> Option<PeerChange> {
        match self.peers.get_mut(&user_id) {
            Some(peer) if peer.public_key == public_key => {
                peer.channels.insert(channel_id);
                None
            }
            Some(peer) => {
                let previous_fingerprint = fingerprint(&peer.public_key);
                peer.public_key = public_key;
                peer.channels.insert(channel_id);
                peer.keys = PeerKeys::default();
                peer.sent_generation = None;
                self.rotation_pending = true;
                Some(PeerChange::KeyChanged {
                    previous_fingerprint,
                })
            }
            None => {
                self.peers.insert(
                    user_id,
                    Peer {
                        public_key,
                        channels: HashSet::from([channel_id]),
                        keys: PeerKeys::default(),
                        sent_generation: None,
                    },
                );
                self.rotation_pending = true;
                Some(PeerChange::New)
            }
        }
    }

    /// A peer announced itself in `channel_id`. A peer we already trust gets our current key
    /// right away (they may have lost it); a new or re-keyed peer triggers a rotation instead,
    /// so they only ever receive a key that post-dates their arrival.
    pub fn on_hello(
        &mut self,
        channel_id: ChannelId,
        user_id: UserId,
        public_key: [u8; PUBLIC_KEY_LEN],
    ) -> Result<(Vec<Outgoing>, Option<PeerChange>)> {
        if !self.channels.contains(&channel_id) {
            return Ok((Vec::new(), None));
        }
        let change = self.learn_peer(channel_id, user_id, public_key);
        if change.is_some() {
            return Ok((Vec::new(), change));
        }
        let out = self.wrap_for(&user_id)?.into_iter().collect();
        Ok((out, None))
    }

    /// A peer sent us their sender key.
    pub fn on_sender_key(
        &mut self,
        channel_id: ChannelId,
        user_id: UserId,
        public_key: [u8; PUBLIC_KEY_LEN],
        generation: u8,
        wrapped: &[u8],
    ) -> Result<(Vec<Outgoing>, Option<PeerChange>)> {
        if !self.channels.contains(&channel_id) {
            return Ok((Vec::new(), None));
        }
        let secret = self.identity.unwrap(&public_key, generation, wrapped)?;
        let change = self.learn_peer(channel_id, user_id, public_key);
        let peer = self.peers.get_mut(&user_id).expect("just learned");
        peer.keys.insert(SenderKey::derive(generation, &secret));
        // A peer that keyed us before we keyed them (both joined at once) still needs ours.
        let out = if change.is_none() && peer.sent_generation.is_none() {
            self.wrap_for(&user_id)?.into_iter().collect()
        } else {
            Vec::new()
        };
        Ok((out, change))
    }

    fn wrap_for(&mut self, user_id: &UserId) -> Result<Option<Outgoing>> {
        let generation = self.key.generation;
        let secret = self.secret;
        let Some(peer) = self.peers.get_mut(user_id) else {
            return Ok(None);
        };
        let Some(channel_id) = peer.channels.iter().next().copied() else {
            return Ok(None);
        };
        let wrapped = self.identity.wrap(&peer.public_key, generation, &secret)?;
        peer.sent_generation = Some(generation);
        Ok(Some(Outgoing::SenderKey {
            channel_id,
            to: *user_id,
            generation,
            wrapped,
        }))
    }

    /// Picks a fresh sender key and wraps it for every peer. Callers debounce this (a join
    /// wave should cost one rotation, not one per newcomer); no-op unless
    /// [`Group::rotation_pending`] or `force`.
    pub fn rotate(&mut self, force: bool) -> Result<Vec<Outgoing>> {
        if !(force || self.rotation_pending) {
            return Ok(Vec::new());
        }
        self.rotation_pending = false;
        rand::thread_rng().fill_bytes(&mut self.secret);
        self.key = SenderKey::derive(self.key.generation.wrapping_add(1), &self.secret);
        self.counter = 0;
        let peers: Vec<UserId> = self.peers.keys().copied().collect();
        let mut out = Vec::with_capacity(peers.len());
        for user in peers {
            if let Some(msg) = self.wrap_for(&user)? {
                out.push(msg);
            }
        }
        Ok(out)
    }

    /// Encrypts one of our frames.
    pub fn encrypt(&mut self, plain: &[u8]) -> Vec<u8> {
        let counter = self.counter;
        self.counter = self.counter.wrapping_add(1);
        if self.counter >= ROTATE_AT_COUNTER {
            self.rotation_pending = true;
        }
        self.key.seal(counter, plain)
    }

    /// Decrypts a frame sent by `user_id`.
    pub fn decrypt(&mut self, user_id: &UserId, frame: &[u8]) -> Result<Vec<u8>> {
        let peer = self
            .peers
            .get_mut(user_id)
            .ok_or_else(|| AurixError::Encryption("unknown E2EE sender".into()))?;
        peer.keys.open(frame)
    }
}

/// Decodes a base64 identity public key from the wire.
pub fn parse_public_key(b64: &str) -> Result<[u8; PUBLIC_KEY_LEN]> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| AurixError::Validation("public_key is not base64".into()))?;
    bytes
        .try_into()
        .map_err(|_| AurixError::Validation("public_key must be 32 bytes".into()))
}

pub fn encode_bytes(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn decode_bytes(b64: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| AurixError::Validation("not base64".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(n: u128) -> ChannelId {
        ChannelId(uuid::Uuid::from_u128(n))
    }

    fn user(n: u128) -> UserId {
        UserId(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn hkdf_matches_rfc5869_case_1() {
        let ikm = [0x0bu8; 22];
        let salt: Vec<u8> = (0x00..=0x0c).collect();
        let info: Vec<u8> = (0xf0..=0xf9).collect();
        let okm = hkdf_sha256(&salt, &ikm, &info, 42);
        assert_eq!(
            hex(&okm),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn frame_round_trip_and_tamper() {
        let key = SenderKey::derive(3, &[7u8; 32]);
        let frame = key.seal(42, b"opus frame");
        assert_eq!(frame.len(), 10 + FRAME_OVERHEAD);
        assert_eq!(SenderKey::peek(&frame), Some((3, 42)));
        assert_eq!(key.open(&frame).unwrap(), b"opus frame");
        let mut bad = frame.clone();
        bad[7] ^= 1;
        assert!(key.open(&bad).is_err());
        let mut wrong_gen = frame.clone();
        wrong_gen[0] = 4;
        assert!(key.open(&wrong_gen).is_err());
        assert!(key.open(&frame[..FRAME_OVERHEAD - 1]).is_err());
        let empty = key.seal(0, b"");
        assert_eq!(key.open(&empty).unwrap(), b"");
    }

    /// Fixed vector shared with the Web (`e2ee.test.mjs`) and .NET (`E2eeTests.cs`) suites;
    /// independently reproduced with Python `hmac`/`cryptography`.
    #[test]
    fn frame_test_vector() {
        let secret: [u8; 32] = core::array::from_fn(|i| i as u8);
        let key = SenderKey::derive(1, &secret);
        assert_eq!(
            hex(&key.enc),
            "495d7612bbcfa75aada371e8facda163a223fac6a9469018e159a3d825ead1c6"
        );
        assert_eq!(
            hex(&key.auth),
            "386f83d423b3d5a81e15c2c09e21d9588c2768da88c96bef6f1f5633be1ff64e"
        );
        assert_eq!(hex(&key.salt), "6ca0258f60de84e095dc2066");
        let frame = key.seal(0x01020304, &[0xf8, 0xff, 0xfe, 0x00, 0x01]);
        assert_eq!(hex(&frame), "0101020304eb1575b9e6b52709cbdab928367122");
        assert_eq!(key.open(&frame).unwrap(), &[0xf8, 0xff, 0xfe, 0x00, 0x01]);
    }

    /// Fixed wrap vector shared with the Web and .NET suites (identities `[1u8; 32]` / `[2u8; 32]`).
    #[test]
    fn wrap_test_vector() {
        let alice = IdentityKey::from_bytes([1u8; 32]);
        let bob = IdentityKey::from_bytes([2u8; 32]);
        assert_eq!(
            hex(alice.public_key()),
            "a4e09292b651c278b9772c569f5fa9bb13d906b46ab68c9df9dc2b4409f8a209"
        );
        assert_eq!(
            hex(bob.public_key()),
            "ce8d3ad1ccb633ec7b70c17814a5c76ecd029685050d344745ba05870e587d59"
        );
        assert_eq!(
            alice.fingerprint(),
            "1a92f23852dc908d97316a3b13578281196c1dd73d9ae5e313f3fb6b8954bf55"
        );
        let (enc, auth) = alice
            .wrap_keys(bob.public_key(), alice.public_key(), bob.public_key())
            .unwrap();
        assert_eq!(
            hex(&enc),
            "d769905c2b8b1019b7e9da448b11c6b59106e1f9858efe13763ff2e3a3ab5507"
        );
        assert_eq!(
            hex(&auth),
            "e56126305aeea09d0929942837301ccdca1c74954283a0895accb5a55e6253fc"
        );
        // Nonce 0x42*12, generation 5, secret [9u8; 32].
        let wrapped = hex_decode(
            "42424242424242424242424249d1a4e521c229f2380da9fe507d631f090452c1bbc07032dcda4bba6be02de573dc00012e371ae34d5fbd125518632b",
        );
        assert_eq!(
            bob.unwrap(alice.public_key(), 5, &wrapped).unwrap(),
            [9u8; 32]
        );
    }

    #[test]
    fn wrap_round_trip_direction_and_tamper() {
        let alice = IdentityKey::from_bytes([1u8; 32]);
        let bob = IdentityKey::from_bytes([2u8; 32]);
        let carol = IdentityKey::from_bytes([3u8; 32]);
        let secret = [9u8; 32];
        let wrapped = alice.wrap(bob.public_key(), 5, &secret).unwrap();
        assert_eq!(wrapped.len(), WRAPPED_KEY_LEN);
        assert_eq!(bob.unwrap(alice.public_key(), 5, &wrapped).unwrap(), secret);
        // Wrong generation, wrong recipient, wrong claimed sender, tampered body.
        assert!(bob.unwrap(alice.public_key(), 6, &wrapped).is_err());
        assert!(carol.unwrap(alice.public_key(), 5, &wrapped).is_err());
        assert!(bob.unwrap(carol.public_key(), 5, &wrapped).is_err());
        let mut bad = wrapped.clone();
        bad[20] ^= 0x80;
        assert!(bob.unwrap(alice.public_key(), 5, &bad).is_err());
        assert!(bob
            .unwrap(alice.public_key(), 5, &wrapped[..WRAPPED_KEY_LEN - 1])
            .is_err());
        // Low-order point is refused.
        assert!(alice.wrap(&[0u8; 32], 0, &secret).is_err());
    }

    #[test]
    fn replay_window() {
        let mut r = ReplayState::new();
        assert!(r.accept(10));
        assert!(!r.accept(10));
        assert!(r.accept(8));
        assert!(!r.accept(8));
        assert!(r.accept(500));
        assert!(!r.accept(10), "too old");
        assert!(r.accept(499));
        assert!(!r.accept(499));
    }

    fn drive(groups: &mut HashMap<UserId, Group>, from: UserId, msgs: Vec<Outgoing>) {
        let pk = *groups[&from].identity().public_key();
        for m in msgs {
            match m {
                Outgoing::Hello { channel_id } => {
                    let others: Vec<UserId> =
                        groups.keys().filter(|u| **u != from).copied().collect();
                    for u in others {
                        let (out, _) = groups
                            .get_mut(&u)
                            .unwrap()
                            .on_hello(channel_id, from, pk)
                            .unwrap();
                        drive(groups, u, out);
                    }
                }
                Outgoing::SenderKey {
                    channel_id,
                    to,
                    generation,
                    wrapped,
                } => {
                    let (out, _) = groups
                        .get_mut(&to)
                        .unwrap()
                        .on_sender_key(channel_id, from, pk, generation, &wrapped)
                        .unwrap();
                    drive(groups, to, out);
                }
            }
        }
    }

    fn settle(groups: &mut HashMap<UserId, Group>) {
        for _ in 0..4 {
            let users: Vec<UserId> = groups.keys().copied().collect();
            for u in users {
                let out = groups.get_mut(&u).unwrap().rotate(false).unwrap();
                drive(groups, u, out);
            }
        }
    }

    #[test]
    fn group_join_rotate_leave() {
        let a = user(1);
        let b = user(2);
        let c = user(3);
        let channel = ch(100);
        let mut groups: HashMap<UserId, Group> = HashMap::new();
        for u in [a, b] {
            groups.insert(u, Group::new(IdentityKey::generate()));
        }
        for u in [a, b] {
            let out = groups.get_mut(&u).unwrap().joined(channel);
            drive(&mut groups, u, out);
        }
        settle(&mut groups);
        // Both rotated once for the newcomer and can decrypt each other.
        assert_eq!(groups[&a].generation(), 1);
        assert_eq!(groups[&b].generation(), 1);
        let frame = groups.get_mut(&a).unwrap().encrypt(b"hi bob");
        assert_eq!(
            groups.get_mut(&b).unwrap().decrypt(&a, &frame).unwrap(),
            b"hi bob"
        );
        assert!(
            groups.get_mut(&b).unwrap().decrypt(&a, &frame).is_err(),
            "replay"
        );

        // Carol joins: everyone rotates, Carol cannot read the pre-join frame.
        let old = groups.get_mut(&a).unwrap().encrypt(b"before carol");
        groups.insert(c, Group::new(IdentityKey::generate()));
        let out = groups.get_mut(&c).unwrap().joined(channel);
        drive(&mut groups, c, out);
        settle(&mut groups);
        assert_eq!(groups[&a].generation(), 2);
        assert!(groups.get_mut(&c).unwrap().decrypt(&a, &old).is_err());
        let fresh = groups.get_mut(&a).unwrap().encrypt(b"after carol");
        assert_eq!(
            groups.get_mut(&c).unwrap().decrypt(&a, &fresh).unwrap(),
            b"after carol"
        );
        assert_eq!(
            groups.get_mut(&b).unwrap().decrypt(&a, &fresh).unwrap(),
            b"after carol"
        );
        let from_c = groups.get_mut(&c).unwrap().encrypt(b"carol here");
        assert_eq!(
            groups.get_mut(&a).unwrap().decrypt(&c, &from_c).unwrap(),
            b"carol here"
        );

        // Bob leaves: Alice and Carol rotate; Bob's stale key no longer opens new frames.
        let mut bob = groups.remove(&b).unwrap();
        for u in [a, c] {
            groups.get_mut(&u).unwrap().peer_left(&channel, &b);
        }
        settle(&mut groups);
        assert_eq!(groups[&a].generation(), 3);
        let after = groups.get_mut(&a).unwrap().encrypt(b"without bob");
        assert!(bob.decrypt(&a, &after).is_err());
        assert_eq!(
            groups.get_mut(&c).unwrap().decrypt(&a, &after).unwrap(),
            b"without bob"
        );
        assert!(groups[&a].peer_fingerprint(&b).is_none());
        assert_eq!(
            groups[&a].peer_fingerprint(&c),
            Some(groups[&c].identity().fingerprint())
        );
    }

    #[test]
    fn frames_in_flight_across_rotation_still_decrypt() {
        let a = user(1);
        let b = user(2);
        let channel = ch(1);
        let mut groups: HashMap<UserId, Group> = HashMap::new();
        for u in [a, b] {
            groups.insert(u, Group::new(IdentityKey::generate()));
            let out = groups.get_mut(&u).unwrap().joined(channel);
            drive(&mut groups, u, out);
        }
        settle(&mut groups);
        let old = groups.get_mut(&a).unwrap().encrypt(b"old gen");
        let out = groups.get_mut(&a).unwrap().rotate(true).unwrap();
        drive(&mut groups, a, out);
        let new = groups.get_mut(&a).unwrap().encrypt(b"new gen");
        let bob = groups.get_mut(&b).unwrap();
        assert_eq!(bob.decrypt(&a, &new).unwrap(), b"new gen");
        assert_eq!(bob.decrypt(&a, &old).unwrap(), b"old gen");
    }

    #[test]
    fn frame_ahead_of_its_key_is_awaited_not_rejected() {
        let a = user(1);
        let b = user(2);
        let channel = ch(1);
        let mut groups: HashMap<UserId, Group> = HashMap::new();
        for u in [a, b] {
            groups.insert(u, Group::new(IdentityKey::generate()));
            let out = groups.get_mut(&u).unwrap().joined(channel);
            drive(&mut groups, u, out);
        }
        settle(&mut groups);
        // Alice rotates and seals before her new key reaches Bob (media overtook control).
        let out = groups.get_mut(&a).unwrap().rotate(true).unwrap();
        let early = groups.get_mut(&a).unwrap().encrypt(b"early");
        let (generation, _) = SenderKey::peek(&early).unwrap();
        let bob = groups.get_mut(&b).unwrap();
        assert!(bob.decrypt(&a, &early).is_err());
        assert!(bob.awaiting_generation(&a, generation));
        assert!(!bob.awaiting_generation(&a, generation.wrapping_sub(1)));
        assert!(!bob.awaiting_generation(&user(9), generation));
        drive(&mut groups, a, out);
        let bob = groups.get_mut(&b).unwrap();
        assert!(!bob.awaiting_generation(&a, generation));
        assert_eq!(bob.decrypt(&a, &early).unwrap(), b"early");
    }

    #[test]
    fn hello_outside_our_channels_is_ignored_and_rekey_is_flagged() {
        let a = user(1);
        let b = user(2);
        let mut ga = Group::new(IdentityKey::generate());
        let bob1 = IdentityKey::generate();
        let bob2 = IdentityKey::generate();
        let (out, change) = ga.on_hello(ch(9), b, *bob1.public_key()).unwrap();
        assert!(out.is_empty() && change.is_none());
        ga.joined(ch(9));
        let (_, change) = ga.on_hello(ch(9), b, *bob1.public_key()).unwrap();
        assert_eq!(change, Some(PeerChange::New));
        ga.rotate(false).unwrap();
        let (out, change) = ga.on_hello(ch(9), b, *bob1.public_key()).unwrap();
        assert!(change.is_none());
        assert!(matches!(out.as_slice(), [Outgoing::SenderKey { to, .. }] if *to == b));
        let (_, change) = ga.on_hello(ch(9), b, *bob2.public_key()).unwrap();
        assert_eq!(
            change,
            Some(PeerChange::KeyChanged {
                previous_fingerprint: bob1.fingerprint()
            })
        );
        assert!(ga.rotation_pending());
        assert_eq!(ga.peer_fingerprint(&b), Some(bob2.fingerprint()));
        assert!(!ga.has_key_for(&b));
        let _ = a;
    }

    #[test]
    fn resume_forgets_departed_peers_and_rehellos() {
        let a = user(1);
        let b = user(2);
        let c = user(3);
        let mut ga = Group::new(IdentityKey::generate());
        let bob = IdentityKey::generate();
        let carol = IdentityKey::generate();
        ga.joined(ch(1));
        ga.joined(ch(2));
        ga.on_hello(ch(1), b, *bob.public_key()).unwrap();
        ga.on_hello(ch(2), b, *bob.public_key()).unwrap();
        ga.on_hello(ch(1), c, *carol.public_key()).unwrap();
        ga.rotate(false).unwrap();
        // Resume replays channel 1 with only Bob left in it: Carol is forgotten (rotation),
        // Bob stays known through channel 2, and we re-announce ourselves.
        let (out, gone) = ga.rejoined(ch(1), &HashSet::from([a, b]));
        assert_eq!(out, vec![Outgoing::Hello { channel_id: ch(1) }]);
        assert_eq!(gone, vec![c]);
        assert!(ga.rotation_pending());
        assert!(ga.peer_fingerprint(&b).is_some());
        assert!(ga.peer_fingerprint(&c).is_none());
        // Same for a channel we did not know about: behaves like a first join.
        let (out, gone) = ga.rejoined(ch(3), &HashSet::new());
        assert_eq!(out, vec![Outgoing::Hello { channel_id: ch(3) }]);
        assert!(gone.is_empty());
    }

    #[test]
    fn leaving_last_shared_channel_drops_peer() {
        let a = user(1);
        let b = user(2);
        let mut ga = Group::new(IdentityKey::generate());
        let bob = IdentityKey::generate();
        ga.joined(ch(1));
        ga.joined(ch(2));
        ga.on_hello(ch(1), b, *bob.public_key()).unwrap();
        ga.on_hello(ch(2), b, *bob.public_key()).unwrap();
        ga.rotate(false).unwrap();
        ga.peer_left(&ch(1), &b);
        assert!(!ga.rotation_pending());
        assert!(ga.peer_fingerprint(&b).is_some());
        ga.left(&ch(2));
        assert!(ga.rotation_pending());
        assert!(ga.peer_fingerprint(&b).is_none());
        assert!(ga.active());
        ga.left(&ch(1));
        assert!(!ga.active());
        let _ = a;
    }
}
