//! Encryption for the stream (Takion) stage.
//!
//! - Key exchange: secp256k1 ECDH. Public keys are exchanged with an HMAC signature keyed by handshake_key.
//! - Encryption: an AES-128 CTR-like key stream. Each packet uses it from the key_pos in its header.
//! - Authentication: the first 4 bytes of an AES-GCM tag (GMAC). The GMAC key is refreshed every 45000 bytes.
//!
//! The sending (local) and receiving (remote) directions are derived from the same secret with different
//! indices (2 and 3).

use aes::Aes128;
use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use aes_gcm::{AesGcm, aead::AeadInPlace, aes::cipher::consts::U16};
use hmac::{Hmac, Mac};
use k256::PublicKey;
use k256::ecdh::EphemeralSecret;
use k256::elliptic_curve::sec1::ToEncodedPoint;
use sha2::{Digest, Sha256};

const GMAC_REFRESH_IV: u64 = 44910;
const GMAC_REFRESH_KEY_POS: u64 = 45000;

type Gcm16 = AesGcm<Aes128, U16>;

/// Treats the IV as a little-endian 128-bit integer and adds counter.
pub fn counter_add(counter: u64, iv: &[u8; 16]) -> [u8; 16] {
    u128::from_le_bytes(*iv).wrapping_add(counter as u128).to_le_bytes()
}

fn base_key_iv(secret: &[u8], handshake_key: &[u8; 16], index: u8) -> ([u8; 16], [u8; 16]) {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret).expect("HMAC key");
    mac.update(&[0x01, index, 0x00]);
    mac.update(handshake_key);
    mac.update(&[0x01, 0x00]);
    let h = mac.finalize().into_bytes();
    (h[..16].try_into().unwrap(), h[16..].try_into().unwrap())
}

fn gmac_key(index: u64, key: &[u8; 16], iv: &[u8; 16]) -> [u8; 16] {
    let mut sha = Sha256::new();
    sha.update(key);
    sha.update(counter_add(index * GMAC_REFRESH_IV, iv));
    let h = sha.finalize();
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = h[i] ^ h[16 + i];
    }
    out
}

/// Cipher for one direction (send or receive).
pub struct DirCipher {
    base_key: [u8; 16],
    base_iv: [u8; 16],
    base_gmac_key: [u8; 16],
    current_gmac_key: [u8; 16],
    gmac_index: u64,
    aes: Aes128,
}

impl DirCipher {
    fn new(secret: &[u8], handshake_key: &[u8; 16], index: u8) -> Self {
        let (base_key, base_iv) = base_key_iv(secret, handshake_key, index);
        let base_gmac_key = gmac_key(0, &base_key, &base_iv);
        Self {
            base_key,
            base_iv,
            base_gmac_key,
            current_gmac_key: base_gmac_key,
            gmac_index: 0,
            aes: Aes128::new(&base_key.into()),
        }
    }

    /// XORs with the key stream at key_pos (same operation for encrypt and decrypt).
    pub fn xor(&self, key_pos: u64, data: &mut [u8]) {
        let pad = (key_pos % 16) as usize;
        let first_block = key_pos / 16 + 1; // the stream starts at the next block
        let blocks = (pad + data.len()).div_ceil(16);
        let mut stream = Vec::with_capacity(blocks * 16);
        for b in 0..blocks as u64 {
            let mut block = GenericArray::from(counter_add(first_block + b, &self.base_iv));
            self.aes.encrypt_block(&mut block);
            stream.extend_from_slice(&block);
        }
        for (d, k) in data.iter_mut().zip(&stream[pad..]) {
            *d ^= k;
        }
    }

    /// 4-byte tag over the whole packet (with the gmac field zeroed).
    pub fn gmac(&mut self, data: &[u8], key_pos: u64) -> [u8; 4] {
        let iv = counter_add(key_pos / 16, &self.base_iv);
        let index = if key_pos > 0 { (key_pos - 1) / GMAC_REFRESH_KEY_POS } else { 0 };
        let key = if index > self.gmac_index {
            self.gmac_index = index;
            // like pyremoteplay, new keys are derived from base_gmac_key
            self.current_gmac_key = gmac_key(index, &self.base_gmac_key, &self.base_iv);
            self.current_gmac_key
        } else if index < self.gmac_index {
            gmac_key(index, &self.base_key, &self.base_iv)
        } else {
            self.current_gmac_key
        };
        let gcm = Gcm16::new(&key.into());
        let tag = gcm.encrypt_in_place_detached(GenericArray::from_slice(&iv), data, &mut []).expect("GMAC");
        tag[..4].try_into().unwrap()
    }
}

/// Pair of send (local) and receive (remote) ciphers. Also tracks the send-side key_pos.
pub struct StreamCipher {
    pub local: DirCipher,
    pub remote: DirCipher,
    pub key_pos: u64,
}

impl StreamCipher {
    pub fn new(secret: &[u8], handshake_key: &[u8; 16]) -> Self {
        Self {
            local: DirCipher::new(secret, handshake_key, 2),
            remote: DirCipher::new(secret, handshake_key, 3),
            key_pos: 0,
        }
    }
}

/// ECDH key exchange state.
pub struct Ecdh {
    pub handshake_key: [u8; 16],
    secret: EphemeralSecret,
    pub public_key: Vec<u8>,
    pub public_sig: Vec<u8>,
}

fn sig(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC key");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

impl Ecdh {
    pub fn new() -> Self {
        let handshake_key: [u8; 16] = rand::random();
        let secret = EphemeralSecret::random(&mut rand::rngs::OsRng);
        let public_key = secret.public_key().to_encoded_point(false).as_bytes().to_vec();
        let public_sig = sig(&handshake_key, &public_key);
        Self { handshake_key, secret, public_key, public_sig }
    }

    /// Verifies the console public key signature and derives the shared secret (32-byte x coordinate).
    pub fn shared_secret(&self, remote_key: &[u8], remote_sig: &[u8]) -> crate::Result<Vec<u8>> {
        if sig(&self.handshake_key, remote_key) != remote_sig {
            return Err(crate::error::protocol("console public key signature mismatch"));
        }
        let remote = PublicKey::from_sec1_bytes(remote_key)
            .map_err(|_| crate::error::protocol("unexpected console public key format"))?;
        Ok(self.secret.diffie_hellman(&remote).raw_secret_bytes().to_vec())
    }
}

impl Default for Ecdh {
    fn default() -> Self {
        Self::new()
    }
}
