//! Encryption: the session stage (AES-CFB, `rpcrypt`), the stream stage (ECDH, key stream and GMAC,
//! `gkcrypt`) and the key tables from the official app (`keys`).

pub(crate) mod gkcrypt;
pub(crate) mod keys;
pub(crate) mod rpcrypt;

#[cfg(test)]
mod tests;
