//! rotten-apple fabric — mesh primitives.
//!
//! Phase 1 of the rotten-apple Xen mesh. This crate is intentionally
//! transport-agnostic: it builds the cryptographic + identity layer
//! (NodeId, KeyPair, signed Envelope, hash-chained EventLog) that
//! sits ABOVE whatever wire is used (LAN+mDNS, Tailscale, WireGuard,
//! manual peers). Transport impls live behind the [`MeshTransport`]
//! trait — see `transport.rs`.
//!
//! Why a hash chain on top of signatures? A bare-signed event tells
//! you "the holder of issuer's privkey said this," but doesn't pin
//! ordering. A monotonic sequence catches reordering but not silent
//! deletion of intermediate events. A hash chain (each Envelope
//! includes the SHA-256 of the previous Envelope's signed bytes)
//! makes any gap visible: tamper with one event and every downstream
//! event's `prev_hash` becomes unverifiable. Verification cost is
//! O(N) on append, O(1) per-event on receive given a known head.
//!
//! Threat model:
//! - Honest issuers (controllers) never sign two events with the
//!   same `(issuer, seq)`. Replay protection is per-issuer.
//! - The transport may be hostile; the chain assumes only that the
//!   agent's local store is intact.
//! - Privkey compromise IS catastrophic — the attacker can append
//!   valid events. Defense-in-depth (manifest `lease_policy` with
//!   `controllers_allowed` + `operations_allowed`) reduces blast
//!   radius but doesn't substitute for key hygiene.

pub mod chain;
pub mod config;
pub mod event;
pub mod keypair;
pub mod node_id;
pub mod transport;

pub use chain::{ChainError, EventLog};
pub use config::{MeshConfig, PeerEntry, TransportKind, TrustMode};
pub use event::{Envelope, Event, EventBody, EventHash, LeaseEpoch, Sequence};
pub use keypair::{KeyPair, PublicKey, SECRET_KEY_BYTES, PUBLIC_KEY_BYTES, SIGNATURE_BYTES};
pub use node_id::NodeId;
pub use transport::{MeshTransport, Peer};
