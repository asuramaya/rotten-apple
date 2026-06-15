//! `mesh.toml` — node-side mesh configuration.
//!
//! Lives at `/etc/rotten-apple/mesh.toml`. Defaults are biased toward
//! "single-host LAN with TOFU trust" which means a fresh install does
//! something sensible without any config at all. Static peers can be
//! listed explicitly to bypass discovery (e.g. when you know the IP
//! and want to skip mDNS, or when peers are behind NAT and reachable
//! only via static address + port).
//!
//! Parsed by `serde` from TOML; deserialization fails loudly on
//! unknown transport / trust kinds (better to refuse to start than
//! silently degrade trust).

use serde::Deserialize;

/// Top-level mesh config.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct MeshConfig {
    #[serde(default)]
    pub mesh: MeshSection,
    #[serde(default)]
    pub identity: IdentitySection,
    #[serde(default)]
    pub trust: TrustSection,
    /// Static peers — explicit entries the transport adds before
    /// discovery runs. Useful when you know the address and want to
    /// shortcut mDNS, or for cross-LAN peers reachable at a
    /// transport-specific address (tailnet IP, public WireGuard
    /// endpoint, etc).
    #[serde(default, rename = "peers")]
    pub peers: Vec<PeerEntry>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct MeshSection {
    /// Which transport to load. `lan` is the default — works in the
    /// no-VPN case and also covers Tailscale-via-router-upstream
    /// (peers appear at tailnet IPs that the OS resolves like LAN).
    #[serde(default = "default_transport_lan")]
    pub transport: TransportKind,
    /// Port on which the agent listens for inbound peer connections.
    /// Default 7042 — picked away from the well-known dev ports.
    #[serde(default = "default_port")]
    pub listen_port: u16,
}

fn default_transport_lan() -> TransportKind { TransportKind::Lan }
fn default_port() -> u16 { 7042 }

impl Default for MeshSection {
    fn default() -> Self {
        MeshSection { transport: TransportKind::Lan, listen_port: 7042 }
    }
}

#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
pub struct IdentitySection {
    /// Override path for the Ed25519 secret. Useful in tests; in
    /// production leave unset and accept the default
    /// `/var/lib/rotten-apple/node.key`.
    #[serde(default)]
    pub key_path: Option<String>,
    /// Override the node id (skips derive-from-machine-id). Used by
    /// peers that explicitly want a hand-picked id.
    #[serde(default)]
    pub node_id: Option<String>,
    /// Optional role hint baked into a freshly-derived node id.
    /// Cosmetic only — equality ignores it.
    #[serde(default)]
    pub role_hint: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct TrustSection {
    /// How peer pubkeys enter the trust store.
    #[serde(default = "default_trust_tofu")]
    pub mode: TrustMode,
}
fn default_trust_tofu() -> TrustMode { TrustMode::Tofu }

impl Default for TrustSection {
    fn default() -> Self { TrustSection { mode: TrustMode::Tofu } }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PeerEntry {
    /// Stable id of the peer.
    pub node_id: String,
    /// `host:port` (any of LAN, tailnet, public — transport
    /// just connects to it). Multiple addresses allowed for
    /// redundancy; transport tries them in order.
    #[serde(default)]
    pub addr: Vec<String>,
    /// Hex-encoded Ed25519 pubkey. Required when `trust.mode = "config"`;
    /// optional for `tofu` (will be learned and recorded on first
    /// contact).
    #[serde(default)]
    pub pubkey: Option<String>,
}

/// Transport kinds we ship support for. Adding a variant requires a
/// matching impl in `transport::*`. Unknown values fail
/// deserialization — agents must NOT silently fall back.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    /// LAN + mDNS discovery (default). Works for tailnet-via-router
    /// out of the box.
    Lan,
    /// Direct Tailscale: read `tailscale status --json`, dial peer
    /// tailnet IPs. Use when the host is on the tailnet itself
    /// (not behind a router that handles Tailscale).
    Tailscale,
    /// Direct WireGuard: read `wg-quick`-shaped config, dial peer
    /// endpoints.
    Wireguard,
    /// No discovery at all — only the `[[peers]]` list is used.
    Manual,
    /// Try `lan` first, fall through to `tailscale` if local node
    /// is on a tailnet. Convenience for laptops that move around.
    Auto,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TrustMode {
    /// Trust on first contact: log new peer pubkeys, persist them,
    /// alert on later mismatches. The default for low-friction setup.
    Tofu,
    /// Only peers explicitly listed in `[[peers]]` (with `pubkey`)
    /// are trusted. The strict, multi-tenant default.
    Config,
    /// Peers present a cert signed by a trusted CA (Phase 6+).
    Ca,
}

impl MeshConfig {
    pub fn from_str(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }

    /// Default config = LAN transport, TOFU trust, no static peers.
    pub fn defaults() -> Self { Self::default() }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_yields_safe_defaults() {
        // No file at all → LAN + TOFU + port 7042 + no static peers.
        // Pin every default so a regression here surfaces immediately
        // (the defaults are deployment-shaping decisions).
        let c = MeshConfig::from_str("").unwrap();
        assert_eq!(c.mesh.transport, TransportKind::Lan);
        assert_eq!(c.mesh.listen_port, 7042);
        assert_eq!(c.trust.mode, TrustMode::Tofu);
        assert!(c.peers.is_empty());
        assert!(c.identity.key_path.is_none());
        assert!(c.identity.node_id.is_none());
    }

    #[test]
    fn parses_minimal_static_peer() {
        // The "I know my desk box's IP, just talk to it" config —
        // no discovery needed, transport=manual + one peer entry.
        let toml = r#"
            [mesh]
            transport = "manual"

            [trust]
            mode = "tofu"

            [[peers]]
            node_id = "desk-box-7f3a9c4d2e1b8a6f"
            addr    = ["100.64.0.5:7042"]
        "#;
        let c = MeshConfig::from_str(toml).unwrap();
        assert_eq!(c.mesh.transport, TransportKind::Manual);
        assert_eq!(c.peers.len(), 1);
        let p = &c.peers[0];
        assert_eq!(p.node_id, "desk-box-7f3a9c4d2e1b8a6f");
        assert_eq!(p.addr, vec!["100.64.0.5:7042".to_string()]);
        assert!(p.pubkey.is_none(), "TOFU mode → pubkey learned later");
    }

    #[test]
    fn config_mode_requires_pubkey_in_principle() {
        // Schema-side, pubkey is `Option<String>` because TOFU doesn't
        // need it. Runtime-side, the trust-store-builder must enforce
        // `mode=config => pubkey present`. Pin both halves: parsing
        // succeeds without pubkey even in config mode (the runtime
        // enforcement happens elsewhere, not in deserialization).
        let toml = r#"
            [trust]
            mode = "config"

            [[peers]]
            node_id = "x"
        "#;
        let c = MeshConfig::from_str(toml).unwrap();
        assert_eq!(c.trust.mode, TrustMode::Config);
        assert!(c.peers[0].pubkey.is_none(),
                "schema permits this; runtime config-loader rejects it");
    }

    #[test]
    fn unknown_transport_fails_loudly() {
        // Refusing to start is much better than silently falling back
        // to a less-secure transport. Pin that an unknown variant is
        // a parse error, not a default.
        let toml = r#"[mesh]
            transport = "carrier-pigeon"
        "#;
        let err = MeshConfig::from_str(toml).unwrap_err();
        assert!(err.to_string().contains("carrier-pigeon"),
                "error must surface the bad value: {err}");
    }

    #[test]
    fn unknown_trust_mode_fails_loudly() {
        // Same rationale as transport: silent trust degradation = bad.
        let toml = r#"[trust]
            mode = "yolo"
        "#;
        let err = MeshConfig::from_str(toml).unwrap_err();
        assert!(err.to_string().contains("yolo"),
                "error must surface the bad value: {err}");
    }

    #[test]
    fn full_config_parses_with_explicit_pubkey() {
        // The "strict shop" config: Config trust + explicit pubkeys
        // for each peer. Round-trips both halves cleanly.
        let toml = r#"
            [mesh]
            transport = "lan"
            listen_port = 7042

            [identity]
            key_path = "/var/lib/rotten-apple/node.key"
            role_hint = "laptop"

            [trust]
            mode = "config"

            [[peers]]
            node_id = "desk-box-7f3a9c4d2e1b8a6f"
            addr    = ["100.64.0.5:7042", "192.168.1.10:7042"]
            pubkey  = "deadbeefcafebabe1111111122223333deadbeefcafebabe1111111122223333"
        "#;
        let c = MeshConfig::from_str(toml).unwrap();
        assert_eq!(c.identity.role_hint.as_deref(), Some("laptop"));
        assert_eq!(c.peers[0].addr.len(), 2);
        assert!(c.peers[0].pubkey.is_some());
    }
}
