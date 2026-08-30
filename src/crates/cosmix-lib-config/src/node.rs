//! Per-node identity and service configuration.
//!
//! `node.conf.mix` defines a node's mesh identity (name + WG IP) and
//! per-service settings.  Listen addresses are **derived** from
//! `wg_ip + port`, enforcing WG-only binding by construction.
//!
//! Config priority: CLI arg > `COSMIX_NODE_CONFIG` env var >
//! `~/.config/cosmix/node.conf.mix` > `/etc/cosmix/node.conf.mix` >
//! error.
//!
//! `.conf.mix` is the only format. The legacy `node.toml`
//! read-and-upgrade fallback was removed in C11
//! (`_doc/planned/2026-05-31-c11-toml-fallback-removal.md`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level per-node configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    /// Human-readable node name (e.g. "alpha", "delta").
    pub node: String,
    /// WireGuard IP address for this node (e.g. "192.0.2.5").
    pub wg_ip: String,
    /// Mesh identity. Per SPEC 01 §4 the mesh is identified by an
    /// optional `@<mesh-fqdn>` suffix on cross-mesh addresses, not by
    /// a label inside the local address. This field records the
    /// FQDN that this node belongs to (e.g. `"example.org"`) so
    /// that the cross-mesh form `<local>@<mesh-fqdn>` can be
    /// recognised at the router as referring to the local mesh.
    /// `None` = legacy / unset; routers must treat a missing value as
    /// the historical default (no cross-mesh recognition).
    pub mesh: Option<String>,

    pub noded: NodedConfig,
    /// SPEC 02 §4.2 broker observation operator allowlist. Top-level
    /// `[observe]` by design: this is an operator capability policy, not a
    /// transport tuning knob. Empty is the fail-closed default.
    pub observe: ObserveConfig,
    pub maild: MaildConfig,
    pub webd: WebdConfig,
    pub deskd: ServiceToggle,
    pub indexd: ServiceToggle,
    pub claud: ServiceToggle,
    pub mcp: ServiceToggle,
}

// ── Derived addresses ────────────────────────────────────────────────

impl NodeConfig {
    /// Broker listener address: `{wg_ip}:{noded.port}`.
    pub fn noded_listen(&self) -> String {
        format!("{}:{}", self.wg_ip, self.noded.port)
    }

    /// Broker WebSocket URL: `ws://{host}:{noded.port}/ws`. An **empty**
    /// `wg_ip` — a standalone / non-mesh node that omits it — falls back to
    /// loopback rather than a hostless `ws://:PORT/ws` that no client can
    /// reach (mirrors mix's `MixNodeConfig::broker_url`; see markc/mix#1).
    pub fn noded_url(&self) -> String {
        let host = if self.wg_ip.is_empty() {
            "127.0.0.1"
        } else {
            self.wg_ip.as_str()
        };
        format!("ws://{}:{}/ws", host, self.noded.port)
    }

    /// JMAP HTTP listen address: `{wg_ip}:{maild.jmap_port}`.
    pub fn jmap_listen(&self) -> String {
        format!("{}:{}", self.wg_ip, self.maild.jmap_port)
    }

    /// SMTP inbound listen address: `{wg_ip}:{maild.smtp_port}`.
    pub fn smtp_inbound_listen(&self) -> String {
        format!("{}:{}", self.wg_ip, self.maild.smtp_port)
    }

    /// SMTPS listen address: `{wg_ip}:{maild.smtps_port}`.
    pub fn smtps_listen(&self) -> String {
        format!("{}:{}", self.wg_ip, self.maild.smtps_port)
    }

    /// Web server listen address: `{wg_ip}:{webd.port}`.
    pub fn web_listen(&self) -> String {
        format!("{}:{}", self.wg_ip, self.webd.port)
    }

    /// JMAP upstream URL for webd reverse proxy.
    pub fn jmap_upstream(&self) -> String {
        format!("https://{}:{}", self.wg_ip, self.maild.jmap_port)
    }

    /// Resolve the webd listener set, validating per-interface listeners
    /// against the full vhost host-set.
    ///
    /// `all_hosts` is every vhost FQDN webd intends to serve — the union
    /// of `[[webd.vhost]]` primaries + aliases and the legacy
    /// `tls_server_name` entries (caller-deduplicated, host-parsed).
    ///
    /// **Empty `[webd] listener` ⇒ back-compat:** one implicit `wg`
    /// listener at `web_listen()` (`wg_ip:port`), internal, serving every
    /// host — byte-identical to the pre-listener single bind.
    ///
    /// **Non-empty ⇒** the array is used and validated:
    /// - non-empty unique ids; non-empty unique binds (each a parseable
    ///   `ip:port`);
    /// - no wildcard (`0.0.0.0` / `::`) sharing a port with another bind
    ///   (would `EADDRINUSE` or silently shadow);
    /// - every host named in a listener's `vhosts` is a known host
    ///   (in `all_hosts`);
    /// - every host in `all_hosts` is claimed by exactly one listener,
    ///   and that listener is **enabled** (a host on no listener, on a
    ///   disabled-only listener, or on two listeners is a hard error —
    ///   never a silent drop or ambiguous route).
    ///
    /// **`disabled_hosts` (B1 fail-soft)** is every vhost name webd
    /// dropped at resolve time (bad cert / missing www_dir / unopenable
    /// CMS DB). Such a host is already absent from `all_hosts` (it left
    /// no routing surface), but it may still be *named* in a listener's
    /// `vhosts` allowlist in the config. A name in `disabled_hosts` is
    /// **skipped** from the allowlist rather than rejected as an unknown
    /// vhost — so one bad cert disables that vhost without failing the
    /// whole listener set. A name that is in neither `all_hosts` nor
    /// `disabled_hosts` is still a hard "unknown vhost" error (catches
    /// config typos). Pass an empty set for the strict pre-B1 behaviour.
    pub fn synthesize_listeners(
        &self,
        all_hosts: &[String],
        disabled_hosts: &std::collections::HashSet<String>,
    ) -> Result<Vec<ResolvedWebdListener>> {
        use std::collections::{HashMap, HashSet};
        use std::net::SocketAddr;

        let listeners = &self.webd.listener;
        if listeners.is_empty() {
            return Ok(vec![ResolvedWebdListener {
                id: "wg".to_string(),
                bind: self.web_listen(),
                external: false,
                enabled: true,
                hosts: all_hosts.to_vec(),
            }]);
        }

        // ── ids + binds ──────────────────────────────────────────────
        // Dedup binds on the *parsed* SocketAddr, not the raw string, so
        // text variants of one address (`[::]:443` vs
        // `[0:0:0:0:0:0:0:0]:443`) can't slip two listeners onto the same
        // socket and have the runtime silently bring up one and drop the
        // other. Per port we track whether any bind is a wildcard
        // (`0.0.0.0` / `::`) and the total count: a wildcard listener
        // must be the *only* bind on its port. This is the conservative
        // family-agnostic policy — `0.0.0.0:P` and `[::]:P` would
        // `EADDRINUSE` on a dual-stack default, and even a non-overlapping
        // v4-wildcard + v6-specific pair is rejected rather than reasoned
        // about. Wildcards are discouraged here anyway (the substrate
        // posture is bind a *specific* interface — `feedback_wg_only_binding`).
        let mut seen_ids: HashSet<&str> = HashSet::new();
        let mut seen_binds: HashSet<SocketAddr> = HashSet::new();
        let mut port_binds: HashMap<u16, (bool, usize)> = HashMap::new();
        for l in listeners {
            if l.id.trim().is_empty() {
                anyhow::bail!("[webd] listener with empty id");
            }
            if !seen_ids.insert(l.id.as_str()) {
                anyhow::bail!("[webd] duplicate listener id {:?}", l.id);
            }
            if l.bind.trim().is_empty() {
                anyhow::bail!("[webd] listener {:?} has empty bind", l.id);
            }
            let addr: SocketAddr = l.bind.parse().with_context(|| {
                format!(
                    "[webd] listener {:?}: bind {:?} is not a valid ip:port",
                    l.id, l.bind
                )
            })?;
            if !seen_binds.insert(addr) {
                anyhow::bail!(
                    "[webd] duplicate listener bind {:?} (resolves to {addr})",
                    l.bind
                );
            }
            let entry = port_binds.entry(addr.port()).or_insert((false, 0));
            entry.0 |= addr.ip().is_unspecified();
            entry.1 += 1;
        }
        for (port, (has_wildcard, count)) in &port_binds {
            if *has_wildcard && *count > 1 {
                anyhow::bail!(
                    "[webd] port {port} has a wildcard (0.0.0.0 / ::) bind plus another bind \
                     — a wildcard listener must be the only bind on its port"
                );
            }
        }

        // ── host → owning listener (each host on exactly one) ────────
        let known: HashSet<&str> = all_hosts.iter().map(String::as_str).collect();
        let mut owner: HashMap<&str, &WebdListenerConfig> = HashMap::new();
        for l in listeners {
            for h in &l.vhosts {
                // B1 fail-soft: a host webd disabled at resolve time
                // (bad cert / missing www_dir / unopenable CMS DB) is
                // absent from `all_hosts` but may still be named here.
                // Skip it — it owns no listener and serves nothing —
                // rather than rejecting the whole set as "unknown vhost".
                if disabled_hosts.contains(h.as_str()) {
                    continue;
                }
                if !known.contains(h.as_str()) {
                    anyhow::bail!(
                        "[webd] listener {:?} references unknown vhost {:?} \
                         (not a configured webd.vhost host/alias or tls_server_name)",
                        l.id,
                        h
                    );
                }
                if let Some(prev) = owner.insert(h.as_str(), l) {
                    anyhow::bail!(
                        "[webd] vhost {:?} is served by two listeners ({:?} and {:?}) \
                         — a host belongs to exactly one listener",
                        h,
                        prev.id,
                        l.id
                    );
                }
            }
        }
        // Every served host must be claimed, by an *enabled* listener.
        for h in all_hosts {
            match owner.get(h.as_str()) {
                None => anyhow::bail!(
                    "[webd] vhost {:?} is not served by any listener \
                     — add it to a listener's `vhosts` allowlist",
                    h
                ),
                Some(l) if !l.enabled => anyhow::bail!(
                    "[webd] vhost {:?} is served only by the disabled listener {:?} \
                     — it would never be reachable (move it to an enabled listener)",
                    h,
                    l.id
                ),
                Some(_) => {}
            }
        }

        Ok(listeners
            .iter()
            .map(|l| ResolvedWebdListener {
                id: l.id.clone(),
                bind: l.bind.clone(),
                external: l.external,
                enabled: l.enabled,
                hosts: l.vhosts.clone(),
            })
            .collect())
    }
}

// ── Service section structs ──────────────────────────────────────────

/// SPEC 13 §9a D2 broker-admission posture (slice 2-c-1). Per-node operator
/// policy, set in `node.conf.mix` `[noded] admission`. **Default `off`** — a
/// fresh node never starts challenging until told to (clients-before-broker,
/// §16). `observe` challenges + verdicts + logs but NEVER refuses (the §7.8 B3
/// decide-by-doing vehicle); `enforce` refuses a failed gated session (2-c-2,
/// Mark-gated, hub-last).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AdmissionMode {
    #[default]
    Off,
    Observe,
    Enforce,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NodedConfig {
    pub port: u16,
    pub mesh_config: Option<String>,
    /// SPEC 13 §9a D2 admission posture (off | observe | enforce). Default off.
    pub admission: AdmissionMode,
}

impl Default for NodedConfig {
    fn default() -> Self {
        Self {
            port: 4200,
            mesh_config: None,
            admission: AdmissionMode::Off,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ObserveConfig {
    /// Anchored service-name globs admitted to `noded.observe.start`.
    /// The broker validates pattern syntax and ignores invalid entries.
    pub allowed_services: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MaildConfig {
    pub enabled: bool,
    pub jmap_port: u16,
    pub smtp_port: u16,
    pub smtps_port: u16,
    pub hostname: String,
    pub database: String,
    pub blob_dir: String,
    /// MDS (Mailbox Data Store) directory. Operators who relocate
    /// `database` / `blob_dir` should also set this so MDS state is
    /// co-located with the legacy mail data for backup/restore. The
    /// MailStore adapter opens / creates an `SqliteCasMds` rooted here.
    pub mds_dir: String,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    /// Multi-identity TLS (SNI-routed). When `tls.identity` is non-
    /// empty, the daemon uses it as the canonical identity list and
    /// ignores `tls_cert` / `tls_key`. When empty, the legacy pair
    /// (if set) is collapsed into a single-identity fallback by
    /// `cosmix-maild` so existing deployments continue working
    /// unchanged. See `_doc/planned/maild-sni-internal-ca.md`.
    pub tls: TlsConfig,
    pub spam_enabled: bool,
    pub spam_db_dir: Option<String>,
}

impl Default for MaildConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            jmap_port: 8443,
            smtp_port: 25,
            smtps_port: 465,
            hostname: String::new(),
            database: "/var/lib/cosmix-jmap/mail.db".into(),
            blob_dir: "/var/lib/cosmix-jmap/blobs".into(),
            mds_dir: "/var/lib/cosmix-jmap/mds".into(),
            tls_cert: None,
            tls_key: None,
            tls: TlsConfig::default(),
            spam_enabled: true,
            spam_db_dir: None,
        }
    }
}

/// Multi-identity TLS configuration.
///
/// One `TlsIdentityConfig` per SNI value to be served. The
/// resolver implementation lives in `cosmix-maild` (Phase 1
/// scope); this struct is the on-disk parse target only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    /// One entry per `[[maild.tls.identity]]` block.
    pub identity: Vec<TlsIdentityConfig>,
    /// Refuse SNI-mismatch and no-SNI handshakes when `true`. Off
    /// by default for MUA compatibility — modern MUAs send SNI on
    /// implicit-TLS ports but the matrix is not yet exhaustively
    /// verified.
    pub strict_sni: bool,
}

/// A single TLS identity for SNI-based cert selection.
///
/// `server_name` is the SNI value this identity matches. `default`
/// marks the identity used for plaintext / pre-STARTTLS greetings
/// and as the SNI-mismatch fallback. `no_sni_fallback` marks the
/// identity used when an implicit-TLS client doesn't send SNI at
/// all. The two flags are independent; an operator may set them
/// on the same row (typical) or on different rows (e.g. a
/// substrate-internal node where the WG side is the primary
/// identity).
/// `server_name`, `cert`, and `key` are required; omitting any one
/// fails the parse. The bool flags default to `false`. This is the
/// inverse of `TlsConfig`'s policy — an absent `[maild.tls]` block
/// is harmless (no identities, legacy collapse applies), but a
/// half-written `[[tls.identity]]` row would silently shadow the
/// legacy `tls_cert`/`tls_key` pair and present an empty-string
/// cert path to the resolver. Hard-failing the parse makes that
/// failure mode loud at startup instead of at handshake time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TlsIdentityConfig {
    pub server_name: String,
    pub cert: String,
    pub key: String,
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub no_sni_fallback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebdConfig {
    pub enabled: bool,
    pub port: u16,
    pub www_dir: String,
    pub tls_cert: Option<String>,
    pub tls_key: Option<String>,
    /// Expected web FQDN(s) for the legacy top-level
    /// `tls_cert`/`tls_key` pair. Used as the SAN-coverage list when
    /// `validate_le_chain` runs on the legacy collapse, and as the
    /// host-routing key-set for the legacy `Arc<VhostState>`. Every
    /// entry must be covered by the leaf certificate's SANs or
    /// startup hard-fails. Non-empty is required whenever the legacy
    /// pair is configured; the resolve step rejects an empty value
    /// with a clear error message. **This is the *web* identity** —
    /// independent of `served_mail_domains` (the autoconfig
    /// admission gate). Phase 3 deprecates the legacy top-level pair
    /// under the SPEC 12 namespace and this field deprecates with
    /// it.
    pub tls_server_name: Vec<String>,
    /// Optional plain-HTTP listener address (e.g. `"0.0.0.0:80"`).
    ///
    /// When set, webd binds a *second* listener serving the NS 3.0
    /// `00-default-http` shape: `301 → https://$host$request_uri` for
    /// everything (mail-client autoconfig / ACME paths are carved out
    /// in a follow-up — see `_doc/planned/maild-autoconfig.md`).
    ///
    /// Deliberately a full `addr:port` string, **not** a `wg_ip`-derived
    /// port like `web_listen()`. Mail-client autoconfig is fetched from
    /// the public internet, so this listener must bind a publicly
    /// reachable address — deriving it from the WireGuard IP would make
    /// it unreachable and defeat the purpose. `None` (the default) means
    /// **no plain-HTTP listener at all**, preserving the WG-only bind
    /// posture (`feedback_wg_only_binding`) on every node that has not
    /// explicitly opted in. Opening :80 is an edge-node deployment
    /// decision, expressed here as an explicit per-node config value
    /// rather than a webd code default.
    pub http_listen: Option<String>,
    /// LE Subscriber-Agreement URL the operator has accepted for this
    /// node's `letsencrypt_prod` vhosts. Per the umbrella's Open Q6
    /// gate and `_doc/planned/webd-vhosts-phase2.md`, a node with **any**
    /// `provider = "letsencrypt_prod"` row must set this field to the
    /// URL of the Subscriber Agreement the operator actually read. The
    /// gate strips leading/trailing whitespace at mint time and stores
    /// the **canonical post-trim string** in
    /// `account.letsencrypt_prod.meta.json` for audit; whitespace-only
    /// edits to the operator's raw input are tolerated, visible URL
    /// edits (host, path, scheme) trip the subsequent-boot mismatch
    /// detector. Staging-only nodes skip the gate (LE staging has no
    /// Subscriber Agreement to record).
    ///
    /// `None` (the default) is the safe default: a node with no prod
    /// vhosts is unaffected, while a node that gains a prod vhost
    /// without setting this field hard-fails at resolve time with a
    /// pointer at <https://letsencrypt.org/repository/>. Validation
    /// (LE host prefix, non-empty, trim-tolerance) is enforced by
    /// the private ToS gate inside the [`crate::acme_policy`] module
    /// and surfaces only through [`crate::acme_policy::resolve_webd_acme`]
    /// — see that module for the structural-unforgeability rationale.
    pub acme_tos_accepted: Option<String>,
    /// Mail domains this node serves client autoconfig/autodiscover
    /// for. **This is the security gate**, not a convenience list: a
    /// `Host`-derived domain that is not a member is `404`ed *before*
    /// any DNS lookup, closing the SSRF / DNS-amplification vector
    /// inherent in resolving an attacker-controlled Host
    /// (`feedback_bus_wire_trust_boundary`, maild-autoconfig.md
    /// §Security). Empty (the default) means autoconfig is effectively
    /// disabled — every request 404s and no resolver is constructed.
    pub served_mail_domains: Vec<String>,
    /// Internal mail host to advertise (and probe) in autoconfig instead of
    /// the domain's public MX. This node exposes the mail client protocols
    /// (IMAPS:993 / SMTPS:465) only on the WireGuard interface — the public
    /// FQDN serves :25 + :443 only — so autoconfig points clients at this
    /// WG-reachable host (e.g. `epsilon.example.com`). `None` (default) ⇒ legacy
    /// behaviour: resolve + probe the domain's public MX.
    #[serde(default)]
    pub autoconfig_mail_host: Option<String>,
    /// Per-vhost configuration. Each row picks one of two issuance
    /// modes: **manual-PEM** (`tls_cert` + `tls_key`, validated by
    /// `cosmix_daemon::tls::le_validator::validate_le_chain` at
    /// startup) or **ACME** (`acme = {...}`, provisioned by webd
    /// against Let's Encrypt). The two modes are mutually exclusive
    /// per row; `crate::acme_policy::resolve_webd_acme` rejects a row
    /// that sets both.
    ///
    /// Empty (the default) ⇒ webd serves the legacy top-level
    /// `tls_cert` + `tls_key` pair under every FQDN listed in
    /// `tls_server_name`. Non-empty ⇒ webd serves each listed
    /// vhost; the legacy fields, if also present, collapse to one
    /// resolver identity per `tls_server_name` entry, with only the
    /// first carrying `default = true, no_sni_fallback = true`.
    /// Duplicate FQDNs across the legacy collapse and any vhost row's
    /// primary/alias are caught by `SniCertResolver::from_config`.
    pub vhost: Vec<WebdVhostConfig>,
    /// Per-interface listeners (one `[[webd.listener]]` block each).
    ///
    /// **Empty (the default) ⇒ back-compat:** webd synthesizes one
    /// implicit WG listener at `web_listen()` (`wg_ip:port`) serving
    /// every vhost — byte-identical to the pre-listener single-bind
    /// behaviour, so existing nodes are untouched.
    ///
    /// Non-empty ⇒ webd binds exactly these listeners, each to an
    /// explicit interface IP with its own vhost allowlist (the
    /// "what is exposed where" block). This is how a node multi-homes
    /// a WG-only internal listener and a public external listener on
    /// the same daemon — the kernel never completes a handshake for
    /// an internal vhost off the mesh. See
    /// [`NodeConfig::synthesize_listeners`] for the resolution +
    /// validation rules.
    #[serde(default)]
    pub listener: Vec<WebdListenerConfig>,
    /// Listener-control (SPEC-12 `webd.listeners` kill-switch + guard
    /// namespace) settings. Currently just the operator allowlist for
    /// the privileged `props.write:webd.listeners` capability (cutting
    /// external traffic / retuning guards is operator-tier, unlike the
    /// permissive every-WG-peer posture of `vhosts`/`accounts`).
    #[serde(default)]
    pub listeners: WebdListenersConfig,
}

/// `[webd.listeners]` — listener-control settings (the SPEC-12
/// `webd.listeners` namespace's L0 seed). The namespace itself holds
/// the live `enabled`/guard state; this block carries only the
/// bootstrap-time policy that can't live in L1 (it gates *who* may
/// write L1).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WebdListenersConfig {
    /// Bus sender names (`cmd.from`) allowed to `props.write` the
    /// `webd.listeners` namespace — i.e. flip the `enabled` kill switch
    /// or retune a listener's guards. Empty (the default) ⇒ no remote
    /// peer may write (the daemon's own backend-origin writes — bootstrap
    /// + the reaction loop's status writeback — still work). read/describe
    ///   stay open to every WG peer. Tightens further when the cross-mesh
    ///   principal model lands; the capability name is stable now.
    pub operators: Vec<String>,
}

impl Default for WebdConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 443,
            www_dir: "/var/lib/cosmix/www".into(),
            tls_cert: None,
            tls_key: None,
            tls_server_name: Vec::new(),
            http_listen: None,
            acme_tos_accepted: None,
            served_mail_domains: Vec::new(),
            autoconfig_mail_host: None,
            vhost: Vec::new(),
            listener: Vec::new(),
            listeners: WebdListenersConfig::default(),
        }
    }
}

/// One `[[webd.listener]]` row — a per-interface listener with a vhost
/// allowlist. The bind is an **explicit** interface IP (e.g. the WG
/// address `192.0.2.5:443` or a public `203.0.113.5:443`), never a
/// `wg_ip`-derived port like `web_listen()`: binding a specific
/// interface is the kernel-level isolation boundary
/// (`feedback_wg_only_binding` — public is an explicit opt-in).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WebdListenerConfig {
    /// Stable id — the control-verb key, log tag, and connection tag.
    /// Unique within the node's listener set; non-empty.
    pub id: String,
    /// Bind address `"ip:port"`. A specific interface IP, not a
    /// wildcard sharing a port with another listener (rejected by
    /// `synthesize_listeners`).
    pub bind: String,
    /// Public / untrusted-facing? Drives the `external` connection
    /// flag and the daemon's guard defaults. Does not itself change
    /// bind behaviour — the address does.
    pub external: bool,
    /// Bootstrap enable (the config-time on/off). The live kill
    /// switch is the SPEC-12 `webd.listeners` namespace (Phase 3); a
    /// disabled listener is held but not bound. Default `true`.
    pub enabled: bool,
    /// The vhost FQDNs served on this listener — the allowlist. Every
    /// vhost webd serves must be named in exactly one **enabled**
    /// listener; a host named here must be a known vhost
    /// (`synthesize_listeners` rejects both gaps).
    pub vhosts: Vec<String>,
}

impl Default for WebdListenerConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            bind: String::new(),
            external: false,
            // A listener row that omits `enabled` is on — the
            // serde(default) fallback must not silently disable it.
            enabled: true,
            vhosts: Vec::new(),
        }
    }
}

/// A resolved webd listener — the output of
/// [`NodeConfig::synthesize_listeners`]. Carries the bind address and
/// the (validated, complete) set of vhost hosts it serves. webd turns
/// each of these into a `cosmix_daemon::listen::ListenerSpec` plus a
/// scoped `ListenerTls`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedWebdListener {
    pub id: String,
    pub bind: String,
    pub external: bool,
    pub enabled: bool,
    /// Vhost FQDNs served here, in declaration order.
    pub hosts: Vec<String>,
}

/// Per-vhost configuration row, one per `[[webd.vhost]]` block in
/// `node.conf.mix`. Each row picks one of two issuance modes:
///
/// 1. **Manual-PEM** — `tls_cert` + `tls_key` are operator-supplied
///    paths to a Let's-Encrypt-issued chain that
///    `cosmix_daemon::tls::le_validator::validate_le_chain` runs
///    against at startup. A non-LE chain (self-signed, internal-CA,
///    DigiCert, etc.) hard-fails the bind.
/// 2. **ACME** — `acme = {...}` declares provider + challenge +
///    contact_email; the webd provisioner obtains a Let's Encrypt
///    cert via the configured ACME directory.
///
/// The two modes are mutually exclusive per row; the lib-config
/// resolver (`crate::acme_policy::resolve_webd_acme`) rejects a row
/// that sets both.
///
/// NOTE: this struct deliberately does NOT use `deny_unknown_fields`
/// (unlike the stricter core structs). node.conf.mix is read by many
/// daemons of DIFFERENT versions, and webd-vhost fields are the
/// fastest-evolving part of it; a strict deny here made the whole
/// config unparseable for any daemon built from a branch older than the
/// newest field, dropping its broker resolution to loopback. Tolerating
/// unknown webd-vhost fields keeps the shared config forward-compatible.
/// Typos in a vhost row still surface at resolve time (a row with a bad
/// cert / missing www_dir / unopenable CMS DB is dropped).
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WebdVhostConfig {
    /// Primary FQDN this vhost serves. LDH-validated against
    /// `cosmix_daemon::http_host::parse_request_host` at startup.
    pub host: String,
    /// Optional alias hostnames that route to this same vhost. Each
    /// alias goes through the same LDH validation as `host`.
    pub aliases: Vec<String>,
    /// Filesystem path to this vhost's document root. Must exist at
    /// startup; webd hard-fails the bind otherwise.
    pub www_dir: String,
    /// Path to operator-supplied PEM cert chain (leaf +
    /// intermediates). Required in Phase 1; mutually exclusive with
    /// `acme` once Phase 2 lands. Validated by
    /// `cosmix_daemon::tls::le_validator::validate_le_chain` at
    /// startup; a non-LE chain (self-signed, internal-CA, DigiCert,
    /// etc.) hard-fails the bind.
    pub tls_cert: Option<String>,
    /// Path to operator-supplied PEM private key matching
    /// `tls_cert`. Required if `tls_cert` is set.
    pub tls_key: Option<String>,
    /// Real ACME sub-config (Phase 2). Mutually exclusive with
    /// `tls_cert` / `tls_key`; `crate::acme_policy::resolve_webd_acme`
    /// rejects a row that sets both. `None` (the default) means this
    /// vhost runs in manual-PEM mode (Phase 1 path); `Some(_)` means
    /// the webd provisioner is expected to obtain a Let's Encrypt
    /// cert via ACME for this row.
    pub acme: Option<WebdVhostAcmeConfig>,
    /// Per-vhost CMS SQLite path. Absent ⇒ this vhost has no posts
    /// surface; the `/api/posts*` routes 404 for this host.
    pub cms_db_path: Option<String>,
    /// Auxiliary SQLite databases ATTACHed onto this vhost's connection at
    /// open, each under its own schema name. Lets a route family keep its
    /// state in a database with an INDEPENDENT lifecycle from `cms_db_path`
    /// (e.g. an admin panel whose data must survive a cms.db re-clone). Each
    /// entry is `{ name, path }`; `name` is a validated schema identifier
    /// (`[a-z_][a-z0-9_]{0,31}`, never `main`/`temp`) referenced as
    /// `<name>.<table>` in SQL. Access to an attached schema is NOT granted by
    /// the plain `db` capability — a route must additionally hold the matching
    /// `db-schema:<name>` capability (enforced by a per-statement SQLite
    /// authorizer), so co-hosted handlers on the shared connection cannot read
    /// or write another family's schema. Absent/empty ⇒ no aux databases.
    pub aux_dbs: Vec<WebdAuxDb>,
    /// Per-vhost JMAP upstream URL. Absent ⇒ this vhost has no mail
    /// surface; the `/jmap` + `/jmap/{*rest}` routes 404 for this host.
    pub jmap_upstream: Option<String>,
    /// Per-vhost noded WebSocket URL. Absent ⇒ this vhost has no Bus
    /// bridge surface.
    pub noded_ws: Option<String>,
    /// Per-vhost documentation directory. Absent ⇒ this vhost has
    /// no `/docs/` route.
    pub docs_dir: Option<String>,
    /// DEV-ONLY auto-session identity (email + password — set BOTH or neither).
    /// When set, a request to this vhost that carries no valid `cosmix_session`
    /// cookie is treated as this maild identity — `$SESSION.authenticated` is
    /// true and the `jmap()` seam authenticates to maild via HTTP Basic.
    /// Honoured ONLY on an internal listener: the hard gate is per-request
    /// (`!scope.external`), and webd additionally refuses at startup to bind a
    /// dev_session vhost unless every serving listener is `external = false` AND
    /// bound to an internal (loopback/RFC1918/ULA/link-local) address. For a
    /// sealed WG dev box; NEVER set on a public vhost.
    pub dev_session_email: Option<String>,
    /// `skip_serializing` so a secret can never round-trip into a serialized
    /// config dump; also redacted by the manual `Debug` below. (Deserialization
    /// from `node.conf.mix` is unaffected.)
    #[serde(skip_serializing)]
    pub dev_session_password: Option<String>,
    /// PUBLIC read-only content credential (email + password — set BOTH or
    /// neither). When set, an ANONYMOUS request to this vhost (no session cookie,
    /// no dev_session) has its `jmap()` seam authenticate to maild via HTTP Basic
    /// as this identity — but it grants NO session (`$SESSION.authenticated`
    /// stays false: no admin, no manage). Intended for a DEDICATED content
    /// account (vtoken segment-1) holding only public-intended content, so the
    /// public read is safe by construction. UNLIKE `dev_session`, this is NOT
    /// internal-gated — it is the public `:443` blog-read path. The credential
    /// is consumed ONLY by routes that opt in via the `public_read` capability
    /// (so it is bounded to audited routes, not vhost-wide across every `jmap`
    /// handler). (A maild-enforced scoped read-only principal is the future
    /// hardening; for now the dedicated-account boundary + the route opt-in +
    /// the handler's redaction allowlist + live predicate are the guarantees.)
    pub public_read_email: Option<String>,
    /// `skip_serializing` + redacted in `Debug` (mirrors `dev_session_password`).
    #[serde(skip_serializing)]
    pub public_read_password: Option<String>,
    /// SYSTEM transactional-sender credential (email + password — set BOTH or
    /// neither). When set, webd can send transactional mail (2FA codes,
    /// registration confirm/approval links) FROM this account with **no user
    /// session** — the pre-auth send path. The account is a real maild account
    /// on this vhost's own mail domain (e.g. `noreply@<domain>`); maild's
    /// RFC 6409 §6.1 envelope-sender check means it can only send AS itself, so
    /// the `From` is pinned to this address. Consumed ONLY by webd's
    /// `send_system_mail` (never a vhost-wide `jmap` credential like
    /// `public_read`). NOT internal-gated — transactional mail is a production
    /// surface — so guard the callers behind rate-limiting (P0c) before exposing
    /// any public trigger.
    pub system_sender_email: Option<String>,
    /// `skip_serializing` + redacted in `Debug` (mirrors `public_read_password`).
    #[serde(skip_serializing)]
    pub system_sender_password: Option<String>,
    /// Email-2FA operator BREAK-GLASS (default `false`). The 2FA
    /// enrollment lookup fails CLOSED: an INDETERMINATE read (broker
    /// down, timeout, transport error) refuses the login outright, so a
    /// broker fault can't be induced to skip a user's second factor —
    /// but that also makes the broker a login-availability SPOF for the
    /// vhost. Setting this to `true` lets an indeterminate lookup
    /// proceed password-only, logged loudly per bypassed login. It is
    /// an EXPLICIT, AUDITED operator action for a confirmed outage
    /// window (root-owned config edit + webd restart — never a silent
    /// fallback); remove it as soon as the broker is healthy.
    /// Determinate reads (2FA on / off / no props row) are never
    /// affected.
    pub mfa_break_glass: bool,
}

impl std::fmt::Debug for WebdVhostConfig {
    // Hand-rolled (Debug dropped from the derive above) so `dev_session_password`
    // can never render in cleartext via a `?`-format of a parsed config.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebdVhostConfig")
            .field("host", &self.host)
            .field("aliases", &self.aliases)
            .field("www_dir", &self.www_dir)
            .field("tls_cert", &self.tls_cert)
            .field("tls_key", &self.tls_key)
            .field("acme", &self.acme)
            .field("cms_db_path", &self.cms_db_path)
            .field("aux_dbs", &self.aux_dbs)
            .field("jmap_upstream", &self.jmap_upstream)
            .field("noded_ws", &self.noded_ws)
            .field("docs_dir", &self.docs_dir)
            .field("dev_session_email", &self.dev_session_email)
            .field(
                "dev_session_password",
                &self.dev_session_password.as_ref().map(|_| "<redacted>"),
            )
            .field("public_read_email", &self.public_read_email)
            .field(
                "public_read_password",
                &self.public_read_password.as_ref().map(|_| "<redacted>"),
            )
            .field("system_sender_email", &self.system_sender_email)
            .field(
                "system_sender_password",
                &self.system_sender_password.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// One auxiliary SQLite database ATTACHed to a vhost connection
/// (`[[webd.vhost.aux_dbs]]` in `node.conf.mix`).
///
/// `deny_unknown_fields` so a typo'd key is a loud config error rather than a
/// silently-ignored aux database. Validation of `name` (schema identifier) and
/// `path` (absolute, distinct from cms/legacy paths) happens at open time in
/// webd, not here — a bad row fails that one vhost soft, never the whole node.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebdAuxDb {
    /// Schema name the database is ATTACHed as; referenced `<name>.<table>` in
    /// SQL and gated by the `db-schema:<name>` route capability.
    pub name: String,
    /// Filesystem path to the SQLite file. Created (with parent dirs) at open
    /// if absent, opened WAL + `synchronous=NORMAL`.
    pub path: String,
}

/// Per-vhost ACME sub-config (`[webd.vhost.acme]` in `node.conf.mix`).
///
/// All three fields are required — the resolve step rejects a row
/// with `provider`/`contact_email` omitted at serde time
/// (`deny_unknown_fields` does the reverse direction; the
/// `serde(default)` shape would silently parse a half-filled block).
/// `challenge` defaults to `Http01` so the common case is one line of
/// config; an explicit `dns01` value parses successfully but is
/// rejected by the resolver with a Phase 4 pointer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebdVhostAcmeConfig {
    /// Which ACME directory to talk to. Closed enum — the umbrella's
    /// "Let's Encrypt only" directive treats adding a non-LE provider
    /// as a normative-doc change, not an API-compat extension.
    pub provider: WebdAcmeProvider,
    /// Which challenge type to satisfy. Defaults to HTTP-01 (the only
    /// Phase 2 implemented variant); a configured `dns01` parses but
    /// is rejected by `crate::acme_policy::resolve_webd_acme` with a
    /// pointer at Phase 4.
    #[serde(default = "default_challenge")]
    pub challenge: WebdAcmeChallenge,
    /// Contact email registered with the ACME directory. Used by LE
    /// for expiration warnings; not exposed to challenge responders.
    /// Sanity-validated (must contain `@`, no CR/LF, ≤ 253 bytes) by
    /// the resolver — RFC-5322 conformance is the operator's job.
    pub contact_email: String,
}

fn default_challenge() -> WebdAcmeChallenge {
    WebdAcmeChallenge::Http01
}

/// ACME provider selection. Closed (`#[non_exhaustive]` deliberately
/// omitted) so a hypothetical fork adding a non-LE provider is forced
/// to confront every match site that today exhaustively enumerates
/// `LetsEncryptProd` and `LetsEncryptStaging`.
///
/// The wire spelling is pinned per-variant rather than via
/// `rename_all = "snake_case"` because the operator-facing form is
/// `"letsencrypt_prod"` / `"letsencrypt_staging"` (no underscore
/// between *lets* and *encrypt*) — it matches Let's Encrypt's own
/// branding, every reference in `_doc/planned/webd-vhosts*.md`, every
/// in-source comment in `cosmix-lib-daemon::tls`, and the meta-file
/// names (`account.letsencrypt_prod.meta.json`) the Phase 2 plan
/// reserves on disk. `rename_all = "snake_case"` would derive
/// `"lets_encrypt_*"`, splitting the wire form away from the docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebdAcmeProvider {
    /// Let's Encrypt **production** directory. Subject to the LE
    /// Subscriber-Agreement gate (umbrella Open Q6) — a node with
    /// any `LetsEncryptProd` row must also set `[webd]
    /// acme_tos_accepted` or the resolver hard-fails at startup.
    #[serde(rename = "letsencrypt_prod")]
    LetsEncryptProd,
    /// Let's Encrypt **staging** directory. Untrusted certs intended
    /// for end-to-end issuance testing without burning prod
    /// rate-limit slots. No ToS gate (LE staging accepts any
    /// account by design).
    #[serde(rename = "letsencrypt_staging")]
    LetsEncryptStaging,
}

/// ACME challenge type selection. `Dns01` is reserved (parses
/// successfully) but the resolver rejects it with a Phase 4 pointer —
/// DNS-01 needs cosmix-dnsd's RFC-2136-style update surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebdAcmeChallenge {
    /// HTTP-01: webd publishes the keyauth at
    /// `http://<fqdn>/.well-known/acme-challenge/<token>` from a
    /// shared in-memory map populated by the provisioner.
    Http01,
    /// DNS-01: reserved for Phase 4. Parses successfully so the
    /// resolver can produce a clean Phase 4 pointer rather than a
    /// serde "unknown variant" error.
    Dns01,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceToggle {
    pub enabled: bool,
}

impl Default for ServiceToggle {
    fn default() -> Self {
        Self { enabled: true }
    }
}

// ── Loading ──────────────────────────────────────────────────────────

const ENV_VAR: &str = "COSMIX_NODE_CONFIG";

/// Search paths in priority order (after env var).
///
/// In user mode (uid != 0) `cosmix_path(Etc)` resolves to
/// `~/.config/cosmix/`, which means a system daemon user (e.g.
/// `cosmix-maild`) running an unprivileged CLI would never find a
/// node config placed at `/etc/cosmix/`. The doc comment at the top of
/// this file already promised user-XDG-then-system-etc; this honours
/// that promise by appending the system directory as a secondary
/// candidate whenever it differs from the primary (root mode
/// dedupes naturally). Within each directory `node.conf.mix` is the
/// sole config file.
///
/// `COSMIX_ETC` is the explicit isolation knob (tests, chroots,
/// alternate installs): when it is set we honour the caller's intent
/// and do NOT silently fall through to the host's `/etc/cosmix/`.
fn search_paths() -> Vec<PathBuf> {
    let primary_dir = crate::cosmix_path(crate::CosmixDir::Etc);
    let cosmix_etc_set = std::env::var_os("COSMIX_ETC").is_some();
    build_search_paths(&primary_dir, cosmix_etc_set)
}

/// Pure helper extracted from `search_paths()` so the search order
/// can be unit-tested without touching process env vars (which would
/// poison the `OnceLock`-cached path resolver for sibling tests).
///
/// Emits one `node.conf.mix` per candidate directory, the user dir
/// ahead of the system dir.
fn build_search_paths(primary_dir: &Path, cosmix_etc_set: bool) -> Vec<PathBuf> {
    let mut dirs = vec![primary_dir.to_path_buf()];
    if !cosmix_etc_set {
        let system = PathBuf::from("/etc/cosmix");
        if !dirs.contains(&system) {
            dirs.push(system);
        }
    }
    dirs.into_iter()
        .map(|dir| dir.join("node.conf.mix"))
        .collect()
}

/// Load node config from the given path (strict-data `.conf.mix`).
///
/// A read or parse failure is a hard error with the path in the chain.
/// C11 removed the legacy `.toml` parse + upgrade-write; a `.toml` path
/// now simply fails to parse as strict-data.
pub fn load_from(path: &Path) -> Result<NodeConfig> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let config: NodeConfig = cosmix_mix::from_conf_mix_str(&contents)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", path.display()))?;
    Ok(config)
}

/// Load node config using the standard search order.
///
/// Returns `Ok(Some(config))` if found, `Ok(None)` if no file exists
/// at any search path.
pub fn load_node_config() -> Result<Option<NodeConfig>> {
    // 1. Environment variable
    if let Ok(path) = std::env::var(ENV_VAR) {
        let config = load_from(Path::new(&path)).with_context(|| format!("{ENV_VAR}={path}"))?;
        tracing::debug!(source = %path, env_var = ENV_VAR, "loaded node config");
        return Ok(Some(config));
    }

    // 2. Standard search paths
    for path in search_paths() {
        if path.exists() {
            let config = load_from(&path)?;
            tracing::debug!(source = %path.display(), "loaded node config");
            return Ok(Some(config));
        }
    }

    Ok(None)
}

/// Load node config, returning an error if not found.
pub fn require_node_config() -> Result<NodeConfig> {
    load_node_config()?.with_context(|| {
        let paths: Vec<_> = search_paths()
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        format!(
            "node config not found — set {ENV_VAR} or place node.conf.mix at: {}",
            paths.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn webd_vhost_tolerates_unknown_fields_core_structs_still_strict() {
        // Forward-compat (drift fix): `WebdVhostConfig` no longer denies unknown
        // fields, so a node.conf.mix carrying a newer webd-vhost field (e.g.
        // `public_read_email`, added on another branch) still parses — any daemon
        // built from an older branch can still resolve the broker URL from the
        // shared config instead of failing the whole parse and dropping to loopback.
        let v: WebdVhostConfig = cosmix_mix::from_conf_mix_str(
            r#"{ "host": "x.example.org", "www_dir": "/srv/www", "public_read_email_future": "a@b.org" }"#,
        )
        .expect("an unknown webd-vhost field must not break parsing");
        assert_eq!(v.host, "x.example.org");

        // A core struct that keeps `deny_unknown_fields` (it has `default`, so the
        // failure is the unknown field, not a missing one) still rejects typos.
        let core: Result<WebdListenerConfig, _> =
            cosmix_mix::from_conf_mix_str(r#"{ "id": "x", "bogus_unknown_field": true }"#);
        assert!(
            core.is_err(),
            "a strict core struct must still reject unknown fields"
        );
    }

    #[test]
    fn noded_url_empty_wg_ip_falls_back_to_loopback() {
        // A standalone / non-mesh node omits wg_ip; the broker URL must resolve
        // to loopback, not a hostless ws://:PORT/ws no client can reach (mix#1).
        let mut cfg = NodeConfig {
            wg_ip: String::new(),
            ..Default::default()
        };
        assert_eq!(cfg.noded_url(), "ws://127.0.0.1:4200/ws");
        cfg.wg_ip = "192.0.2.9".into();
        assert_eq!(cfg.noded_url(), "ws://192.0.2.9:4200/ws");
    }

    // Tests exercise the pure `build_search_paths()` helper with
    // explicit inputs, so they do not depend on (and do not mutate)
    // the inherited `COSMIX_ETC` env var or the `OnceLock`-cached
    // path resolver in `paths.rs`. This keeps them hermetic regardless
    // of the test harness's environment.

    #[test]
    fn build_search_paths_user_mode_no_env_appends_system_fallback() {
        let dir = PathBuf::from("/home/alice/.config/cosmix");
        let paths = build_search_paths(&dir, false);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/alice/.config/cosmix/node.conf.mix"),
                PathBuf::from("/etc/cosmix/node.conf.mix"),
            ],
            "user-mode (no COSMIX_ETC) must search user dir then /etc/cosmix"
        );
    }

    #[test]
    fn build_search_paths_cosmix_etc_set_isolates() {
        // The COSMIX_ETC isolation case — the original Medium finding.
        // When the caller has explicitly overridden the etc dir, we
        // do NOT silently fall through to the host's /etc/cosmix.
        let dir = PathBuf::from("/sandbox/etc/cosmix");
        let paths = build_search_paths(&dir, true);
        assert_eq!(
            paths,
            vec![PathBuf::from("/sandbox/etc/cosmix/node.conf.mix")],
            "COSMIX_ETC set must suppress /etc/cosmix fallback"
        );
    }

    #[test]
    fn build_search_paths_root_mode_dedupes() {
        let dir = PathBuf::from("/etc/cosmix");
        let paths = build_search_paths(&dir, false);
        assert_eq!(
            paths,
            vec![PathBuf::from("/etc/cosmix/node.conf.mix")],
            "root mode (primary dir already == system) must not duplicate"
        );
    }

    // The maild-TLS config-shape tests now exercise the `.conf.mix`
    // strict-data path (the real load format) via the serde bridge.
    // `.conf.mix` map literals separate entries with `,` (newlines are
    // suppressed inside `{ }`); the top-level body uses newlines.

    #[test]
    fn maild_tls_identities_parse_multi() {
        let src = r#"
            node: "alpha"
            wg_ip: "192.0.2.5"
            maild: {
              hostname: "alpha.example.org",
              tls: {
                identity: [
                  { server_name: "alpha.example.org", cert: "/etc/cosmix/maild/tls/lan.crt", key: "/etc/cosmix/maild/tls/lan.key", default: true, no_sni_fallback: true },
                  { server_name: "alpha.bus.example.org", cert: "/etc/cosmix/maild/tls/wg.crt", key: "/etc/cosmix/maild/tls/wg.key" }
                ]
              }
            }
        "#;
        let nc: NodeConfig = cosmix_mix::from_conf_mix_str(src).expect("parse");
        assert_eq!(nc.maild.tls.identity.len(), 2);
        assert_eq!(nc.maild.tls.identity[0].server_name, "alpha.example.org");
        assert!(nc.maild.tls.identity[0].default);
        assert!(nc.maild.tls.identity[0].no_sni_fallback);
        assert_eq!(
            nc.maild.tls.identity[1].server_name,
            "alpha.bus.example.org"
        );
        assert!(!nc.maild.tls.identity[1].default);
        assert!(!nc.maild.tls.identity[1].no_sni_fallback);
        assert!(!nc.maild.tls.strict_sni);
    }

    #[test]
    fn maild_tls_strict_sni_toggle() {
        let src = "maild: { tls: { strict_sni: true } }\n";
        let nc: NodeConfig = cosmix_mix::from_conf_mix_str(src).expect("parse");
        assert!(nc.maild.tls.strict_sni);
        assert!(nc.maild.tls.identity.is_empty());
    }

    #[test]
    fn maild_tls_legacy_only_leaves_identity_empty() {
        let src = "maild: { tls_cert: \"/etc/cosmix/maild/tls/legacy.crt\", \
                   tls_key: \"/etc/cosmix/maild/tls/legacy.key\" }\n";
        let nc: NodeConfig = cosmix_mix::from_conf_mix_str(src).expect("parse");
        assert!(nc.maild.tls.identity.is_empty());
        assert_eq!(
            nc.maild.tls_cert.as_deref(),
            Some("/etc/cosmix/maild/tls/legacy.crt"),
            "legacy pair must still parse unchanged"
        );
    }

    #[test]
    fn noded_admission_parses_lowercase_and_defaults_off() {
        // Absent → off (a fresh node never starts challenging, §16).
        let nc: NodeConfig = cosmix_mix::from_conf_mix_str("node: \"alpha\"\n").expect("parse");
        assert_eq!(nc.noded.admission, AdmissionMode::Off);
        // Explicit lowercase values round-trip.
        for (s, want) in [
            ("off", AdmissionMode::Off),
            ("observe", AdmissionMode::Observe),
            ("enforce", AdmissionMode::Enforce),
        ] {
            let src = format!("noded: {{ admission: \"{s}\" }}\n");
            let nc: NodeConfig = cosmix_mix::from_conf_mix_str(&src).expect("parse");
            assert_eq!(nc.noded.admission, want, "admission={s}");
        }
    }

    #[test]
    fn observe_allowlist_parses_and_defaults_fail_closed() {
        let empty: NodeConfig =
            cosmix_mix::from_conf_mix_str("node: \"alpha\"\n").expect("parse defaults");
        assert!(empty.observe.allowed_services.is_empty());

        let configured: NodeConfig = cosmix_mix::from_conf_mix_str(
            "observe: { allowed_services: [\"tower-bevy-*\", \"log-observe-*\"] }\n",
        )
        .expect("parse observe allowlist");
        assert_eq!(
            configured.observe.allowed_services,
            ["tower-bevy-*", "log-observe-*"]
        );
    }

    #[test]
    fn maild_tls_identity_row_missing_required_field_fails_parse() {
        // An operator writing a tls identity row but forgetting `cert`
        // (or `key`, or `server_name`) must hit a hard parse error at
        // startup, not silently inherit empty strings that would later
        // shadow the legacy `tls_cert` / `tls_key` pair with an
        // unservable identity. `TlsIdentityConfig` has no
        // `#[serde(default)]`, so the three string fields are required.
        let cases = [
            // missing cert
            "maild: { tls_cert: \"/etc/legacy.crt\", tls_key: \"/etc/legacy.key\", \
             tls: { identity: [ { server_name: \"host.example\", key: \"/etc/host.key\" } ] } }\n",
            // missing key
            "maild: { tls: { identity: [ { server_name: \"host.example\", cert: \"/etc/host.crt\" } ] } }\n",
            // missing server_name
            "maild: { tls: { identity: [ { cert: \"/etc/host.crt\", key: \"/etc/host.key\" } ] } }\n",
        ];
        for (i, src) in cases.iter().enumerate() {
            let res: Result<NodeConfig, _> = cosmix_mix::from_conf_mix_str(src);
            assert!(
                res.is_err(),
                "case {i}: malformed identity row must fail parse, got Ok"
            );
        }
    }

    #[test]
    fn load_from_reads_conf_mix() {
        // C11: `.conf.mix` is the only format; load_from reads it
        // directly with no upgrade-write.
        let dir = std::env::temp_dir().join(format!("cosmix-node-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let conf_mix_path = dir.join("node.conf.mix");
        std::fs::write(&conf_mix_path, "node: \"alpha\"\nwg_ip: \"192.0.2.5\"\n").unwrap();

        let cfg = load_from(&conf_mix_path).expect("conf.mix load");
        assert_eq!(cfg.node, "alpha");
        assert_eq!(cfg.wg_ip, "192.0.2.5");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_toml_is_hard_error() {
        // A `.toml` path now fails to parse as strict-data (no fallback).
        let dir = std::env::temp_dir().join(format!("cosmix-node-toml-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let toml_path = dir.join("node.toml");
        std::fs::write(&toml_path, "node = \"alpha\"\nwg_ip = \"192.0.2.5\"\n").unwrap();
        assert!(load_from(&toml_path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn webd_acme_provider_wire_form_matches_docs() {
        // The operator-facing form is `letsencrypt_prod` /
        // `letsencrypt_staging` (no underscore between *lets* and
        // *encrypt*) per every reference in
        // `_doc/planned/webd-vhosts*.md`, every in-source comment in
        // `cosmix-lib-daemon::tls`, and the on-disk meta-file names
        // (`account.letsencrypt_prod.meta.json`) the Phase 2 plan
        // reserves. Pinned here so a future refactor that swaps
        // the per-variant `#[serde(rename = ...)]` back to a
        // `rename_all = "snake_case"` (which would derive
        // `lets_encrypt_*`) regresses loudly in CI rather than
        // silently breaking every operator config that uses the
        // documented spelling.
        let prod = serde_json::to_string(&WebdAcmeProvider::LetsEncryptProd).expect("serialise");
        let stg = serde_json::to_string(&WebdAcmeProvider::LetsEncryptStaging).expect("serialise");
        assert_eq!(prod, "\"letsencrypt_prod\"");
        assert_eq!(stg, "\"letsencrypt_staging\"");

        let parsed_prod: WebdAcmeProvider =
            serde_json::from_str("\"letsencrypt_prod\"").expect("deserialise prod");
        let parsed_stg: WebdAcmeProvider =
            serde_json::from_str("\"letsencrypt_staging\"").expect("deserialise staging");
        assert_eq!(parsed_prod, WebdAcmeProvider::LetsEncryptProd);
        assert_eq!(parsed_stg, WebdAcmeProvider::LetsEncryptStaging);

        // Negative: the `rename_all = "snake_case"` derivation
        // (`lets_encrypt_*`) must NOT parse. Catches the exact
        // refactor we're guarding against — a maintainer dropping
        // the per-variant renames thinking they're redundant.
        let bad_prod = serde_json::from_str::<WebdAcmeProvider>("\"lets_encrypt_prod\"");
        let bad_stg = serde_json::from_str::<WebdAcmeProvider>("\"lets_encrypt_staging\"");
        assert!(bad_prod.is_err(), "lets_encrypt_prod must not parse");
        assert!(bad_stg.is_err(), "lets_encrypt_staging must not parse");
    }

    // ── synthesize_listeners ──────────────────────────────────────────

    fn node_with_listeners(src: &str) -> NodeConfig {
        cosmix_mix::from_conf_mix_str(src).expect("parse listener config")
    }

    #[test]
    fn synthesize_listeners_empty_is_backcompat_single_wg() {
        // No `[webd] listener` array ⇒ one implicit internal `wg`
        // listener at wg_ip:port serving every host (byte-identical to
        // the pre-listener single bind).
        let nc =
            node_with_listeners("node: \"alpha\"\nwg_ip: \"192.0.2.5\"\nwebd: { port: 443 }\n");
        let hosts = vec!["a.example.org".to_string(), "b.example.org".to_string()];
        let out = nc
            .synthesize_listeners(&hosts, &HashSet::new())
            .expect("synthesize");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "wg");
        assert_eq!(out[0].bind, "192.0.2.5:443");
        assert!(!out[0].external);
        assert!(out[0].enabled);
        assert_eq!(out[0].hosts, hosts);
    }

    #[test]
    fn synthesize_listeners_enabled_defaults_true_when_omitted() {
        // A row omitting `enabled` must be on (serde(default) → true).
        let nc = node_with_listeners(
            "wg_ip: \"192.0.2.5\"\nwebd: { listener: [ \
             { id: \"wg\", bind: \"192.0.2.5:443\", vhosts: [\"a.example.org\"] } ] }\n",
        );
        let hosts = vec!["a.example.org".to_string()];
        let out = nc
            .synthesize_listeners(&hosts, &HashSet::new())
            .expect("synthesize");
        assert_eq!(out.len(), 1);
        assert!(out[0].enabled, "omitted enabled must default true");
    }

    #[test]
    fn synthesize_listeners_two_interface_split_ok() {
        let nc = node_with_listeners(
            "wg_ip: \"192.0.2.5\"\nwebd: { listener: [ \
             { id: \"wg\", bind: \"192.0.2.5:443\", vhosts: [\"int.example.org\"] }, \
             { id: \"pub\", bind: \"203.0.113.5:443\", external: true, vhosts: [\"ext.example.org\"] } ] }\n",
        );
        let hosts = vec!["int.example.org".to_string(), "ext.example.org".to_string()];
        let out = nc
            .synthesize_listeners(&hosts, &HashSet::new())
            .expect("synthesize");
        assert_eq!(out.len(), 2);
        let publ = out.iter().find(|l| l.id == "pub").unwrap();
        assert!(publ.external);
        assert_eq!(publ.bind, "203.0.113.5:443");
        assert_eq!(publ.hosts, vec!["ext.example.org".to_string()]);
    }

    #[test]
    fn synthesize_listeners_rejects_dup_id_and_bind() {
        let dup_id = node_with_listeners(
            "wg_ip: \"192.0.2.5\"\nwebd: { listener: [ \
             { id: \"x\", bind: \"192.0.2.5:443\", vhosts: [\"a.example.org\"] }, \
             { id: \"x\", bind: \"203.0.113.5:443\", vhosts: [\"b.example.org\"] } ] }\n",
        );
        assert!(
            dup_id
                .synthesize_listeners(
                    &["a.example.org".into(), "b.example.org".into()],
                    &HashSet::new()
                )
                .is_err()
        );

        let dup_bind = node_with_listeners(
            "wg_ip: \"192.0.2.5\"\nwebd: { listener: [ \
             { id: \"x\", bind: \"192.0.2.5:443\", vhosts: [\"a.example.org\"] }, \
             { id: \"y\", bind: \"192.0.2.5:443\", vhosts: [\"b.example.org\"] } ] }\n",
        );
        assert!(
            dup_bind
                .synthesize_listeners(
                    &["a.example.org".into(), "b.example.org".into()],
                    &HashSet::new()
                )
                .is_err()
        );
    }

    #[test]
    fn synthesize_listeners_rejects_wildcard_specific_same_port() {
        let nc = node_with_listeners(
            "wg_ip: \"192.0.2.5\"\nwebd: { listener: [ \
             { id: \"wg\", bind: \"192.0.2.5:443\", vhosts: [\"a.example.org\"] }, \
             { id: \"any\", bind: \"0.0.0.0:443\", external: true, vhosts: [\"b.example.org\"] } ] }\n",
        );
        let err = nc
            .synthesize_listeners(
                &["a.example.org".into(), "b.example.org".into()],
                &HashSet::new(),
            )
            .unwrap_err();
        assert!(format!("{err}").contains("wildcard"), "got: {err}");

        // Two wildcards on the same port (v4 + v6) also conflict — on a
        // dual-stack default `[::]:443` already covers v4, so the pair
        // would EADDRINUSE. A wildcard must be the only bind on its port.
        let dual_wild = node_with_listeners(
            "wg_ip: \"192.0.2.5\"\nwebd: { listener: [ \
             { id: \"v4\", bind: \"0.0.0.0:443\", external: true, vhosts: [\"a.example.org\"] }, \
             { id: \"v6\", bind: \"[::]:443\", external: true, vhosts: [\"b.example.org\"] } ] }\n",
        );
        assert!(
            dual_wild
                .synthesize_listeners(
                    &["a.example.org".into(), "b.example.org".into()],
                    &HashSet::new()
                )
                .is_err()
        );

        // Two specific IPs on the same port are fine — distinct
        // interfaces, no kernel conflict (the WG + public split).
        let two_specific = node_with_listeners(
            "wg_ip: \"192.0.2.5\"\nwebd: { listener: [ \
             { id: \"wg\", bind: \"192.0.2.5:443\", vhosts: [\"a.example.org\"] }, \
             { id: \"pub\", bind: \"203.0.113.5:443\", external: true, vhosts: [\"b.example.org\"] } ] }\n",
        );
        assert!(
            two_specific
                .synthesize_listeners(
                    &["a.example.org".into(), "b.example.org".into()],
                    &HashSet::new()
                )
                .is_ok()
        );
    }

    #[test]
    fn synthesize_listeners_dedups_bind_on_parsed_addr() {
        // Text variants of one IPv6 address resolve to the same socket;
        // must be caught as a duplicate, not slip two listeners onto it.
        let nc = node_with_listeners(
            "wg_ip: \"192.0.2.5\"\nwebd: { listener: [ \
             { id: \"a\", bind: \"[::1]:443\", vhosts: [\"a.example.org\"] }, \
             { id: \"b\", bind: \"[0:0:0:0:0:0:0:1]:443\", vhosts: [\"b.example.org\"] } ] }\n",
        );
        let err = nc
            .synthesize_listeners(
                &["a.example.org".into(), "b.example.org".into()],
                &HashSet::new(),
            )
            .unwrap_err();
        assert!(
            format!("{err}").contains("duplicate listener bind"),
            "got: {err}"
        );
    }

    #[test]
    fn synthesize_listeners_rejects_unknown_and_uncovered_hosts() {
        // A listener naming a host that is not a known vhost.
        let unknown = node_with_listeners(
            "wg_ip: \"192.0.2.5\"\nwebd: { listener: [ \
             { id: \"wg\", bind: \"192.0.2.5:443\", vhosts: [\"ghost.example.org\"] } ] }\n",
        );
        assert!(
            unknown
                .synthesize_listeners(&["a.example.org".into()], &HashSet::new())
                .is_err()
        );

        // A served host claimed by no listener (silent-drop guard).
        let uncovered = node_with_listeners(
            "wg_ip: \"192.0.2.5\"\nwebd: { listener: [ \
             { id: \"wg\", bind: \"192.0.2.5:443\", vhosts: [\"a.example.org\"] } ] }\n",
        );
        assert!(
            uncovered
                .synthesize_listeners(
                    &["a.example.org".into(), "b.example.org".into()],
                    &HashSet::new()
                )
                .is_err()
        );
    }

    #[test]
    fn synthesize_listeners_rejects_disabled_only_and_double_claim() {
        // Host served only by a disabled listener ⇒ unreachable ⇒ error.
        let disabled_only = node_with_listeners(
            "wg_ip: \"192.0.2.5\"\nwebd: { listener: [ \
             { id: \"wg\", bind: \"192.0.2.5:443\", enabled: false, vhosts: [\"a.example.org\"] } ] }\n",
        );
        assert!(
            disabled_only
                .synthesize_listeners(&["a.example.org".into()], &HashSet::new())
                .is_err()
        );

        // Host claimed by two listeners ⇒ ambiguous route ⇒ error.
        let double = node_with_listeners(
            "wg_ip: \"192.0.2.5\"\nwebd: { listener: [ \
             { id: \"wg\", bind: \"192.0.2.5:443\", vhosts: [\"a.example.org\"] }, \
             { id: \"pub\", bind: \"203.0.113.5:443\", external: true, vhosts: [\"a.example.org\"] } ] }\n",
        );
        assert!(
            double
                .synthesize_listeners(&["a.example.org".into()], &HashSet::new())
                .is_err()
        );
    }

    #[test]
    fn synthesize_listeners_failsoft_skips_disabled_host() {
        // B1 fail-soft: `ext.example.org` had a bad cert at resolve, so
        // it is absent from `all_hosts` but still named in the `pub`
        // listener's allowlist. With it in `disabled_hosts` the set
        // resolves — `pub` simply serves nothing — instead of failing
        // the whole node with "unknown vhost".
        let nc = node_with_listeners(
            "wg_ip: \"192.0.2.5\"\nwebd: { listener: [ \
             { id: \"wg\", bind: \"192.0.2.5:443\", vhosts: [\"int.example.org\"] }, \
             { id: \"pub\", bind: \"203.0.113.5:443\", external: true, vhosts: [\"ext.example.org\"] } ] }\n",
        );
        // Healthy host set after fail-soft drop: only the WG vhost.
        let healthy = vec!["int.example.org".to_string()];
        let mut disabled = HashSet::new();
        disabled.insert("ext.example.org".to_string());

        let out = nc
            .synthesize_listeners(&healthy, &disabled)
            .expect("disabled host must be skipped, not rejected");
        // Both listeners still resolve (the public one binds but serves
        // no vhost until the cert is repaired).
        assert_eq!(out.len(), 2);
        let publ = out.iter().find(|l| l.id == "pub").unwrap();
        assert!(publ.external);

        // Without the disabled set, the same config is a hard error —
        // the dangling allowlist entry is treated as a config typo.
        assert!(
            nc.synthesize_listeners(&healthy, &HashSet::new()).is_err(),
            "an un-disabled unknown vhost must still be rejected"
        );
    }

    #[test]
    fn synthesize_listeners_rejects_unknown_field() {
        // deny_unknown_fields closes the typo-silently-parses trap.
        let res: Result<NodeConfig, _> = cosmix_mix::from_conf_mix_str(
            "webd: { listener: [ { id: \"wg\", bind: \"192.0.2.5:443\", \
             vhost: [\"a.example.org\"] } ] }\n",
        );
        assert!(
            res.is_err(),
            "misspelled `vhost` (should be `vhosts`) must fail parse"
        );
    }
}
