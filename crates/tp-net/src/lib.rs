pub mod auth;
pub mod client;
pub mod identity;
pub mod pairing;
pub mod peer;
pub mod probe;
pub mod ratelimit;
pub mod wire;

pub mod tls;

pub use auth::{
    sign_request, verify_request, ChallengeStore, SignedHeaders, VerifiedRequest, SKEW_SECS,
};
pub use client::{ping, send_pair_request};
pub use ed25519_dalek::VerifyingKey;
pub use identity::{fingerprint, Identity};
pub use pairing::{
    approve, name_is_displayable, record_incoming, reject, request_out, revoke, Incoming,
    PairingResult, PairingStatus, MAX_NAME_CHARS, MAX_PENDING_IN,
};
pub use peer::{merge_hits, query_peers, FanOutResult, PeerAddr, PeerHit, PeerQuery};
pub use probe::{probe, serve_port, serve_port_with, DiscoveredPeer, DEFAULT_PORT, PROBE_PORTS};
pub use wire::{hex_decode_pub, hex_encode, PairRequest, PingResponse};
