//! Checks that the crypto matches pyremoteplay byte for byte.
//! Expected values were produced by feeding the same inputs to pyremoteplay 0.7.6.

use crate::gkcrypt::{StreamCipher, counter_add};
use crate::rpcrypt::{SessionCipher, aes_key, rp_nonce};

const RP_NONCE: &str = "6b4f07f794c0e77dd30fdcc404215295";
const AES_KEY: &str = "a316e20155e96d2ca9ce82d069ca28e0";
const ENC0: &str = "db6a1aec8c73019a395ab53e380ba2d140abd7";
const ENC1: &str = "979594";
const X5: &str = "3c69f67ada25905c581bd81a806ede8ffd1eb6662fdb0bea8ce130a1c410c4d1541749d0047a8b94";
const X100: &str = "374a5c5a2c3392";
const G0: &str = "a4d8bdfa";
const G1: &str = "9e24807e";
const R33: &str = "727d907f0c7dfdae8d54";
const CA: &str = "fe010002000000000000000000000000";

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn seq(start: u8) -> [u8; 16] {
    std::array::from_fn(|i| start + i as u8)
}

#[test]
fn session_cipher_matches() {
    let nonce = seq(16);
    let rp_key = seq(100);
    let rn = rp_nonce(&nonce);
    let ak = aes_key(&nonce, &rp_key);
    assert_eq!(hex(&rn), RP_NONCE);
    assert_eq!(hex(&ak), AES_KEY);
    let mut c = SessionCipher::new(ak, rn);
    assert_eq!(hex(&c.encrypt(b"hello world 1234567")), ENC0);
    assert_eq!(hex(&c.encrypt(b"abc")), ENC1);
}

#[test]
fn stream_cipher_matches() {
    let secret: Vec<u8> = (200..232).collect();
    let hk = seq(50);
    let mut c = StreamCipher::new(&secret, &hk);

    let mut b = vec![0u8; 40];
    c.local.xor(5, &mut b);
    assert_eq!(hex(&b), X5);
    let mut b = vec![0u8; 7];
    c.local.xor(100, &mut b);
    assert_eq!(hex(&b), X100);

    let data = b"some packet bytes here";
    assert_eq!(hex(&c.local.gmac(data, 0)), G0);
    assert_eq!(hex(&c.local.gmac(data, 50000)), G1); // after one GMAC key refresh

    let mut b = vec![0u8; 10];
    c.remote.xor(33, &mut b);
    assert_eq!(hex(&b), R33);
}

#[test]
fn counter_add_matches() {
    let mut iv = [0u8; 16];
    iv[..4].copy_from_slice(&[0xff, 0xff, 0xff, 0x01]);
    assert_eq!(hex(&counter_add(0x1ff, &iv)), CA);
}
