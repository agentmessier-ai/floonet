//! The peer wire — types and encodings both sides of the protocol name.
//!
//! These belong with the protocol rather than with either end: `client.rs`
//! parses a `PingResponse` it did not produce and builds a `PairRequest` it
//! will not handle.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResponse {
    pub device_id: String,
    pub name: String,
    pub version: String,
    /// Hex ed25519 public key. The initiator needs this to record who it is
    /// pairing with; it is public by definition, and the caller must check it
    /// hashes to `device_id` before storing either.
    pub pubkey: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRequest {
    pub device_id: String,
    pub name: String,
    pub pubkey: String, // hex
    /// The port the requester serves on. The TCP source port is ephemeral and
    /// useless for calling back, so the peer states its listen port while the
    /// IP is taken from the observed connection (which it cannot forge on an
    /// established TCP session).
    pub port: u16,
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parse untrusted hex. Must not panic: its input is the `pubkey` field of
/// `/v1/pair/request`, which is remote-controlled, so odd lengths and
/// non-ASCII input are rejected rather than sliced.
pub fn hex_decode_pub(s: &str) -> Option<Vec<u8>> {
    hex_decode(s)
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.is_ascii() || !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .chunks_exact(2)
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some(((hi << 4) | lo) as u8)
        })
        .collect()
}
