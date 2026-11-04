//! Encryption for the session stage (HTTP and control TCP): AES-128-CFB.
//!
//! The key is derived from the console's nonce and the regist RP-Key. Each message bumps a counter and
//! gets a fresh CFB cipher with IV = HMAC-SHA256(HMAC_KEY, nonce || counter)[..16].

use aes::cipher::{AsyncStreamCipher, KeyIvInit};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::keys::{HMAC_KEY, SESSION_KEY_0, SESSION_KEY_1};

type Cfb = cfb_mode::Encryptor<aes::Aes128>;
type CfbDec = cfb_mode::Decryptor<aes::Aes128>;

/// Nonce from the session init response → rp_nonce used for encryption (PS5 variant).
pub fn rp_nonce(nonce: &[u8; 16]) -> [u8; 16] {
    let key = &SESSION_KEY_0[(nonce[0] as usize >> 3) * 112..];
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = nonce[i].wrapping_sub(45).wrapping_sub(i as u8) ^ key[i];
    }
    out
}

/// Nonce from the session init response + RP-Key → AES key (PS5 variant).
pub fn aes_key(nonce: &[u8; 16], rp_key: &[u8; 16]) -> [u8; 16] {
    let key = &SESSION_KEY_1[(nonce[7] as usize >> 3) * 112..];
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = rp_key[i].wrapping_add(24).wrapping_add(i as u8) ^ nonce[i] ^ key[i];
    }
    out
}

pub struct SessionCipher {
    key: [u8; 16],
    nonce: [u8; 16],
    enc_counter: u64,
    dec_counter: u64,
}

impl SessionCipher {
    pub fn new(key: [u8; 16], nonce: [u8; 16]) -> Self {
        Self { key, nonce, enc_counter: 0, dec_counter: 0 }
    }

    fn iv(&self, counter: u64) -> [u8; 16] {
        let mut mac = Hmac::<Sha256>::new_from_slice(&HMAC_KEY).expect("HMAC key");
        mac.update(&self.nonce);
        mac.update(&counter.to_be_bytes());
        let tag = mac.finalize().into_bytes();
        tag[..16].try_into().unwrap()
    }

    /// Encrypts with the given counter (does not advance the counter).
    pub fn encrypt_at(&self, counter: u64, data: &[u8]) -> Vec<u8> {
        let mut buf = data.to_vec();
        Cfb::new(&self.key.into(), &self.iv(counter).into()).encrypt(&mut buf);
        buf
    }

    /// Encrypts with the next counter.
    pub fn encrypt(&mut self, data: &[u8]) -> Vec<u8> {
        let out = self.encrypt_at(self.enc_counter, data);
        self.enc_counter += 1;
        out
    }

    /// Only advances the send counter (pyremoteplay sends control messages in plain text but still
    /// advances the counter).
    pub fn skip_encrypt(&mut self) {
        self.enc_counter += 1;
    }

    pub fn decrypt(&mut self, data: &[u8]) -> Vec<u8> {
        let mut buf = data.to_vec();
        CfbDec::new(&self.key.into(), &self.iv(self.dec_counter).into()).decrypt(&mut buf);
        self.dec_counter += 1;
        buf
    }
}
