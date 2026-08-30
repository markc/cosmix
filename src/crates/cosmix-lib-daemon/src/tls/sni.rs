//! SNI-routed TLS resolver shared across cosmix daemons.
//!
//! Originally landed in `cosmix-maild/src/tls.rs` for the maild
//! dual-bind TLS plan (Phase 1, commits `aa67a61` / `2197c32`). Lifted
//! to `cosmix-lib-daemon` in webd-vhosts Phase 1 commit 1 so webd and
//! maild share one implementation rather than diverging copies — the
//! anti-pattern security-relevant TLS code is most prone to growing.
//!
//! This module hosts the **multi-identity surface**: a rustls
//! [`ResolvesServerCert`] that picks a cert by SNI value at handshake
//! time, the runtime [`TlsIdentity`] type that pairs a loaded
//! [`CertifiedKey`] with the policy flags from
//! [`TlsIdentityConfig`], and the free function [`pick_identity`]
//! that encodes the selection policy *once*.
//!
//! The free function exists because rustls's post-handshake
//! `ServerConnection::server_name()` returns the *client's* SNI
//! value, not the certificate actually served — the session layer
//! cannot reverse-engineer which identity the resolver picked by
//! inspecting `server_name()`. The session layer therefore re-runs
//! [`pick_identity`] with the same inputs to drive the EHLO and IMAP
//! greeting hostnames (maild Phase 1 step 4). Keeping that policy in
//! one place is what prevents the resolver and the session layer
//! from drifting; the spec sketch is in `_doc/planned/maild-sni-
//! internal-ca.md` §1.
//!
//! Pre-maild-Phase-1 deployments used a single `tls_cert` / `tls_key`
//! pair handed to `rustls::ConfigBuilder::with_single_cert`. Step 1
//! collapsed that pair into a one-row implicit identity list
//! (`Config::resolve_tls_identities`); step 2 feeds that list into a
//! real [`SniCertResolver`]. Step 3 swaps the `with_single_cert` call
//! sites for `with_cert_resolver`.

use anyhow::{Context, Result};
use rustls::server::ResolvesServerCert;
use rustls::sign::CertifiedKey;
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;

use cosmix_config::node::TlsIdentityConfig;

/// A loaded TLS identity ready to be served.
///
/// Pairs a parsed cert+key chain with the policy flags from the
/// originating [`TlsIdentityConfig`]. The fields are public because
/// the maild session layer reaches in for `name` to populate the
/// EHLO / IMAP greeting hostname after the handshake completes
/// (maild Phase 1 step 4).
#[derive(Debug, Clone)]
pub struct TlsIdentity {
    /// SNI value this identity matches (the parsed `server_name`).
    pub name: String,
    /// Parsed leaf+chain+signer ready to hand to rustls.
    pub certified_key: Arc<CertifiedKey>,
    /// True if this identity is the SNI-mismatch fallback and the
    /// pre-TLS greeting hostname for plaintext / STARTTLS connections.
    pub is_default: bool,
    /// True if this identity is served when an implicit-TLS client
    /// sends no SNI at all.
    pub is_no_sni_fallback: bool,
}

/// SNI-routed cert resolver. Implements rustls's
/// [`ResolvesServerCert`] and shares the same selection policy with
/// the session layer via [`SniCertResolver::pick_for_sni`] /
/// [`pick_identity`].
///
/// Construction order matters only for the "no flags set anywhere"
/// fallback: when no identity carries `default = true` (or
/// `no_sni_fallback = true`), the *first* identity in config order
/// becomes both. This preserves the byte-identical behavior of the
/// legacy `tls_cert` / `tls_key` pair (collapsed in maild step 1 to a
/// single-row identity with both flags already set, so the fallback
/// is never exercised on legacy configs — but the same code path
/// covers a forgetful operator who writes one row with neither flag).
#[derive(Debug)]
pub struct SniCertResolver {
    identities: Arc<Vec<TlsIdentity>>,
    by_name: HashMap<String, usize>,
    default_idx: Option<usize>,
    no_sni_idx: Option<usize>,
    strict_sni: bool,
}

impl SniCertResolver {
    /// Build a resolver from a validated identity list (i.e. the
    /// result of `Config::resolve_tls_identities()` after
    /// `Config::validate_tls()` has passed).
    ///
    /// Loads each cert / key pair from disk. Duplicate `server_name`
    /// values are a hard error — a `[[tls.identity]]` collision
    /// silently shadowing one of the rows is a deployment footgun
    /// that has to be loud at startup.
    pub fn from_config(identities: &[TlsIdentityConfig], strict_sni: bool) -> Result<Self> {
        let mut runtime: Vec<TlsIdentity> = Vec::with_capacity(identities.len());
        for ident in identities {
            let certified_key = load_certified_key(&ident.cert, &ident.key)
                .with_context(|| format!("loading TLS identity {:?}", ident.server_name))?;
            // Normalize the configured `server_name` to lowercase so
            // SNI lookup is case-insensitive (DNS names are case-
            // insensitive per RFC 4343; some MUAs / proxies will send
            // mixed-case SNI). The display-form `name` is the
            // normalized one — it's what step 4 will surface in the
            // EHLO / IMAP greeting, and lowercase is the universally
            // safe rendering.
            runtime.push(TlsIdentity {
                name: ident.server_name.to_ascii_lowercase(),
                certified_key: Arc::new(certified_key),
                is_default: ident.default,
                is_no_sni_fallback: ident.no_sni_fallback,
            });
        }

        let mut by_name = HashMap::with_capacity(runtime.len());
        for (i, id) in runtime.iter().enumerate() {
            if by_name.insert(id.name.clone(), i).is_some() {
                anyhow::bail!(
                    "duplicate [[tls.identity]] server_name {:?} (collisions are case-insensitive)",
                    id.name
                );
            }
        }

        // Explicit flags win. The "first identity becomes both
        // default + no_sni_fallback" rule fires ONLY when neither
        // flag is set on any row — that's the legacy collapse case
        // and the hand-written-config-without-flags case. If the
        // operator set `default=true` on row N, leaving no_sni_idx
        // as `None` lets `pick_identity(None)` fall through to
        // `default_idx` via `.or(default_idx)` — which is what they
        // meant. Eagerly defaulting `no_sni_idx = Some(0)` would
        // ignore an explicit `default=true` on a non-first row and
        // silently serve the wrong identity for no-SNI connections.
        let any_default = runtime.iter().any(|i| i.is_default);
        let any_no_sni = runtime.iter().any(|i| i.is_no_sni_fallback);
        let (default_idx, no_sni_idx) = if !runtime.is_empty() && !any_default && !any_no_sni {
            (Some(0), Some(0))
        } else {
            (
                runtime.iter().position(|i| i.is_default),
                runtime.iter().position(|i| i.is_no_sni_fallback),
            )
        };

        Ok(Self {
            identities: Arc::new(runtime),
            by_name,
            default_idx,
            no_sni_idx,
            strict_sni,
        })
    }

    /// Number of loaded identities.
    pub fn len(&self) -> usize {
        self.identities.len()
    }

    /// Whether the resolver carries any identities at all. An empty
    /// resolver never resolves a cert (rustls handshake fails).
    pub fn is_empty(&self) -> bool {
        self.identities.is_empty()
    }

    /// View of the loaded identities, in config order.
    pub fn identities(&self) -> &[TlsIdentity] {
        &self.identities
    }

    /// Whether strict-SNI mode is active. Exposed for the
    /// `maild.props.list namespace="tls_identities"` snapshot
    /// (maild Phase 1 step 5).
    pub fn strict_sni(&self) -> bool {
        self.strict_sni
    }

    /// Identity flagged `default = true` (or the "first becomes
    /// both" fallback when neither flag is set anywhere). Distinct
    /// from `pick_for_sni(None)`: `pick_for_sni(None)` returns the
    /// `no_sni_fallback` identity (the cert served on no-SNI
    /// handshakes), which is the right answer for *post-handshake*
    /// recovery when a TLS-terminated client omitted SNI. This
    /// method returns the `default = true` identity, which the
    /// maild plan §5 reserves for *pre-TLS* application-layer
    /// greetings — SMTP `220` on port 25 before STARTTLS, where no
    /// TLS layer exists yet to consult. In single-identity configs
    /// the two coincide via the "first becomes both" rule;
    /// multi-identity configs (e.g. external FQDN as `default`,
    /// substrate-internal `*.bus` as `no_sni_fallback`) can split
    /// them deliberately.
    pub fn default_identity(&self) -> Option<&TlsIdentity> {
        self.default_idx.and_then(|i| self.identities.get(i))
    }

    /// Re-apply the resolver's selection policy outside the rustls
    /// handshake path. The maild session layer calls this with
    /// `ServerConnection::server_name()` to drive the post-handshake
    /// EHLO / IMAP greeting hostname. The SNI value is lowercased
    /// before lookup; configured `server_name`s are already
    /// normalized at `from_config` time.
    pub fn pick_for_sni(&self, sni: Option<&str>) -> Option<&TlsIdentity> {
        let normalized = sni.map(|s| s.to_ascii_lowercase());
        let idx = pick_identity(
            &self.by_name,
            self.default_idx,
            self.no_sni_idx,
            self.strict_sni,
            normalized.as_deref(),
        )?;
        self.identities.get(idx)
    }
}

impl ResolvesServerCert for SniCertResolver {
    fn resolve(&self, hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let normalized = hello.server_name().map(|s| s.to_ascii_lowercase());
        let idx = pick_identity(
            &self.by_name,
            self.default_idx,
            self.no_sni_idx,
            self.strict_sni,
            normalized.as_deref(),
        )?;
        Some(self.identities[idx].certified_key.clone())
    }
}

/// SNI selection policy. Pure function — takes the resolver's
/// indexed view of the identity list plus the optional SNI value the
/// client sent (or didn't), returns the chosen identity's index.
///
/// Policy:
///
/// - `sni = Some(name)`: exact-match lookup wins. On miss, fall
///   through to `default_idx` unless `strict_sni` is set, in which
///   case the handshake fails.
/// - `sni = None`: prefer the `no_sni_fallback` identity, falling
///   back to `default_idx`. With `strict_sni = true`, no-SNI
///   handshakes fail outright.
///
/// Empty identity list → always `None` (caller's `by_name` will be
/// empty and `default_idx` / `no_sni_idx` `None`).
pub fn pick_identity(
    by_name: &HashMap<String, usize>,
    default_idx: Option<usize>,
    no_sni_idx: Option<usize>,
    strict_sni: bool,
    sni: Option<&str>,
) -> Option<usize> {
    match sni {
        Some(name) => by_name
            .get(name)
            .copied()
            .or(if strict_sni { None } else { default_idx }),
        None if strict_sni => None,
        None => no_sni_idx.or(default_idx),
    }
}

/// Read a PEM cert chain + private key from disk and assemble a
/// `CertifiedKey`. Errors carry the offending path so the operator
/// can find the wrong file from `journalctl` alone.
fn load_certified_key(cert_path: &str, key_path: &str) -> Result<CertifiedKey> {
    let cert_data = fs::read(cert_path).with_context(|| format!("reading cert {cert_path}"))?;
    let key_data = fs::read(key_path).with_context(|| format!("reading key {key_path}"))?;

    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut &cert_data[..])
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("parsing cert {cert_path}"))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {cert_path}");
    }

    let key = rustls_pemfile::private_key(&mut &key_data[..])
        .with_context(|| format!("parsing key {key_path}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {key_path}"))?;

    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .with_context(|| format!("building signing key from {key_path}"))?;

    Ok(CertifiedKey::new(certs, signing_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure policy tests use synthetic maps — no rustls fixtures —
    // because `pick_identity` is intentionally CertifiedKey-free.

    fn map_of(entries: &[(&str, usize)]) -> HashMap<String, usize> {
        entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect()
    }

    #[test]
    fn pick_identity_sni_exact_match_wins() {
        let by_name = map_of(&[("host.lan", 0), ("host.bus", 1)]);
        assert_eq!(
            pick_identity(&by_name, Some(0), Some(0), false, Some("host.bus")),
            Some(1)
        );
    }

    #[test]
    fn pick_identity_sni_miss_falls_to_default() {
        let by_name = map_of(&[("host.lan", 0)]);
        assert_eq!(
            pick_identity(&by_name, Some(0), Some(0), false, Some("unknown")),
            Some(0)
        );
    }

    #[test]
    fn pick_identity_sni_miss_strict_returns_none() {
        let by_name = map_of(&[("host.lan", 0)]);
        assert_eq!(
            pick_identity(&by_name, Some(0), Some(0), true, Some("unknown")),
            None
        );
    }

    #[test]
    fn pick_identity_no_sni_prefers_no_sni_fallback() {
        let by_name = map_of(&[("host.lan", 0), ("host.bus", 1)]);
        // default=0, no_sni=1 → no-SNI handshake picks 1
        assert_eq!(
            pick_identity(&by_name, Some(0), Some(1), false, None),
            Some(1)
        );
    }

    #[test]
    fn pick_identity_no_sni_falls_back_to_default() {
        let by_name = map_of(&[("host.lan", 0)]);
        // no_sni_idx absent → default
        assert_eq!(pick_identity(&by_name, Some(0), None, false, None), Some(0));
    }

    #[test]
    fn pick_identity_no_sni_strict_returns_none() {
        let by_name = map_of(&[("host.lan", 0)]);
        assert_eq!(pick_identity(&by_name, Some(0), Some(0), true, None), None);
    }

    #[test]
    fn pick_identity_empty_map_no_indices_returns_none() {
        let by_name: HashMap<String, usize> = HashMap::new();
        assert_eq!(pick_identity(&by_name, None, None, false, None), None);
        assert_eq!(
            pick_identity(&by_name, None, None, false, Some("anything")),
            None
        );
    }

    // Resolver-construction tests use a real rcgen-generated cert
    // pair so the full PEM → CertifiedKey path is exercised. The
    // cert content is irrelevant to selection policy; what matters
    // is that `from_config` accepts plausible material and refuses
    // implausible material.

    fn write_self_signed_pair(dir: &std::path::Path, stem: &str) -> (String, String) {
        let cert = rcgen::generate_simple_self_signed(vec![stem.to_string()]).expect("rcgen");
        let cert_path = dir.join(format!("{stem}.crt"));
        let key_path = dir.join(format!("{stem}.key"));
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();
        (
            cert_path.to_string_lossy().into_owned(),
            key_path.to_string_lossy().into_owned(),
        )
    }

    fn install_default_crypto_provider_once() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    #[test]
    fn from_config_loads_single_identity() {
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = write_self_signed_pair(dir.path(), "lan");
        let cfg = vec![TlsIdentityConfig {
            server_name: "host.lan".into(),
            cert,
            key,
            default: true,
            no_sni_fallback: true,
        }];
        let r = SniCertResolver::from_config(&cfg, false).expect("build");
        assert_eq!(r.len(), 1);
        assert!(r.pick_for_sni(Some("host.lan")).is_some());
        assert_eq!(r.pick_for_sni(Some("host.lan")).unwrap().name, "host.lan");
    }

    #[test]
    fn from_config_no_flags_treats_first_as_both_default_and_no_sni() {
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert_a, key_a) = write_self_signed_pair(dir.path(), "a");
        let (cert_b, key_b) = write_self_signed_pair(dir.path(), "b");
        let cfg = vec![
            TlsIdentityConfig {
                server_name: "a.example".into(),
                cert: cert_a,
                key: key_a,
                default: false,
                no_sni_fallback: false,
            },
            TlsIdentityConfig {
                server_name: "b.example".into(),
                cert: cert_b,
                key: key_b,
                default: false,
                no_sni_fallback: false,
            },
        ];
        let r = SniCertResolver::from_config(&cfg, false).expect("build");
        // No SNI / no explicit flags → first identity ("a.example")
        // is treated as both no-SNI fallback and default.
        let no_sni = r.pick_for_sni(None).expect("no-SNI pick");
        assert_eq!(no_sni.name, "a.example");
        let mismatch = r.pick_for_sni(Some("nowhere.example")).expect("default");
        assert_eq!(mismatch.name, "a.example");
    }

    #[test]
    fn from_config_explicit_flags_respected_across_rows() {
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert_lan, key_lan) = write_self_signed_pair(dir.path(), "lan");
        let (cert_bus, key_bus) = write_self_signed_pair(dir.path(), "bus");
        let cfg = vec![
            TlsIdentityConfig {
                server_name: "host.lan".into(),
                cert: cert_lan,
                key: key_lan,
                default: true,
                no_sni_fallback: false,
            },
            TlsIdentityConfig {
                server_name: "host.bus".into(),
                cert: cert_bus,
                key: key_bus,
                default: false,
                no_sni_fallback: true,
            },
        ];
        let r = SniCertResolver::from_config(&cfg, false).expect("build");
        // SNI miss → default (host.lan)
        assert_eq!(r.pick_for_sni(Some("nope")).unwrap().name, "host.lan");
        // No SNI → no_sni_fallback (host.bus), not default
        assert_eq!(r.pick_for_sni(None).unwrap().name, "host.bus");
    }

    #[test]
    fn from_config_duplicate_server_name_is_error() {
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert_a, key_a) = write_self_signed_pair(dir.path(), "a");
        let (cert_b, key_b) = write_self_signed_pair(dir.path(), "b");
        let cfg = vec![
            TlsIdentityConfig {
                server_name: "dup.example".into(),
                cert: cert_a,
                key: key_a,
                default: true,
                no_sni_fallback: true,
            },
            TlsIdentityConfig {
                server_name: "dup.example".into(),
                cert: cert_b,
                key: key_b,
                default: false,
                no_sni_fallback: false,
            },
        ];
        let err = SniCertResolver::from_config(&cfg, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("duplicate"),
            "expected duplicate-server_name error, got: {err:#}"
        );
    }

    #[test]
    fn from_config_missing_cert_file_is_error() {
        install_default_crypto_provider_once();
        let cfg = vec![TlsIdentityConfig {
            server_name: "host".into(),
            cert: "/nonexistent/host.crt".into(),
            key: "/nonexistent/host.key".into(),
            default: true,
            no_sni_fallback: true,
        }];
        let err = SniCertResolver::from_config(&cfg, false).unwrap_err();
        let s = format!("{err:#}");
        assert!(
            s.contains("/nonexistent/host.crt") || s.contains("TLS identity"),
            "error message should name the offending file: {s}"
        );
    }

    #[test]
    fn from_config_default_on_non_first_row_handles_no_sni_via_default() {
        // Regression: a row carrying `default=true` but no
        // `no_sni_fallback` must serve no-SNI handshakes from the
        // *default* row, not from row 0. The earlier draft eagerly
        // set `no_sni_idx = Some(0)` whenever no row had the no-SNI
        // flag, which would silently ignore the explicit default.
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert_a, key_a) = write_self_signed_pair(dir.path(), "a");
        let (cert_b, key_b) = write_self_signed_pair(dir.path(), "b");
        let cfg = vec![
            TlsIdentityConfig {
                server_name: "a.example".into(),
                cert: cert_a,
                key: key_a,
                default: false,
                no_sni_fallback: false,
            },
            TlsIdentityConfig {
                server_name: "b.example".into(),
                cert: cert_b,
                key: key_b,
                default: true,
                no_sni_fallback: false,
            },
        ];
        let r = SniCertResolver::from_config(&cfg, false).expect("build");
        // No SNI → should fall through to default (b.example), NOT
        // row 0 (a.example).
        let no_sni = r.pick_for_sni(None).expect("no-SNI pick");
        assert_eq!(no_sni.name, "b.example");
        // SNI miss → default
        assert_eq!(r.pick_for_sni(Some("nope")).unwrap().name, "b.example");
    }

    #[test]
    fn from_config_sni_lookup_is_case_insensitive() {
        // DNS hostnames are case-insensitive. Mixed-case SNI from a
        // client must still match a lowercase-configured identity,
        // and vice versa — without this, a mixed-case SNI under
        // `strict_sni = true` would spuriously fail the handshake.
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = write_self_signed_pair(dir.path(), "host");
        let cfg = vec![TlsIdentityConfig {
            // Configured with mixed case on purpose.
            server_name: "Host.Example".into(),
            cert,
            key,
            default: true,
            no_sni_fallback: true,
        }];
        let r = SniCertResolver::from_config(&cfg, true).expect("build");
        // Identity is stored in lowercase form.
        assert_eq!(r.identities()[0].name, "host.example");
        // Lookup matches regardless of incoming case.
        assert!(r.pick_for_sni(Some("host.example")).is_some());
        assert!(r.pick_for_sni(Some("HOST.EXAMPLE")).is_some());
        assert!(r.pick_for_sni(Some("Host.Example")).is_some());
    }

    #[test]
    fn from_config_duplicate_server_name_collides_case_insensitively() {
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert_a, key_a) = write_self_signed_pair(dir.path(), "a");
        let (cert_b, key_b) = write_self_signed_pair(dir.path(), "b");
        let cfg = vec![
            TlsIdentityConfig {
                server_name: "dup.example".into(),
                cert: cert_a,
                key: key_a,
                default: true,
                no_sni_fallback: true,
            },
            TlsIdentityConfig {
                // Same name, mixed case. Must still collide.
                server_name: "DUP.example".into(),
                cert: cert_b,
                key: key_b,
                default: false,
                no_sni_fallback: false,
            },
        ];
        let err = SniCertResolver::from_config(&cfg, false).unwrap_err();
        assert!(
            format!("{err:#}").contains("duplicate"),
            "expected duplicate-server_name error, got: {err:#}"
        );
    }

    #[test]
    fn resolver_strict_sni_blocks_mismatch_and_no_sni() {
        install_default_crypto_provider_once();
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = write_self_signed_pair(dir.path(), "lan");
        let cfg = vec![TlsIdentityConfig {
            server_name: "host.lan".into(),
            cert,
            key,
            default: true,
            no_sni_fallback: true,
        }];
        let r = SniCertResolver::from_config(&cfg, true).expect("build");
        assert!(
            r.pick_for_sni(Some("host.lan")).is_some(),
            "exact match still serves"
        );
        assert!(
            r.pick_for_sni(Some("nope")).is_none(),
            "strict blocks mismatch"
        );
        assert!(r.pick_for_sni(None).is_none(), "strict blocks no-SNI");
    }
}
