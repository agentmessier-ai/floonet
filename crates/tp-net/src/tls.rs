//! Encrypt-only TLS for the peer channel.
//!
//! Its only job is to stop a passive listener reading traffic in flight. Peer
//! authenticity comes entirely from the ed25519 signature on each request,
//! checked against the trust store (`auth::verify_request`). The certificate
//! is a fresh self-signed one with no CA and no pinning: pinning the same
//! identity a second time at the TLS layer would add a second key per peer
//! and a second trust store without changing what an attacker can do.

use anyhow::{Context, Result};
use std::sync::Once;

/// rustls needs one process-wide default `CryptoProvider`. It is ring: rcgen
/// needs ring for key generation anyway, so this keeps the build on one
/// crypto stack. Idempotent, so every `serve()` may call it.
fn ensure_crypto_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// A fresh self-signed cert + key, PEM-encoded. Generated once per process;
/// nothing pins to it, so it need not be stable across restarts.
pub fn self_signed_pem() -> Result<(Vec<u8>, Vec<u8>)> {
    ensure_crypto_provider();
    let cert = rcgen::generate_simple_self_signed(vec!["floonet-peer".to_string()])
        .context("generate self-signed TLS cert")?;
    let cert_pem = cert.cert.pem().into_bytes();
    let key_pem = cert.key_pair.serialize_pem().into_bytes();
    Ok((cert_pem, key_pem))
}
