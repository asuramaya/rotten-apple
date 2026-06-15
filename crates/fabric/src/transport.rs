//! Pluggable mesh transport abstraction.
//!
//! The trait surface is deliberately small — discovery + dial + listen
//! — because the chain layer above doesn't care HOW envelopes get
//! across the wire, just that they arrive intact. Concrete impls
//! (LAN+mDNS, manual peer list, Tailscale, WireGuard) live behind
//! this trait so the agent + controller code never bake in a
//! particular wire vendor.
//!
//! Phase 1 ships:
//!   - the trait
//!   - a [`Peer`] descriptor type
//!   - a `manual` impl that reads from `/etc/rotten-apple/mesh.toml`
//!
//! Phase 1.5 (next): a `lan` impl using mDNS for discovery + plain TCP
//! for transport. Tailscale-as-router-upstream needs no special impl —
//! it just looks like LAN once the router announces the route, and
//! peers are reachable at their tailnet IPs (which can also be
//! manually listed).

use crate::keypair::PublicKey;
use crate::node_id::NodeId;
use std::io;
use std::net::SocketAddr;

/// What we know about a discovered peer. Identity (NodeId + pubkey) is
/// authoritative; the address may rotate (tailnet IPs after roaming,
/// LAN DHCP renewals, etc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    /// Stable identity of the peer.
    pub node_id: NodeId,
    /// Long-lived signing pubkey (from `[[peers]]` config or learned
    /// via TOFU on first contact).
    pub pubkey: Option<PublicKey>,
    /// Where to reach the peer right now. May be empty if discovery
    /// found the node but no transport-level address is available.
    pub addrs: Vec<SocketAddr>,
    /// Free-form transport-specific tag — `"tailscale"`, `"lan"`,
    /// `"manual"`, etc. For diagnostics + UX (cockpit shows "via lan").
    pub via: String,
}

/// Pluggable transport contract. Keep this trait small — every method
/// here is a plumbing burden multiplied across N transport impls.
pub trait MeshTransport: Send + Sync {
    /// This node's stable id, as known by THIS transport. Some
    /// transports auto-derive from their own identity (Tailscale's
    /// machine name); LAN/manual fall through to [`NodeId::derive_for_this_host`].
    fn local_node_id(&self) -> &NodeId;

    /// Currently reachable peers, deduped by node_id. Returns empty
    /// vec rather than erroring on transient discovery hiccups —
    /// callers loop, they don't crash.
    fn list_peers(&self) -> Vec<Peer>;

    /// Open a connection to a peer. The returned address is a hint
    /// only; impls may try multiple addresses internally before
    /// returning. Connection-level encryption (TLS, Noise, etc.) is
    /// the impl's choice; the chain layer authenticates events
    /// independently via signatures, so even plaintext TCP is
    /// acceptable for low-stakes deployments.
    fn dial(&self, peer: &NodeId) -> io::Result<Box<dyn MeshStream>>;

    /// Bind a listener for inbound peer connections. Each accept
    /// yields a stream + the dialing peer's NodeId (which the
    /// transport learned during handshake) for routing.
    fn listen(&self) -> io::Result<Box<dyn MeshListener>>;

    /// Tag for logs / cockpit ("lan" | "tailscale" | "manual" | ...).
    fn name(&self) -> &str;
}

/// A bidirectional stream to a peer — the wire over which envelopes
/// travel. Uses io::Read + io::Write so we can layer JSON-Lines
/// framing on top in the agent without coupling to a specific
/// transport's stream type.
pub trait MeshStream: io::Read + io::Write + Send {
    /// Authenticated peer on the other end. None if the transport
    /// doesn't authenticate at the connection layer (rare; TOFU/LAN).
    fn peer(&self) -> Option<&NodeId>;
}

/// Listener for inbound peer connections. Modeled on UnixListener:
/// blocking accept, returns a stream + identifying metadata.
pub trait MeshListener: Send {
    fn accept(&self) -> io::Result<Box<dyn MeshStream>>;
}

// ---------------------------------------------------------------------------
// Tests — exercise the trait shape with a tiny in-memory impl so
// downstream callers (agent, controller) can unit-test without a real
// transport. The real LAN/Tailscale impls land later.

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// In-memory transport for tests: no real I/O, just a list of
    /// peers and a noop dial that returns a [`MemStream`]. Not used
    /// in production.
    struct MemTransport {
        local: NodeId,
        peers: Vec<Peer>,
    }

    struct MemStream {
        buf: Arc<Mutex<Vec<u8>>>,
        peer: NodeId,
    }

    impl io::Read for MemStream {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            let mut g = self.buf.lock().unwrap();
            let n = std::cmp::min(b.len(), g.len());
            b[..n].copy_from_slice(&g[..n]);
            g.drain(..n);
            Ok(n)
        }
    }
    impl io::Write for MemStream {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.buf.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    impl MeshStream for MemStream {
        fn peer(&self) -> Option<&NodeId> { Some(&self.peer) }
    }

    impl MeshTransport for MemTransport {
        fn local_node_id(&self) -> &NodeId { &self.local }
        fn list_peers(&self) -> Vec<Peer> { self.peers.clone() }
        fn name(&self) -> &str { "mem" }
        fn dial(&self, peer: &NodeId) -> io::Result<Box<dyn MeshStream>> {
            if !self.peers.iter().any(|p| &p.node_id == peer) {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no such peer"));
            }
            Ok(Box::new(MemStream {
                buf: Arc::new(Mutex::new(Vec::new())),
                peer: peer.clone(),
            }))
        }
        fn listen(&self) -> io::Result<Box<dyn MeshListener>> {
            unimplemented!("test transport doesn't listen")
        }
    }

    #[test]
    fn trait_dials_a_listed_peer() {
        let local = NodeId::new("test-node-deadbeefcafebabe");
        let target = NodeId::new("peer-a-1111111111111111");
        let t = MemTransport {
            local: local.clone(),
            peers: vec![Peer {
                node_id: target.clone(),
                pubkey: None,
                addrs: vec![],
                via: "mem".into(),
            }],
        };
        assert_eq!(t.local_node_id(), &local);
        assert_eq!(t.list_peers().len(), 1);
        let mut s = t.dial(&target).unwrap();
        s.write_all(b"hello").unwrap();
        let mut got = vec![0u8; 5];
        s.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hello");
        assert_eq!(s.peer(), Some(&target));
    }

    #[test]
    fn trait_rejects_dialing_unknown_peer() {
        // Pin: dial of a peer we never listed is io::ErrorKind::NotFound.
        // Not a panic, not a silent connect-to-self — a clean error
        // the agent surfaces upstream. Use match (not unwrap_err) so we
        // don't need Debug on Box<dyn MeshStream>.
        let t = MemTransport {
            local: NodeId::new("test-node-deadbeefcafebabe"),
            peers: vec![],
        };
        match t.dial(&NodeId::new("ghost-aaaaaaaaaaaaaaaa")) {
            Ok(_) => panic!("dial of unlisted peer must fail"),
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::NotFound),
        }
    }
}
