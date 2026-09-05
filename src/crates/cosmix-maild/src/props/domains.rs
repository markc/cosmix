//! SPEC 12 property-substrate adoption for `maild.domains`.
//!
//! Collection-cardinality namespace owning the authoritative per-vhost
//! row. Phase 2 (this commit) ships the schema, validation, and
//! registration. The reconciliation diagnostic lands in commit 2; the
//! `cosmix-maild domain add` CLI lands in commit 3. Consumers (RCPT TO
//! gate, HELO selection, DKIM signer composition, TLS resolver) plumb
//! in over Phases 3–5; in Phase 2 the namespace is legible-only.
//!
//! ## Lifecycle
//!
//! `Simple` — no provisioning side-effects. `before_set` validates the
//! effective value (Replace requires the full required set; Patch
//! merges over the prior row, then re-validates). `before_delete` is
//! permissive at the hook layer; the substrate runtime still enforces
//! `if_version` (the namespace runs with `require_version=true`) and
//! applies its default `SoftDelete` mode. `after_set` is the trait
//! default (no-op) until later phases attach consumers.
//!
//! ## Cardinality
//!
//! `Collection { primary_key_field: "domain" }` — key is the FQDN,
//! lowercased. The substrate runtime enforces
//! `value.domain == key.key` on Replace and on Patch-creates before
//! the hook ever runs (`crates/cosmix-lib-props-store/src/runtime.rs`
//! `validate_primary_key`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::Result;
use cosmix_props::bus::mutation::PropsRouter;
use cosmix_props::capability::{Capability, CapabilitySet};
use cosmix_props::hooks::{HookCtx, HookError, HookFuture, HookHandler, Hooks};
use cosmix_props::namespace::{
    AuthPolicy, Cardinality, FieldSchema, FieldType, NamespaceLifecycle, NamespaceName,
    NamespaceSpec, PeerIdentity, PropertySchema, StorageBackendKind,
};
use cosmix_props::record::Record;
use cosmix_props::runtime::Runtime;
use cosmix_props::sqlite::{JsonValuesMapping, SqliteStore};
use cosmix_props::store::{MergeMode, PropertyStore};
use cosmix_props::value::PropValue;

/// Unqualified namespace name; fully qualified is `maild.domains`.
pub const NAMESPACE: &str = "domains";

/// Substrate-required primary key field per `Cardinality::Collection`.
/// Every Replace body and every Patch-create body must carry
/// `domain: "<key>"`; the substrate's `validate_primary_key` rejects
/// upstream if it does not.
pub const PRIMARY_KEY_FIELD: &str = "domain";

/// `before_set` rejects `MergeMode::Replace` requests that omit any
/// required field. The substrate stores the submitted object
/// verbatim (no default materialisation), so a partial Replace would
/// leave the row missing fields the consumer needs.
///
/// **Exception**: `cert_source` (added in Path β-1) is absent-tolerant
/// — `validate_shape` treats a missing key as equivalent to `Null` so
/// pre-β-1 persisted rows continue to validate. `defaults_value` still
/// emits explicit `Null` for fresh writes; the carve only affects
/// legacy rows whose merged Patch body inherited the absence.
pub const REPLACE_REQUIRES_FULL_BODY_PREFIX: &str = "domains_replace_requires_full_body:";

/// `before_set` rejects `MergeMode::Patch` against an absent row.
/// Without a prior, Patch has nothing to merge over; operators
/// initialising a row use `Replace` with the full `defaults_value`.
pub const PATCH_REQUIRES_EXISTING_ROW_PREFIX: &str = "domains_patch_requires_existing_row:";

/// `before_set` rejects writes whose key is not a well-formed FQDN
/// (uppercase keys included — substrate keys are exact-match
/// identifiers; auto-lowercasing would mask operator typos in
/// `maild.props.list` output).
pub const INVALID_FQDN_KEY_PREFIX: &str = "domains_invalid_fqdn_key:";

/// `before_set` rejects per-field shape / type / membership errors.
pub const INVALID_FIELD_PREFIX: &str = "domains_invalid_field:";

/// `before_set` rejects cross-field invariant violations
/// (e.g. `dkim_selector_active` not in `dkim_selectors`).
pub const CROSS_FIELD_PREFIX: &str = "domains_cross_field:";

pub fn namespace_name() -> NamespaceName {
    NamespaceName::new(NAMESPACE).expect("constant namespace name is valid")
}

/// Known `role` enum variants. `"relay"` is **not** included — the
/// umbrella's relay row needs downstream routing fields the substrate
/// has no shape for in Phase 2; admitting it now would let operators
/// write `enabled: true, role: "relay"` rows Phase 3 cannot honour
/// safely. A later phase adds the variant together with the routing
/// fields it implies.
pub fn role_variants() -> Vec<String> {
    vec!["authoritative".into(), "backup_mx".into()]
}

pub fn spf_policy_variants() -> Vec<String> {
    vec!["strict".into(), "lenient".into(), "off".into()]
}

pub fn dmarc_policy_variants() -> Vec<String> {
    vec!["enforce".into(), "monitor".into(), "off".into()]
}

/// `cert_source.kind` enum variants. `"manual"` = operator-deployed PEM
/// (current behaviour). `"webd"` = bridge-promoted from a webd vhost
/// per `_doc/planned/maild-webd-cert-bridge.md`. Path β-1 lands the
/// shape; the bridge code (β-2/β-3) is the consumer.
pub fn cert_source_kind_variants() -> Vec<String> {
    vec!["manual".into(), "webd".into()]
}

/// Build the substrate-shape value for a fresh row keyed `domain`.
/// Caller (CLI, tests, later-phase verbs) uses this to assemble a
/// full Replace body without hand-keying every field.
///
/// Every `FieldType::Option` field is `PropValue::Null`; list fields
/// are empty; the substrate-required `domain` field equals the
/// caller-supplied key. The substrate does **not** materialise
/// schema defaults at commit time, so this helper is the only place
/// the default shape lives.
pub fn defaults_value(domain: &str) -> PropValue {
    let mut m = BTreeMap::new();
    m.insert("domain".into(), PropValue::String(domain.into()));
    m.insert("enabled".into(), PropValue::Bool(true));
    m.insert("role".into(), PropValue::String("authoritative".into()));
    m.insert("primary_hostname".into(), PropValue::Null);
    m.insert("mx_targets".into(), PropValue::List(Vec::new()));
    m.insert("relay_recipients".into(), PropValue::List(Vec::new()));
    m.insert("helo_identity".into(), PropValue::Null);
    m.insert("message_id_host".into(), PropValue::Null);
    m.insert("dkim_selector_active".into(), PropValue::Null);
    m.insert("dkim_selectors".into(), PropValue::List(Vec::new()));
    m.insert("tls_identity_name".into(), PropValue::Null);
    m.insert("cert_source".into(), PropValue::Null);
    m.insert("spf_policy".into(), PropValue::String("lenient".into()));
    m.insert("dmarc_policy".into(), PropValue::String("monitor".into()));
    m.insert("autoconfig_published".into(), PropValue::Bool(false));
    PropValue::Object(m)
}

/// The set of schema field names. Used by the unknown-field guard
/// (`JsonValuesMapping` stores arbitrary top-level keys verbatim, so
/// the hook is the only line of defence against typo'd fields).
fn schema_field_names() -> &'static [&'static str] {
    &[
        "domain",
        "enabled",
        "role",
        "primary_hostname",
        "mx_targets",
        "relay_recipients",
        "helo_identity",
        "message_id_host",
        "dkim_selector_active",
        "dkim_selectors",
        "tls_identity_name",
        "cert_source",
        "spf_policy",
        "dmarc_policy",
        "autoconfig_published",
    ]
}

pub fn schema() -> PropertySchema {
    PropertySchema::new(vec![
        FieldSchema {
            name: "domain".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "FQDN; must equal the row key. Substrate-required primary key.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "enabled".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(true)),
            secret: false,
            help: "Phase 3 RCPT TO gate; ignored by Phase 2 (legible-only).".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "role".into(),
            ty: FieldType::Enum {
                variants: role_variants(),
            },
            default: Some(PropValue::String("authoritative".into())),
            secret: false,
            help: "Phase 3 routing role. `relay` deferred to a later phase.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "primary_hostname".into(),
            ty: FieldType::Option {
                inner: Box::new(FieldType::String),
            },
            default: Some(PropValue::Null),
            secret: false,
            help: "Phase 3 HELO/Message-ID source; Phase 5 TLS hint. Null → consumer falls back to row key.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "mx_targets".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::String),
            },
            default: Some(PropValue::List(Vec::new())),
            secret: false,
            help: "Phase 3 backup_mx routing / Phase 6 autoconfig. Empty → consumer falls back to [resolved primary_hostname].".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "relay_recipients".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::String),
            },
            default: Some(PropValue::List(Vec::new())),
            secret: false,
            help: "Phase 3 backup_mx RCPT allowlist (bare local-parts or full addresses).".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "helo_identity".into(),
            ty: FieldType::Option {
                inner: Box::new(FieldType::String),
            },
            default: Some(PropValue::Null),
            secret: false,
            help: "Phase 3 outbound HELO. Null → consumer falls back to effective primary_hostname.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "message_id_host".into(),
            ty: FieldType::Option {
                inner: Box::new(FieldType::String),
            },
            default: Some(PropValue::Null),
            secret: false,
            help: "Phase 3 Message-ID host part. Null → consumer falls back to effective primary_hostname.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "dkim_selector_active".into(),
            ty: FieldType::Option {
                inner: Box::new(FieldType::String),
            },
            default: Some(PropValue::Null),
            secret: false,
            help: "Phase 4 active DKIM selector. Null → no active signer for this domain.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "dkim_selectors".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::String),
            },
            default: Some(PropValue::List(Vec::new())),
            secret: false,
            help: "Phase 4 selector rotation history.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "tls_identity_name".into(),
            ty: FieldType::Option {
                inner: Box::new(FieldType::String),
            },
            default: Some(PropValue::Null),
            secret: false,
            help: "Phase 5 SNI binding. Null → no SNI binding declared.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "cert_source".into(),
            ty: FieldType::Option {
                inner: Box::new(FieldType::Struct {
                    fields: vec![
                        FieldSchema {
                            name: "kind".into(),
                            ty: FieldType::Enum {
                                variants: cert_source_kind_variants(),
                            },
                            default: None,
                            secret: false,
                            help: "manual = operator-deployed PEM (current behaviour); \
                                   webd = bridge-promoted from a webd vhost."
                                .into(),
                            since: None,
                            until: None,
                            validators: Vec::new(),
                        },
                        FieldSchema {
                            name: "vhost_fqdn".into(),
                            ty: FieldType::Option {
                                inner: Box::new(FieldType::String),
                            },
                            default: Some(PropValue::Null),
                            secret: false,
                            help: "Required and FQDN-shaped when kind=webd; \
                                   absent/Null when kind=manual."
                                .into(),
                            since: None,
                            until: None,
                            validators: Vec::new(),
                        },
                    ],
                }),
            },
            default: Some(PropValue::Null),
            secret: false,
            help: "Issuer-of-PEM binding. Null = manual operator deployment \
                   (current behaviour). See _doc/planned/maild-webd-cert-bridge.md."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "spf_policy".into(),
            ty: FieldType::Enum {
                variants: spf_policy_variants(),
            },
            default: Some(PropValue::String("lenient".into())),
            secret: false,
            help: "Phase 3 inbound SPF check intensity.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "dmarc_policy".into(),
            ty: FieldType::Enum {
                variants: dmarc_policy_variants(),
            },
            default: Some(PropValue::String("monitor".into())),
            secret: false,
            help: "Phase 3 inbound DMARC enforcement.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "autoconfig_published".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(false)),
            secret: false,
            help: "Phase 6 — autoconfig endpoint published?".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
    ])
}

/// SPEC 12 §7.2 [`AuthPolicy`] — Phase 2 grants every property
/// capability to every reachable peer (WireGuard `/24` trust domain,
/// same posture as Phase 1/2/3 of the prior SPEC 12 carvings).
///
/// **Hardening obligation — Phases 3/4/5 MUST narrow this before
/// their consumers go live.** Phase 2 rows are inert (no consumer
/// reads them), so a hostile peer cannot change daemon behaviour by
/// writing here yet. From Phase 3 on, the same writes steer RCPT TO
/// acceptance, outbound DKIM signing, and TLS certificate selection;
/// each consumer-cutover commit MUST land an `auth_policy` narrowing
/// in the same diff. Recorded in the Phase 2 plan and on the umbrella
/// roadmap so this is not lost.
///
/// **5a-commit-2 carry-over (accepted risk, to be closed in
/// 5a-commit-2b).** The `maild.tls.reload` verb shipped in
/// 5a-commit-2 makes `tls_identity_name` writes load-bearing for
/// live TLS resolution, so the obligation above applies, but the
/// narrowing did not land in the same diff. The actor visible at
/// the substrate write boundary for an Bus-driven write is derived
/// from `PeerIdentity.service_name` (i.e. `cmd.from`), which noded
/// strips for anonymous callers — including the legitimate
/// `maild domain set` CLI path — so an `Actor::operator(...)` hook
/// gate would reject the only working write path with no replacement
/// primitive yet in place (the operator-form distinction over Bus is
/// the subject of `src/_doc/planned/cosmix-cross-mesh-authz.md`).
///
/// Threat model under this gap: a WG-trusted peer can plant
/// `tls_identity_name = "<name>"` on a domain row, and after the next
/// `maild.tls.reload` that domain will be served using the cert at
/// `<tls_root>/<name>.cert.pem`. The substrate does not write PEMs
/// (operator action does), so the redirect is bounded by what the
/// operator has already deployed under `<tls_root>`. The worst case
/// is not key exfiltration; it is unauthorised live TLS policy
/// control — specifically, if `<tls_root>` contains a wildcard or
/// multi-SAN cert whose SANs match a target hostname, a peer can
/// redirect that hostname to serve a *validating* but
/// operator-unintended cert.
///
/// **Disposition (2026-05-26).** Acceptable bridging risk; the
/// narrowing remains open. The two original options — field-level
/// hook gate or verb-level capability check on `maild.tls.reload` —
/// both depend on a substrate primitive that does not exist yet:
///
///   * Field-level: blocked on the operator-actor distinction over
///     Bus (`src/_doc/planned/cosmix-cross-mesh-authz.md`).
///   * Verb-level: `cosmix-maild-auth` carries no access-control
///     vocabulary (it is mail authentication — SPF, DKIM, DMARC, ARC,
///     iprev), and the sibling Bus verbs (`maild.dkim.*`,
///     `maild.rules.*`, `maild.accounts.*`, `maild.search.*`) are
///     direct WG-trust surfaces — with audit cause where they perform
///     substrate mutations (e.g. `maild.dkim.*` embeds `cmd.from`),
///     but no shared maild Bus capability surface to plug into. A
///     meaningful cap check therefore implies introducing a new authz
///     surface in maild, not plumbing an existing one.
///
/// Until cross-mesh-authz lands, `maild.tls.reload` is gated by the
/// WG trust boundary alone — the same posture as the rest of
/// `maild.*`. Two near-options have been
/// considered and **rejected** so a future reviewer doesn't
/// re-propose them:
///
///   * Refusing reload when `cmd.from` is non-empty (so only the
///     unauthenticated local-CLI path triggers swaps): `cmd.from`
///     emptiness is not a capability boundary — noded does not
///     cryptographically bind "empty `from`" to local-only — and it
///     would block the planned webd → maild cert bridge (β-2) from
///     driving a reload after a certificate refresh.
///   * A config-driven `tls.reload_allowlist` of peer service
///     names: a new policy surface that depends on `cmd.from` being
///     authenticated and non-spoofable by noded, which is itself
///     part of the cross-mesh-authz scope.
///
/// The right shape for the eventual narrowing lands with
/// cross-mesh-authz; we deliberately do not paper over the gap with
/// a weaker check that looks like a fix.
pub fn auth_policy() -> AuthPolicy {
    fn resolve(_peer: &PeerIdentity) -> CapabilitySet {
        [
            "props.read:maild.domains",
            "props.write:maild.domains",
            "props.describe:maild.domains:public",
            "props.describe:maild.domains:full",
            "props.audit:maild.domains",
        ]
        .into_iter()
        .map(|s| Capability::new(s).expect("non-empty capability"))
        .collect()
    }
    AuthPolicy::new(resolve)
}

pub fn spec(hooks: Hooks) -> NamespaceSpec {
    let mut s = NamespaceSpec::new(
        namespace_name(),
        schema(),
        Cardinality::Collection {
            primary_key_field: PRIMARY_KEY_FIELD.into(),
        },
        StorageBackendKind::SqliteTable {
            table: "__props_values".into(),
        },
    );
    s.lifecycle = NamespaceLifecycle::Simple;
    // `require_version=true` closes the hook/commit TOCTOU window for
    // cross-field invariants under Patch interleaving. Example: Patch A
    // removes selector "2026a" from `dkim_selectors`; Patch B sets
    // `dkim_selector_active = "2026a"`. Each Patch individually
    // validates against the prior row (`ctx.old`), but the merged final
    // state violates the cross-field rule. CAS forces the second writer
    // to observe the first commit and re-validate against it. Replaces
    // don't merge and so are individually safe — the rationale is
    // Patch-driven.
    s.require_version = true;
    s.auth = auth_policy();
    s.hooks = hooks;
    s
}

/// Phase 2 hook handler.
///
/// No fields — `before_set` reads merge-resolution and prior state
/// from `ctx` (handed by the substrate on Patch) and the submitted
/// body. Phase 3 may add a consumer-cache handle when `after_set`
/// becomes non-trivial; at that point the struct grows fields, not
/// before.
#[derive(Default)]
pub struct DomainsHooks;

impl DomainsHooks {
    pub fn new() -> Self {
        Self
    }

    /// Resolve the effective value the substrate will commit. Replace
    /// is the body verbatim; Patch shallow-merges the body over
    /// `ctx.old`. Patch over an absent row is rejected here.
    fn effective_value(ctx: &HookCtx) -> Result<PropValue, HookError> {
        let merge = ctx.merge.ok_or_else(|| {
            HookError::hook(
                "domains before_set fired with merge=None \
                 (substrate regression — HookCtx.merge should be Some on set hooks)",
            )
        })?;
        let new = ctx
            .new
            .clone()
            .ok_or_else(|| HookError::hook("domains before_set fired with new=None"))?;
        match merge {
            MergeMode::Replace => Ok(new),
            MergeMode::Patch => {
                let old = ctx.old.clone().ok_or_else(|| {
                    HookError::validation(format!(
                        "{PATCH_REQUIRES_EXISTING_ROW_PREFIX} maild.domains has no prior row for \
                         key {:?}; use merge=replace with the full defaults body to initialise",
                        ctx.key.key
                    ))
                })?;
                Ok(merge_patch_object(&old, new))
            }
        }
    }
}

impl HookHandler for DomainsHooks {
    fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            validate_key(&ctx.key.key)?;
            let effective = Self::effective_value(ctx)?;
            validate_shape(&ctx.key.key, &effective)?;
            validate_invariants(&effective)?;
            Ok(())
        })
    }

    // `before_delete` is the trait default (a no-op = permissive). The
    // substrate runtime still enforces `if_version` on the request
    // (require_version=true) and applies its default `SoftDelete` mode.
    // Phase 3 may tighten this to refuse deletion of a row that still
    // has live `accounts` referencing its FQDN — the current accounts
    // table has no FK dependency on this row, so deletion is always
    // safe at the substrate layer today.

    // `after_set` is the trait default — Phase 2 has no live consumers.
}

/// Shallow object-level merge mirroring the substrate's
/// `sqlite::merge_patch`. Keeps the hook validating the same
/// effective value the storage layer will commit.
fn merge_patch_object(old: &PropValue, new: PropValue) -> PropValue {
    match (old, new) {
        (PropValue::Object(o), PropValue::Object(n)) => {
            let mut out = o.clone();
            for (k, v) in n {
                out.insert(k, v);
            }
            PropValue::Object(out)
        }
        (_, new) => new,
    }
}

/// FQDN validation per RFC 1035 §2.3.1 (LDH labels) with explicit
/// lowercase enforcement. Used both for the row key (`ctx.key.key`)
/// and for body fields that hold a hostname.
///
/// Public so the CLI (`cosmix-maild domain add`) can pre-validate
/// before the Bus roundtrip — surfaces operator typos before the
/// broker handshake.
///
/// Rules:
/// - Total length 1..=253.
/// - At least one label; each label 1..=63 chars.
/// - Each label matches `[a-z0-9]([a-z0-9-]*[a-z0-9])?` — uppercase
///   is rejected (not silently lowercased) so an operator typo
///   surfaces in `props.list` output rather than being mutated away.
/// - No leading / trailing dot, no consecutive dots.
pub fn validate_fqdn(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("empty string".into());
    }
    if s.len() > 253 {
        return Err(format!("length {} exceeds 253", s.len()));
    }
    if s.starts_with('.') || s.ends_with('.') {
        return Err("leading or trailing dot".into());
    }
    for label in s.split('.') {
        if label.is_empty() {
            return Err("empty label (consecutive dots?)".into());
        }
        if label.len() > 63 {
            return Err(format!("label {label:?} length exceeds 63"));
        }
        let bytes = label.as_bytes();
        if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
            return Err(format!("label {label:?} must start and end with [a-z0-9]"));
        }
        for &b in bytes {
            let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
            if !ok {
                return Err(format!(
                    "label {label:?} contains illegal byte {b:#x} \
                     (allowed: lowercase [a-z0-9-]; uppercase rejected explicitly)"
                ));
            }
        }
    }
    Ok(())
}

fn validate_key(key: &str) -> Result<(), HookError> {
    validate_fqdn(key).map_err(|e| HookError::validation(format!("{INVALID_FQDN_KEY_PREFIX} {e}")))
}

/// `tls_identity_name` validator — FQDN shape PLUS path-segment-safe
/// checks. The value flows into both an SNI server-name match (RFC
/// 6066) and a filesystem path component (Phase 5a's `maild.tls.reload`
/// builds `<tls_root>/<name>.{cert,key}.pem`); a value like
/// `../etc/passwd` or `with/slash` would otherwise pass the substrate
/// write and escape `<tls_root>` at PEM-resolve time.
///
/// Step 1 reuses [`validate_fqdn`] for the SNI label shape. Step 2 adds
/// path-segment-safe checks `validate_fqdn` does not cover: rejecting
/// `/`, `\\`, NUL, control bytes (`<0x20`), and the whole values `.` /
/// `..` (defensive — `validate_fqdn` already excludes empty labels but
/// a future change could re-admit them).
pub fn validate_tls_identity_name(s: &str) -> Result<(), String> {
    validate_fqdn(s)?;
    if s == "." || s == ".." {
        return Err(format!("whole value {s:?} is not a usable SNI label"));
    }
    for &b in s.as_bytes() {
        if b == b'/' || b == b'\\' || b == 0 || b < 0x20 {
            return Err(format!(
                "byte {b:#x} disallowed (path-segment-safe rules: no /, \\, NUL, or control bytes)"
            ));
        }
    }
    Ok(())
}

/// `cert_source` value shape — Null or
/// `{kind: "manual" | "webd", vhost_fqdn: Null | String}`.
///
/// The carve in `_doc/planned/maild-webd-cert-bridge.md` constrains:
///
/// - `kind = "manual"`: `vhost_fqdn` MUST be absent or Null. Equivalent
///   to the row-level Null cert_source — explicit form for operators
///   who want the field non-Null for legibility.
/// - `kind = "webd"`: `vhost_fqdn` MUST be a non-empty string that
///   passes [`validate_tls_identity_name`] (FQDN + path-segment-safe,
///   since the value flows into a webd Bus call and may be persisted
///   through to a filesystem path).
///
/// Cross-field constraint "`kind=webd` requires the row's
/// `tls_identity_name` to be set" lives in [`validate_invariants`]
/// because it spans two top-level fields.
///
/// Unknown nested-field rejection: only `kind` and `vhost_fqdn` are
/// admitted; an operator-supplied `cert_source.foo` is rejected here
/// (the substrate stores objects verbatim — JsonValuesMapping — so the
/// hook is the only enforcer for the inner object too).
pub fn validate_cert_source(value: &PropValue) -> Result<(), String> {
    let obj = match value {
        PropValue::Null => return Ok(()),
        PropValue::Object(m) => m,
        other => {
            return Err(format!(
                "must be Null or an object (got {})",
                other.type_name()
            ));
        }
    };

    const KNOWN: &[&str] = &["kind", "vhost_fqdn"];
    for k in obj.keys() {
        if !KNOWN.contains(&k.as_str()) {
            return Err(format!(
                "unknown nested field {k:?} (allowed: {})",
                KNOWN.join(", ")
            ));
        }
    }

    let kind = match obj.get("kind") {
        Some(PropValue::String(s)) => s.as_str(),
        Some(other) => {
            return Err(format!("kind must be a string (got {})", other.type_name()));
        }
        None => return Err("kind is required".into()),
    };
    if !cert_source_kind_variants().iter().any(|v| v == kind) {
        return Err(format!(
            "kind {kind:?} not in {{{}}}",
            cert_source_kind_variants().join(", ")
        ));
    }

    let vhost_fqdn = obj.get("vhost_fqdn");
    match kind {
        "manual" => match vhost_fqdn {
            None | Some(PropValue::Null) => {}
            Some(_) => {
                return Err("vhost_fqdn must be absent or Null when kind=\"manual\"".into());
            }
        },
        "webd" => match vhost_fqdn {
            Some(PropValue::String(s)) => {
                if s.is_empty() {
                    return Err("vhost_fqdn must be a non-empty FQDN when kind=\"webd\"".into());
                }
                validate_tls_identity_name(s).map_err(|e| format!("vhost_fqdn: {e}"))?;
            }
            None | Some(PropValue::Null) => {
                return Err("vhost_fqdn is required when kind=\"webd\"".into());
            }
            Some(other) => {
                return Err(format!(
                    "vhost_fqdn must be a string when kind=\"webd\" (got {})",
                    other.type_name()
                ));
            }
        },
        _ => unreachable!("kind enum membership already checked"),
    }

    Ok(())
}

/// DKIM selector shape — `[a-z0-9-]{1,63}` per the DKIM convention.
pub(crate) fn validate_dkim_selector(s: &str) -> Result<(), String> {
    if s.is_empty() || s.len() > 63 {
        return Err(format!("length {} out of 1..=63", s.len()));
    }
    for &b in s.as_bytes() {
        let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-';
        if !ok {
            return Err(format!("illegal byte {b:#x} (allowed: [a-z0-9-])"));
        }
    }
    Ok(())
}

/// `relay_recipients` entry — bare local-part or full `local@fqdn`.
fn validate_recipient(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("empty entry".into());
    }
    let (local, dom) = match s.rsplit_once('@') {
        Some((l, d)) => (l, Some(d)),
        None => (s, None),
    };
    if local.is_empty() || local.len() > 64 {
        return Err(format!("local-part length {} out of 1..=64", local.len()));
    }
    for &b in local.as_bytes() {
        let ok = b.is_ascii_alphanumeric()
            || b == b'.'
            || b == b'_'
            || b == b'%'
            || b == b'+'
            || b == b'-';
        if !ok {
            return Err(format!(
                "local-part {local:?} byte {b:#x} not in [A-Za-z0-9._%+-]"
            ));
        }
    }
    if let Some(d) = dom {
        validate_fqdn(d).map_err(|e| format!("domain part {d:?}: {e}"))?;
    }
    Ok(())
}

fn require_object(value: &PropValue) -> Result<&BTreeMap<String, PropValue>, HookError> {
    value.as_object().ok_or_else(|| {
        HookError::validation(format!(
            "{INVALID_FIELD_PREFIX} record must be an object (got {})",
            value.type_name()
        ))
    })
}

fn validate_shape(key: &str, value: &PropValue) -> Result<(), HookError> {
    let obj = require_object(value)?;

    // Unknown-field rejection — the `JsonValuesMapping` adapter
    // stores arbitrary top-level keys verbatim, so the hook is the
    // only enforcer.
    let known: std::collections::HashSet<&str> = schema_field_names().iter().copied().collect();
    for k in obj.keys() {
        if !known.contains(k.as_str()) {
            return Err(HookError::validation(format!(
                "{INVALID_FIELD_PREFIX} unknown field {k:?} (allowed: {})",
                schema_field_names().join(", ")
            )));
        }
    }

    // `domain` — required, must equal the row key. The runtime's
    // `validate_primary_key` already enforces this on Replace and
    // Patch-create; we re-check for defence in depth and to catch a
    // future Patch-update that tried to flip the field.
    match obj.get("domain") {
        Some(PropValue::String(s)) => {
            if s != key {
                return Err(HookError::validation(format!(
                    "{INVALID_FIELD_PREFIX} domain {s:?} does not match key {key:?}"
                )));
            }
            // Body-side shape re-check (substrate string-compares only).
            validate_fqdn(s).map_err(|e| {
                HookError::validation(format!("{INVALID_FIELD_PREFIX} domain: {e}"))
            })?;
        }
        Some(other) => {
            return Err(HookError::validation(format!(
                "{INVALID_FIELD_PREFIX} domain must be a string (got {})",
                other.type_name()
            )));
        }
        None => {
            return Err(HookError::validation(format!(
                "{REPLACE_REQUIRES_FULL_BODY_PREFIX} domain is required"
            )));
        }
    }

    require_bool(obj, "enabled")?;
    require_enum(obj, "role", &role_variants())?;
    require_enum(obj, "spf_policy", &spf_policy_variants())?;
    require_enum(obj, "dmarc_policy", &dmarc_policy_variants())?;
    require_bool(obj, "autoconfig_published")?;

    require_option_string_fqdn(obj, "primary_hostname")?;
    require_option_string_fqdn(obj, "helo_identity")?;
    require_option_string_fqdn(obj, "message_id_host")?;

    // `tls_identity_name` — Option<String>; if present, validated as
    // an FQDN + path-segment-safe (the value flows into a PEM path
    // component via Phase 5a `maild.tls.reload`'s
    // `<tls_root>/<name>.{cert,key}.pem` resolve). See
    // [`validate_tls_identity_name`].
    match obj.get("tls_identity_name") {
        Some(PropValue::Null) => {}
        Some(PropValue::String(s)) => {
            validate_tls_identity_name(s).map_err(|e| {
                HookError::validation(format!("{INVALID_FIELD_PREFIX} tls_identity_name: {e}"))
            })?;
        }
        Some(other) => {
            return Err(HookError::validation(format!(
                "{INVALID_FIELD_PREFIX} tls_identity_name must be Null or a string (got {})",
                other.type_name()
            )));
        }
        None => {
            return Err(HookError::validation(format!(
                "{REPLACE_REQUIRES_FULL_BODY_PREFIX} tls_identity_name is required"
            )));
        }
    }

    // `cert_source` — Null or `{kind: "manual"|"webd",
    // vhost_fqdn: Null|String}`. Per-field shape only; the cross-field
    // constraint "kind=webd requires tls_identity_name to be set" lives
    // in `validate_invariants`.
    //
    // Missing field = treat as Null (manual). The β-1 commit added this
    // field to the schema; legacy rows committed before β-1 do not carry
    // it, and a Patch over such a prior row merges to a body without
    // `cert_source` even when the operator did not touch it. Per Path α
    // (`_doc/planned/maild-webd-cert-bridge.md`), absent ≡ kind=manual is
    // the substrate behaviour. `defaults_value` still emits explicit
    // Null so fresh rows persist the field for legibility.
    if let Some(value) = obj.get("cert_source") {
        validate_cert_source(value).map_err(|e| {
            HookError::validation(format!("{INVALID_FIELD_PREFIX} cert_source: {e}"))
        })?;
    }

    // `dkim_selector_active` — Option<String>; if String, must be a
    // valid selector shape. Membership in `dkim_selectors` is the
    // cross-field check.
    match obj.get("dkim_selector_active") {
        Some(PropValue::Null) => {}
        Some(PropValue::String(s)) => {
            validate_dkim_selector(s).map_err(|e| {
                HookError::validation(format!("{INVALID_FIELD_PREFIX} dkim_selector_active: {e}"))
            })?;
        }
        Some(other) => {
            return Err(HookError::validation(format!(
                "{INVALID_FIELD_PREFIX} dkim_selector_active must be Null or a string (got {})",
                other.type_name()
            )));
        }
        None => {
            return Err(HookError::validation(format!(
                "{REPLACE_REQUIRES_FULL_BODY_PREFIX} dkim_selector_active is required"
            )));
        }
    }

    // `mx_targets` — List<String>, each a valid FQDN.
    require_list(obj, "mx_targets", |i, item| match item {
        PropValue::String(s) => validate_fqdn(s).map_err(|e| format!("[{i}]: {e}")),
        other => Err(format!(
            "[{i}] must be a string (got {})",
            other.type_name()
        )),
    })?;

    // `dkim_selectors` — List<String>, each a valid selector,
    // duplicates rejected.
    let dkim_selectors_field = require_list(obj, "dkim_selectors", |i, item| match item {
        PropValue::String(s) => validate_dkim_selector(s).map_err(|e| format!("[{i}]: {e}")),
        other => Err(format!(
            "[{i}] must be a string (got {})",
            other.type_name()
        )),
    })?;
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (i, item) in dkim_selectors_field.iter().enumerate() {
        if let PropValue::String(s) = item
            && !seen.insert(s.as_str())
        {
            return Err(HookError::validation(format!(
                "{INVALID_FIELD_PREFIX} dkim_selectors[{i}] {s:?} is a duplicate"
            )));
        }
    }

    // `relay_recipients` — List<String>, each a local-part or address.
    require_list(obj, "relay_recipients", |i, item| match item {
        PropValue::String(s) => validate_recipient(s).map_err(|e| format!("[{i}]: {e}")),
        other => Err(format!(
            "[{i}] must be a string (got {})",
            other.type_name()
        )),
    })?;

    Ok(())
}

fn require_bool(obj: &BTreeMap<String, PropValue>, field: &str) -> Result<bool, HookError> {
    match obj.get(field) {
        Some(PropValue::Bool(b)) => Ok(*b),
        Some(other) => Err(HookError::validation(format!(
            "{INVALID_FIELD_PREFIX} {field} must be a bool (got {})",
            other.type_name()
        ))),
        None => Err(HookError::validation(format!(
            "{REPLACE_REQUIRES_FULL_BODY_PREFIX} {field} is required"
        ))),
    }
}

fn require_enum(
    obj: &BTreeMap<String, PropValue>,
    field: &str,
    variants: &[String],
) -> Result<String, HookError> {
    match obj.get(field) {
        Some(PropValue::String(s)) => {
            if variants.iter().any(|v| v == s) {
                Ok(s.clone())
            } else {
                Err(HookError::validation(format!(
                    "{INVALID_FIELD_PREFIX} {field} {s:?} not in {{{}}}",
                    variants.join(", ")
                )))
            }
        }
        Some(other) => Err(HookError::validation(format!(
            "{INVALID_FIELD_PREFIX} {field} must be a string (got {})",
            other.type_name()
        ))),
        None => Err(HookError::validation(format!(
            "{REPLACE_REQUIRES_FULL_BODY_PREFIX} {field} is required"
        ))),
    }
}

fn require_option_string_fqdn(
    obj: &BTreeMap<String, PropValue>,
    field: &str,
) -> Result<(), HookError> {
    match obj.get(field) {
        Some(PropValue::Null) => Ok(()),
        Some(PropValue::String(s)) => {
            if s.is_empty() {
                return Err(HookError::validation(format!(
                    "{INVALID_FIELD_PREFIX} {field} must be Null or a non-empty FQDN \
                     (operators send Null for absence, not an empty string)"
                )));
            }
            validate_fqdn(s)
                .map_err(|e| HookError::validation(format!("{INVALID_FIELD_PREFIX} {field}: {e}")))
        }
        Some(other) => Err(HookError::validation(format!(
            "{INVALID_FIELD_PREFIX} {field} must be Null or a string (got {})",
            other.type_name()
        ))),
        None => Err(HookError::validation(format!(
            "{REPLACE_REQUIRES_FULL_BODY_PREFIX} {field} is required"
        ))),
    }
}

fn require_list<'a, F>(
    obj: &'a BTreeMap<String, PropValue>,
    field: &str,
    mut per_item: F,
) -> Result<&'a [PropValue], HookError>
where
    F: FnMut(usize, &PropValue) -> Result<(), String>,
{
    let list = match obj.get(field) {
        Some(PropValue::List(l)) => l,
        Some(other) => {
            return Err(HookError::validation(format!(
                "{INVALID_FIELD_PREFIX} {field} must be a list (got {})",
                other.type_name()
            )));
        }
        None => {
            return Err(HookError::validation(format!(
                "{REPLACE_REQUIRES_FULL_BODY_PREFIX} {field} is required"
            )));
        }
    };
    for (i, item) in list.iter().enumerate() {
        per_item(i, item)
            .map_err(|e| HookError::validation(format!("{INVALID_FIELD_PREFIX} {field}{e}")))?;
    }
    Ok(list)
}

/// Cross-field invariants — every per-field check has already
/// passed when this fires.
fn validate_invariants(value: &PropValue) -> Result<(), HookError> {
    let obj = require_object(value)?;

    // `cert_source.kind == "webd"` requires the row's
    // `tls_identity_name` to be set — the bridge needs both the
    // vhost FQDN to fetch from and the local PEM filename to write to.
    if let Some(PropValue::Object(cs)) = obj.get("cert_source")
        && matches!(cs.get("kind"), Some(PropValue::String(k)) if k == "webd")
    {
        match obj.get("tls_identity_name") {
            Some(PropValue::String(s)) if !s.is_empty() => {}
            _ => {
                return Err(HookError::validation(format!(
                    "{CROSS_FIELD_PREFIX} cert_source.kind=\"webd\" requires \
                     tls_identity_name to be set (the bridge uses it as the \
                     local PEM filename under <tls_root>)"
                )));
            }
        }
    }

    // `dkim_selector_active`, if set, must be a member of
    // `dkim_selectors`.
    if let Some(PropValue::String(active)) = obj.get("dkim_selector_active") {
        let selectors = match obj.get("dkim_selectors") {
            Some(PropValue::List(l)) => l,
            _ => unreachable!("validate_shape ensures dkim_selectors is List"),
        };
        let is_member = selectors
            .iter()
            .any(|s| matches!(s, PropValue::String(v) if v == active));
        if !is_member {
            return Err(HookError::validation(format!(
                "{CROSS_FIELD_PREFIX} dkim_selector_active {active:?} not in dkim_selectors"
            )));
        }
    }

    Ok(())
}

/// Umbrella Phase 2 startup reconciliation diagnostic.
///
/// Emits a single INFO log record listing the two-sided diff between
/// the set of domains carrying accounts (`accounts_domains`, gathered
/// by the caller via `Db::distinct_account_domains`) and the set of
/// `maild.domains` row keys present in the substrate. Diagnostic-only
/// — the daemon does **not** auto-create or auto-delete substrate rows
/// based on this diff. An operator who wants the diff again after
/// adding/removing accounts restarts the daemon or invokes a future
/// `maild.domains.reconcile` verb (Phase 3 territory if/when needed).
///
/// Errors are surfaced for the caller to log-and-continue; a substrate
/// read failure on first boot must not block a daemon that hasn't even
/// processed mail yet.
pub async fn reconcile_diagnostic(
    accounts_domains: BTreeSet<String>,
    store: &Arc<SqliteStore>,
) -> Result<()> {
    let ns = namespace_name();
    // `PropertyStore::list` returns `Snapshot<Vec<Record>>`; we don't
    // watch from this one-shot, so `observed_nseq` is discarded.
    let snapshot = PropertyStore::list(store.as_ref(), &ns)
        .await
        .map_err(|e| anyhow::anyhow!("list maild.domains: {e}"))?;
    let namespace_rows: BTreeSet<String> =
        snapshot.value.iter().map(|r| r.key.key.clone()).collect();

    let (missing_from_namespace, orphaned_in_namespace) =
        compute_reconcile_diff(&accounts_domains, &namespace_rows);

    tracing::info!(
        accounts_domains = accounts_domains.len(),
        namespace_rows = namespace_rows.len(),
        ?missing_from_namespace,
        ?orphaned_in_namespace,
        "maild.domains reconciliation diagnostic — informational only, \
         the daemon does not auto-create or auto-delete rows"
    );
    Ok(())
}

/// Pure two-sided set difference used by [`reconcile_diagnostic`].
/// Exposed so unit tests don't need to spin up a substrate or capture
/// tracing output to validate the diagnostic's logic.
///
/// Returns `(missing_from_namespace, orphaned_in_namespace)`:
/// - `missing_from_namespace` — accounts-bearing domains with no row.
/// - `orphaned_in_namespace` — row keys with no account on them.
///
/// Both vectors are deterministic-ordered (caller-passed `BTreeSet`s
/// iterate in sort order).
pub fn compute_reconcile_diff(
    accounts_domains: &BTreeSet<String>,
    namespace_rows: &BTreeSet<String>,
) -> (Vec<String>, Vec<String>) {
    let missing = accounts_domains
        .difference(namespace_rows)
        .cloned()
        .collect();
    let orphaned = namespace_rows
        .difference(accounts_domains)
        .cloned()
        .collect();
    (missing, orphaned)
}

/// Typed projection over a `maild.domains` row's `PropValue::Object`.
/// The substrate stores rows verbatim (no schema-default materialisation
/// at commit), so consumers either repeat the lookup-and-cast dance per
/// field or extract via this helper. Phase 3 commits 2 + 3 read
/// substrate rows in four places (RCPT TO gate, HELO selection,
/// Message-ID host, JMAP `Identity/get`); the typed projection keeps
/// the parsing single-sourced so a field-typing bug only needs fixing
/// once.
///
/// All fields except `enabled` and `role` are nullable in the schema,
/// so the projection surfaces them as `Option`. List fields are
/// projected as `Vec<String>` (already empty by default in
/// [`defaults_value`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainView {
    pub enabled: bool,
    pub role: String,
    pub primary_hostname: Option<String>,
    pub helo_identity: Option<String>,
    pub message_id_host: Option<String>,
    pub relay_recipients: Vec<String>,
    pub dkim_selector_active: Option<String>,
    pub dkim_selectors: Vec<String>,
}

/// Project a [`Record`]'s value into a typed [`DomainView`].
///
/// Returns `Err` when the row body is shaped wrong — missing the
/// required `enabled`/`role` fields, or carrying the wrong PropValue
/// variant. The substrate hook validates these at write time, so a
/// failing `view` call means on-disk corruption or a post-write schema
/// drift the consumer should surface (Phase 3 receiver returns 451
/// transient in that case to invite operator intervention).
pub fn view(record: &Record) -> Result<DomainView, anyhow::Error> {
    let PropValue::Object(map) = &record.value else {
        anyhow::bail!("maild.domains row body is not an Object PropValue");
    };
    let key_display = record.key.key.as_str();
    let enabled = match map.get("enabled") {
        Some(PropValue::Bool(b)) => *b,
        Some(other) => {
            anyhow::bail!("maild.domains[{key_display}].enabled has unexpected variant {other:?}")
        }
        None => anyhow::bail!("maild.domains[{key_display}].enabled missing"),
    };
    let role = match map.get("role") {
        Some(PropValue::String(s)) => s.clone(),
        Some(other) => {
            anyhow::bail!("maild.domains[{key_display}].role has unexpected variant {other:?}")
        }
        None => anyhow::bail!("maild.domains[{key_display}].role missing"),
    };
    let primary_hostname =
        nullable_string(map.get("primary_hostname"), key_display, "primary_hostname")?;
    let helo_identity = nullable_string(map.get("helo_identity"), key_display, "helo_identity")?;
    let message_id_host =
        nullable_string(map.get("message_id_host"), key_display, "message_id_host")?;
    let relay_recipients = match map.get("relay_recipients") {
        Some(PropValue::List(items)) => items
            .iter()
            .map(|v| match v {
                PropValue::String(s) => Ok(s.clone()),
                other => Err(anyhow::anyhow!(
                    "maild.domains[{key_display}].relay_recipients item has unexpected variant {other:?}"
                )),
            })
            .collect::<Result<Vec<String>, _>>()?,
        None => anyhow::bail!("maild.domains[{key_display}].relay_recipients missing"),
        Some(other) => anyhow::bail!(
            "maild.domains[{key_display}].relay_recipients has unexpected variant {other:?}"
        ),
    };
    let dkim_selector_active = nullable_string(
        map.get("dkim_selector_active"),
        key_display,
        "dkim_selector_active",
    )?;
    let dkim_selectors = match map.get("dkim_selectors") {
        Some(PropValue::List(items)) => items
            .iter()
            .map(|v| match v {
                PropValue::String(s) => Ok(s.clone()),
                other => Err(anyhow::anyhow!(
                    "maild.domains[{key_display}].dkim_selectors item has unexpected variant {other:?}"
                )),
            })
            .collect::<Result<Vec<String>, _>>()?,
        None => anyhow::bail!("maild.domains[{key_display}].dkim_selectors missing"),
        Some(other) => anyhow::bail!(
            "maild.domains[{key_display}].dkim_selectors has unexpected variant {other:?}"
        ),
    };
    Ok(DomainView {
        enabled,
        role,
        primary_hostname,
        helo_identity,
        message_id_host,
        relay_recipients,
        dkim_selector_active,
        dkim_selectors,
    })
}

fn nullable_string(
    value: Option<&PropValue>,
    key: &str,
    field: &'static str,
) -> Result<Option<String>, anyhow::Error> {
    match value {
        None | Some(PropValue::Null) => Ok(None),
        Some(PropValue::String(s)) => Ok(Some(s.clone())),
        Some(other) => {
            anyhow::bail!("maild.domains[{key}].{field} has unexpected variant {other:?}")
        }
    }
}

/// Wire the `maild.domains` namespace into the substrate.
pub fn register(router: &mut PropsRouter, store: &Arc<SqliteStore>) -> Result<Arc<Runtime>> {
    let hooks = Hooks::new(DomainsHooks::new());
    let spec = spec(hooks);
    store
        .register_namespace(&spec, Arc::new(JsonValuesMapping::new(namespace_name())))
        .map_err(|e| anyhow::anyhow!("register domains namespace in store: {e}"))?;
    let runtime = Arc::new(Runtime::new(router.service(), spec, store.clone()));
    router
        .register(runtime.clone())
        .map_err(|e| anyhow::anyhow!("register domains runtime on router: {e}"))?;
    Ok(runtime)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_props::record::{Actor, RecordKey, Version};

    fn key(domain: &str) -> RecordKey {
        RecordKey::collection(namespace_name(), domain.to_string())
    }

    fn set_ctx(domain: &str, merge: MergeMode, old: Option<PropValue>, new: PropValue) -> HookCtx {
        HookCtx {
            key: key(domain),
            old,
            new: Some(new),
            version: Version::zero(),
            actor: Actor::service("test").expect("valid actor"),
            merge: Some(merge),
            origin: cosmix_props::WriteOrigin::caller(),
        }
    }

    /// Build a Replace body from `defaults_value(domain)` after
    /// mutating it in place.
    fn defaults_obj_mut(
        domain: &str,
        mut f: impl FnMut(&mut BTreeMap<String, PropValue>),
    ) -> PropValue {
        let mut v = defaults_value(domain);
        match &mut v {
            PropValue::Object(m) => f(m),
            _ => unreachable!(),
        }
        v
    }

    fn hooks() -> DomainsHooks {
        DomainsHooks::new()
    }

    #[test]
    fn namespace_name_is_domains() {
        assert_eq!(namespace_name().as_str(), "domains");
    }

    #[test]
    fn defaults_value_carries_every_schema_field() {
        let v = defaults_value("example.com");
        let obj = v.as_object().expect("object");
        for field in schema_field_names() {
            assert!(
                obj.contains_key(*field),
                "defaults_value missing schema field {field:?}"
            );
        }
        assert_eq!(obj.len(), schema_field_names().len());
        assert_eq!(
            obj.get("domain"),
            Some(&PropValue::String("example.com".into()))
        );
        for opt_field in [
            "primary_hostname",
            "helo_identity",
            "message_id_host",
            "dkim_selector_active",
            "tls_identity_name",
            "cert_source",
        ] {
            assert_eq!(
                obj.get(opt_field),
                Some(&PropValue::Null),
                "{opt_field} default should be Null"
            );
        }
        assert_eq!(
            obj.get("role"),
            Some(&PropValue::String("authoritative".into()))
        );
        assert_eq!(
            obj.get("spf_policy"),
            Some(&PropValue::String("lenient".into()))
        );
        assert_eq!(
            obj.get("dmarc_policy"),
            Some(&PropValue::String("monitor".into()))
        );
        assert_eq!(obj.get("enabled"), Some(&PropValue::Bool(true)));
        assert_eq!(
            obj.get("autoconfig_published"),
            Some(&PropValue::Bool(false))
        );
    }

    #[tokio::test]
    async fn replace_defaults_body_accepted() {
        hooks()
            .before_set(&set_ctx(
                "example.com",
                MergeMode::Replace,
                None,
                defaults_value("example.com"),
            ))
            .await
            .expect("defaults body should pass Replace");
    }

    #[tokio::test]
    async fn replace_missing_required_field_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            m.remove("role");
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(
            err.message().starts_with(REPLACE_REQUIRES_FULL_BODY_PREFIX),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("role"));
    }

    #[tokio::test]
    async fn replace_unknown_field_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert("made_up_field".into(), PropValue::Bool(true));
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(
            err.message().starts_with(INVALID_FIELD_PREFIX),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("made_up_field"));
    }

    #[tokio::test]
    async fn replace_invalid_enum_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert("role".into(), PropValue::String("supervisor".into()));
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(err.message().starts_with(INVALID_FIELD_PREFIX));
        assert!(err.message().contains("supervisor"));
    }

    #[tokio::test]
    async fn replace_relay_role_rejected_in_phase2() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert("role".into(), PropValue::String("relay".into()));
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(err.message().starts_with(INVALID_FIELD_PREFIX));
        assert!(err.message().contains("relay"));
    }

    #[tokio::test]
    async fn replace_invalid_fqdn_key_rejected() {
        // `validate_primary_key` would reject body.domain != key first,
        // so use a body whose `domain` matches the bad key.
        let v = defaults_value("bad..name");
        let err = hooks()
            .before_set(&set_ctx("bad..name", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(
            err.message().starts_with(INVALID_FQDN_KEY_PREFIX),
            "got: {}",
            err.message()
        );
    }

    #[tokio::test]
    async fn replace_uppercase_key_rejected() {
        let v = defaults_value("EXAMPLE.ORG");
        let err = hooks()
            .before_set(&set_ctx("EXAMPLE.ORG", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(err.message().starts_with(INVALID_FQDN_KEY_PREFIX));
    }

    #[tokio::test]
    async fn replace_domain_field_mismatches_key_rejected() {
        // Substrate's `validate_primary_key` fires at the runtime layer
        // before the hook does — but the hook also re-checks for
        // defence in depth. Exercising the hook path here: build a body
        // whose `domain` mismatches, against a (valid) key.
        let mut v = defaults_value("example.com");
        if let PropValue::Object(m) = &mut v {
            m.insert("domain".into(), PropValue::String("evil.example".into()));
        }
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(err.message().starts_with(INVALID_FIELD_PREFIX));
        assert!(err.message().contains("evil.example"));
    }

    #[tokio::test]
    async fn cross_field_active_selector_not_in_list_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert(
                "dkim_selector_active".into(),
                PropValue::String("2026b".into()),
            );
            m.insert(
                "dkim_selectors".into(),
                PropValue::List(vec![PropValue::String("2026a".into())]),
            );
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(
            err.message().starts_with(CROSS_FIELD_PREFIX),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("2026b"));
    }

    #[tokio::test]
    async fn cross_field_active_selector_in_list_accepted() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert(
                "dkim_selector_active".into(),
                PropValue::String("2026a".into()),
            );
            m.insert(
                "dkim_selectors".into(),
                PropValue::List(vec![
                    PropValue::String("2026a".into()),
                    PropValue::String("2026b".into()),
                ]),
            );
        });
        hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .expect("active selector in list should accept");
    }

    #[tokio::test]
    async fn dkim_selectors_duplicate_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert(
                "dkim_selectors".into(),
                PropValue::List(vec![
                    PropValue::String("2026a".into()),
                    PropValue::String("2026a".into()),
                ]),
            );
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(err.message().contains("duplicate"));
    }

    #[tokio::test]
    async fn null_optional_string_field_accepted() {
        // defaults_value already sets helo_identity = Null; this just
        // exercises the round-trip path.
        let v = defaults_value("example.com");
        hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .expect("Null option field should accept");
    }

    #[tokio::test]
    async fn empty_string_optional_field_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert("helo_identity".into(), PropValue::String("".into()));
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(err.message().starts_with(INVALID_FIELD_PREFIX));
        assert!(err.message().contains("helo_identity"));
    }

    #[tokio::test]
    async fn empty_mx_targets_accepted() {
        // defaults_value sets mx_targets = []; consumer fallback is a
        // Phase 3 concern, not a Phase 2 validation concern.
        hooks()
            .before_set(&set_ctx(
                "example.com",
                MergeMode::Replace,
                None,
                defaults_value("example.com"),
            ))
            .await
            .expect("empty mx_targets should accept");
    }

    #[tokio::test]
    async fn mx_targets_invalid_fqdn_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert(
                "mx_targets".into(),
                PropValue::List(vec![PropValue::String("bad..name".into())]),
            );
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(err.message().contains("mx_targets"));
    }

    #[tokio::test]
    async fn relay_recipient_bare_local_part_accepted() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert("role".into(), PropValue::String("backup_mx".into()));
            m.insert(
                "relay_recipients".into(),
                PropValue::List(vec![PropValue::String("postmaster".into())]),
            );
        });
        hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .expect("bare local-part should accept");
    }

    #[tokio::test]
    async fn relay_recipient_full_address_accepted() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert("role".into(), PropValue::String("backup_mx".into()));
            m.insert(
                "relay_recipients".into(),
                PropValue::List(vec![PropValue::String("alice@example.com".into())]),
            );
        });
        hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .expect("full address should accept");
    }

    #[tokio::test]
    async fn relay_recipient_garbage_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert(
                "relay_recipients".into(),
                PropValue::List(vec![PropValue::String("@@bogus".into())]),
            );
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(err.message().contains("relay_recipients"));
    }

    #[tokio::test]
    async fn patch_no_prior_rejected() {
        // Body must carry `domain` matching the key; otherwise the
        // runtime's `validate_primary_key` fires first and the test
        // asserts the wrong rejection.
        let patch = defaults_obj_mut("example.com", |m| {
            // Patch keeps domain to match the runtime check, but
            // varies one other field.
            m.insert("enabled".into(), PropValue::Bool(false));
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Patch, None, patch))
            .await
            .unwrap_err();
        assert!(
            err.message()
                .starts_with(PATCH_REQUIRES_EXISTING_ROW_PREFIX),
            "got: {}",
            err.message()
        );
    }

    #[tokio::test]
    async fn patch_preserves_unwritten_fields() {
        let prior = defaults_value("example.com");
        // Patch with just the `enabled` flip — substrate-shaped Patch
        // body must still carry `domain` to satisfy validate_primary_key
        // (which fires on Patch-create; here ctx.old is Some so the
        // runtime allows the omission, but the merged body still has
        // `domain` because we Patch over the defaults).
        let mut patch_obj = BTreeMap::new();
        patch_obj.insert("domain".into(), PropValue::String("example.com".into()));
        patch_obj.insert("enabled".into(), PropValue::Bool(false));
        let patch = PropValue::Object(patch_obj);
        hooks()
            .before_set(&set_ctx(
                "example.com",
                MergeMode::Patch,
                Some(prior),
                patch,
            ))
            .await
            .expect("patch over prior should accept");
    }

    #[tokio::test]
    async fn tls_identity_long_string_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert(
                "tls_identity_name".into(),
                PropValue::String("a".repeat(254)),
            );
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(err.message().contains("tls_identity_name"));
    }

    #[tokio::test]
    async fn tls_identity_empty_string_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert("tls_identity_name".into(), PropValue::String("".into()));
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(err.message().contains("tls_identity_name"));
    }

    /// 5a-commit-2: `tls_identity_name` flows into a PEM path component,
    /// so the substrate write must reject anything that escapes the
    /// SNI-label shape or that would let `<tls_root>.join(name)`
    /// traverse out of `<tls_root>`. This pins all four classes the
    /// validator must catch: traversal, slash, uppercase, leading-dot.
    #[tokio::test]
    async fn tls_identity_name_path_unsafe_rejected() {
        for bad in [
            "../escape",
            "with/slash",
            "with\\back",
            "EXAMPLE.ORG",
            ".leading",
            ".",
            "..",
            "ok\0nul",
        ] {
            let v = defaults_obj_mut("example.com", |m| {
                m.insert("tls_identity_name".into(), PropValue::String(bad.into()));
            });
            let err = hooks()
                .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
                .await
                .unwrap_err();
            assert!(
                err.message().contains("tls_identity_name"),
                "bad value {bad:?} should surface tls_identity_name error: {}",
                err.message()
            );
        }
    }

    #[tokio::test]
    async fn tls_identity_name_valid_fqdn_accepted() {
        // FQDN-shaped values that are also path-segment-safe pass.
        // Single-label `host` is intentionally allowed — `validate_fqdn`
        // accepts it and there's no traversal risk.
        for ok in ["example.net", "mail.example.com", "host"] {
            let v = defaults_obj_mut("example.com", |m| {
                m.insert("tls_identity_name".into(), PropValue::String(ok.into()));
            });
            hooks()
                .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
                .await
                .unwrap_or_else(|e| {
                    panic!("good value {ok:?} should accept, got: {}", e.message())
                });
        }
    }

    fn cert_source_obj(kind: &str, vhost_fqdn: Option<&str>) -> PropValue {
        let mut m = BTreeMap::new();
        m.insert("kind".into(), PropValue::String(kind.into()));
        if let Some(f) = vhost_fqdn {
            m.insert("vhost_fqdn".into(), PropValue::String(f.into()));
        } else {
            m.insert("vhost_fqdn".into(), PropValue::Null);
        }
        PropValue::Object(m)
    }

    /// Null cert_source = manual (current behaviour). Already covered
    /// by `replace_defaults_body_accepted` indirectly; this pin makes
    /// the carve explicit and resists a future "make cert_source
    /// non-optional" refactor.
    #[tokio::test]
    async fn cert_source_null_accepted() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert("cert_source".into(), PropValue::Null);
        });
        hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .expect("Null cert_source should accept");
    }

    #[tokio::test]
    async fn cert_source_manual_accepted() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert("cert_source".into(), cert_source_obj("manual", None));
        });
        hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .expect("kind=manual with Null vhost_fqdn should accept");
    }

    /// kind=manual must NOT carry a non-Null vhost_fqdn.
    #[tokio::test]
    async fn cert_source_manual_with_vhost_fqdn_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert(
                "cert_source".into(),
                cert_source_obj("manual", Some("example.org")),
            );
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(
            err.message().contains("cert_source"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("manual"),
            "should mention kind=manual constraint: {}",
            err.message()
        );
    }

    #[tokio::test]
    async fn cert_source_webd_accepted() {
        let v = defaults_obj_mut("example.com", |m| {
            m.insert(
                "tls_identity_name".into(),
                PropValue::String("example.com".into()),
            );
            m.insert(
                "cert_source".into(),
                cert_source_obj("webd", Some("example.org")),
            );
        });
        hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .expect("kind=webd + valid vhost_fqdn + tls_identity_name should accept");
    }

    /// kind=webd requires vhost_fqdn (Null/missing both rejected).
    #[tokio::test]
    async fn cert_source_webd_missing_vhost_fqdn_rejected() {
        for vhost in [None, Some(PropValue::Null)] {
            let v = defaults_obj_mut("example.com", |m| {
                m.insert(
                    "tls_identity_name".into(),
                    PropValue::String("example.com".into()),
                );
                let mut cs = BTreeMap::new();
                cs.insert("kind".into(), PropValue::String("webd".into()));
                if let Some(vv) = &vhost {
                    cs.insert("vhost_fqdn".into(), vv.clone());
                }
                m.insert("cert_source".into(), PropValue::Object(cs));
            });
            let err = hooks()
                .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
                .await
                .unwrap_err();
            assert!(
                err.message().contains("vhost_fqdn"),
                "vhost={vhost:?} should surface vhost_fqdn error: {}",
                err.message()
            );
        }
    }

    /// kind=webd vhost_fqdn must be path-segment-safe (same shape as
    /// tls_identity_name — the value flows into an Bus call to webd
    /// and may be persisted into a filename downstream).
    #[tokio::test]
    async fn cert_source_webd_path_unsafe_vhost_fqdn_rejected() {
        for bad in ["../escape", "with/slash", "EXAMPLE.ORG", "", "ok\0nul"] {
            let v = defaults_obj_mut("example.com", |m| {
                m.insert(
                    "tls_identity_name".into(),
                    PropValue::String("example.com".into()),
                );
                m.insert("cert_source".into(), cert_source_obj("webd", Some(bad)));
            });
            let err = hooks()
                .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
                .await
                .unwrap_err();
            assert!(
                err.message().contains("vhost_fqdn"),
                "bad value {bad:?} should surface vhost_fqdn error: {}",
                err.message()
            );
        }
    }

    /// Cross-field: kind=webd without tls_identity_name set (Null) is
    /// rejected — the bridge needs both fields.
    #[tokio::test]
    async fn cert_source_webd_without_tls_identity_name_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            // tls_identity_name stays Null (defaults).
            m.insert(
                "cert_source".into(),
                cert_source_obj("webd", Some("example.org")),
            );
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(
            err.message().contains(CROSS_FIELD_PREFIX),
            "should be a cross-field error: {}",
            err.message()
        );
        assert!(
            err.message().contains("tls_identity_name"),
            "should mention tls_identity_name: {}",
            err.message()
        );
    }

    /// Patch happy path: prior row carries `tls_identity_name`; Patch
    /// flips `cert_source` from Null → kind=webd. Cross-field check
    /// fires against the merged effective body (Patch over `ctx.old`)
    /// and accepts because `tls_identity_name` survives the merge.
    #[tokio::test]
    async fn patch_cert_source_to_webd_with_prior_tls_identity_accepted() {
        let prior = defaults_obj_mut("example.com", |m| {
            m.insert(
                "tls_identity_name".into(),
                PropValue::String("example.com".into()),
            );
        });
        let mut patch_obj = BTreeMap::new();
        patch_obj.insert("domain".into(), PropValue::String("example.com".into()));
        patch_obj.insert(
            "cert_source".into(),
            cert_source_obj("webd", Some("example.org")),
        );
        let patch = PropValue::Object(patch_obj);
        hooks()
            .before_set(&set_ctx(
                "example.com",
                MergeMode::Patch,
                Some(prior),
                patch,
            ))
            .await
            .expect("Patch to kind=webd with prior tls_identity_name should accept");
    }

    /// Patch rejection: prior row has both `tls_identity_name` and
    /// kind=webd. Patch clears `tls_identity_name` → merged body
    /// keeps kind=webd but loses the local PEM filename → cross-field
    /// invariant fires and rejects the Patch.
    #[tokio::test]
    async fn patch_clearing_tls_identity_with_webd_cert_rejected() {
        let prior = defaults_obj_mut("example.com", |m| {
            m.insert(
                "tls_identity_name".into(),
                PropValue::String("example.com".into()),
            );
            m.insert(
                "cert_source".into(),
                cert_source_obj("webd", Some("example.org")),
            );
        });
        let mut patch_obj = BTreeMap::new();
        patch_obj.insert("domain".into(), PropValue::String("example.com".into()));
        patch_obj.insert("tls_identity_name".into(), PropValue::Null);
        let patch = PropValue::Object(patch_obj);
        let err = hooks()
            .before_set(&set_ctx(
                "example.com",
                MergeMode::Patch,
                Some(prior),
                patch,
            ))
            .await
            .unwrap_err();
        assert!(
            err.message().contains(CROSS_FIELD_PREFIX),
            "should be a cross-field error: {}",
            err.message()
        );
        assert!(
            err.message().contains("tls_identity_name"),
            "should mention tls_identity_name: {}",
            err.message()
        );
    }

    /// Unknown nested field on cert_source is rejected (JsonValuesMapping
    /// stores objects verbatim; the hook is the only enforcer for the
    /// nested object too).
    #[tokio::test]
    async fn cert_source_unknown_nested_field_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            let mut cs = BTreeMap::new();
            cs.insert("kind".into(), PropValue::String("manual".into()));
            cs.insert("vhost_fqdn".into(), PropValue::Null);
            cs.insert("typo".into(), PropValue::String("x".into()));
            m.insert("cert_source".into(), PropValue::Object(cs));
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(
            err.message().contains("cert_source") && err.message().contains("typo"),
            "got: {}",
            err.message()
        );
    }

    /// Bad kind value is rejected at the per-field check (before
    /// cross-field rules fire).
    #[tokio::test]
    async fn cert_source_bad_kind_rejected() {
        let v = defaults_obj_mut("example.com", |m| {
            let mut cs = BTreeMap::new();
            cs.insert("kind".into(), PropValue::String("acme_direct".into()));
            cs.insert("vhost_fqdn".into(), PropValue::Null);
            m.insert("cert_source".into(), PropValue::Object(cs));
        });
        let err = hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .unwrap_err();
        assert!(
            err.message().contains("kind") && err.message().contains("acme_direct"),
            "got: {}",
            err.message()
        );
    }

    /// cert_source omitted entirely is treated as Null (kind=manual).
    /// Path α (`_doc/planned/maild-webd-cert-bridge.md`) says absent ≡
    /// manual; pre-β-1 persisted rows do not carry the key and must
    /// continue to validate so a future Patch over them does not get
    /// stuck on substrate shape it predates. `defaults_value` still
    /// emits explicit Null so fresh-written rows keep the field for
    /// legibility.
    #[tokio::test]
    async fn cert_source_missing_field_treated_as_manual() {
        let v = defaults_obj_mut("example.com", |m| {
            m.remove("cert_source");
        });
        hooks()
            .before_set(&set_ctx("example.com", MergeMode::Replace, None, v))
            .await
            .expect("absent cert_source should validate as kind=manual");
    }

    /// MAJOR-2 (β-1 codex round 3): the substrate's audit-event
    /// projection (`RecordEvent.fields_changed`) MUST surface
    /// `cert_source` when a write transitions it. The dispatcher
    /// projects this directly onto the `<svc>.props.records.changed`
    /// and `<svc>.props.audit` topics (SPEC 12 §5.5/§10), so a missing
    /// entry would silently break downstream subscribers (the bridge
    /// included). Exercises Runtime + MemoryStore — the SqliteStore's
    /// `fields_changed` projection uses the same
    /// `changed_top_level_fields` diff (`crates/cosmix-lib-props-store/src/sqlite.rs`).
    #[tokio::test]
    async fn audit_event_fields_changed_includes_cert_source() {
        use cosmix_props::memory::MemoryStore;
        use cosmix_props::runtime::SetOpts;
        use cosmix_props::store::PropertyStore;

        let store: Arc<dyn PropertyStore> = Arc::new(MemoryStore::new("maild"));
        let spec = spec(Hooks::new(DomainsHooks::new()));
        let runtime = Runtime::new("maild", spec, store);
        let key = key("example.com");

        // Initial Replace at Version::zero() lands the default body
        // (cert_source = Null). We don't inspect this event's diff —
        // a create reports every present key — but the row must exist
        // for the transition write to be an update.
        let create = runtime
            .set(
                key.clone(),
                defaults_value("example.com"),
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::service("test").expect("valid actor"),
                    cause: None,
                    ts_ms: 0,
                },
            )
            .await
            .expect("create defaults_value row");

        // Transition: set tls_identity_name + cert_source.
        // changed_top_level_fields diffs against the prior row, so the
        // event's fields_changed must include both.
        let transition_body = defaults_obj_mut("example.com", |m| {
            m.insert(
                "tls_identity_name".into(),
                PropValue::String("example.com".into()),
            );
            m.insert(
                "cert_source".into(),
                cert_source_obj("webd", Some("example.org")),
            );
        });
        let transition = runtime
            .set(
                key.clone(),
                transition_body,
                SetOpts {
                    expected_version: Some(create.set_event.version),
                    merge: MergeMode::Replace,
                    actor: Actor::service("test").expect("valid actor"),
                    cause: None,
                    ts_ms: 1,
                },
            )
            .await
            .expect("transition write should commit");

        assert!(
            transition
                .set_event
                .fields_changed
                .contains(&"cert_source".to_string()),
            "audit event should report cert_source as changed: {:?}",
            transition.set_event.fields_changed
        );
        assert!(
            transition
                .set_event
                .fields_changed
                .contains(&"tls_identity_name".to_string()),
            "audit event should report tls_identity_name as changed: {:?}",
            transition.set_event.fields_changed
        );
    }

    #[test]
    fn auth_policy_grants_phase2_caps() {
        let caps = auth_policy().resolve(&PeerIdentity::default());
        for want in [
            "props.read:maild.domains",
            "props.write:maild.domains",
            "props.describe:maild.domains:public",
            "props.describe:maild.domains:full",
            "props.audit:maild.domains",
        ] {
            assert!(
                caps.contains(&Capability::new(want).expect("non-empty capability")),
                "missing cap {want}"
            );
        }
        // `props.read:maild.domains:secrets` intentionally absent in
        // Phase 2 — no secret-bucketed fields yet (`dkim_keys` is
        // Phase 4).
        assert!(!caps.contains(
            &Capability::new("props.read:maild.domains:secrets").expect("non-empty capability")
        ));
    }

    #[test]
    fn role_variants_excludes_relay() {
        let v = role_variants();
        assert!(v.contains(&"authoritative".to_string()));
        assert!(v.contains(&"backup_mx".to_string()));
        assert!(!v.contains(&"relay".to_string()));
    }

    #[test]
    fn validate_fqdn_accepts_typical_names() {
        for ok in [
            "example.com",
            "mail.example.com",
            "a.b.c.d.example",
            "xn--bcher-kva.example", // ASCII A-label
        ] {
            assert!(validate_fqdn(ok).is_ok(), "{ok} should validate");
        }
    }

    // ---- Bucket B — reconciliation diagnostic (diff helper only) ----
    //
    // Plan §C — `reconcile_emits_two_sided_diff` capturing tracing output
    // would need a `tracing_test` harness this crate doesn't yet pull in.
    // Until that lands as a follow-up (alongside the broker fake the plan
    // names in §C Phase 2.5), we test the underlying set-difference
    // pure-function, which is the part the diagnostic's correctness
    // hinges on. `Db::distinct_account_domains` is itself a thin
    // sqlite-portable SELECT — covered by live verification step 5
    // rather than a unit test, to avoid pinning a second test-only
    // sqlite fixture in this crate.

    fn bset<I: IntoIterator<Item = &'static str>>(it: I) -> BTreeSet<String> {
        it.into_iter().map(String::from).collect()
    }

    #[test]
    fn reconcile_diff_two_sided() {
        // Three accounts on two domains, two domain rows — one matching,
        // one orphaned, one missing from the namespace.
        let accounts = bset(["one.example", "two.example"]);
        let rows = bset(["one.example", "three.example"]);
        let (missing, orphaned) = compute_reconcile_diff(&accounts, &rows);
        assert_eq!(missing, vec!["two.example".to_string()]);
        assert_eq!(orphaned, vec!["three.example".to_string()]);
    }

    #[test]
    fn reconcile_diff_empty_inputs() {
        let empty: BTreeSet<String> = BTreeSet::new();
        let (missing, orphaned) = compute_reconcile_diff(&empty, &empty);
        assert!(missing.is_empty());
        assert!(orphaned.is_empty());
    }

    #[test]
    fn reconcile_diff_full_overlap_empty_diffs() {
        let s = bset(["one.example", "two.example"]);
        let (missing, orphaned) = compute_reconcile_diff(&s, &s);
        assert!(missing.is_empty());
        assert!(orphaned.is_empty());
    }

    #[test]
    fn reconcile_diff_deterministic_order() {
        // BTreeSet iteration is sorted; the diff vectors inherit that.
        let accounts = bset(["c.example", "a.example", "b.example"]);
        let rows = bset(["a.example"]);
        let (missing, _) = compute_reconcile_diff(&accounts, &rows);
        assert_eq!(
            missing,
            vec!["b.example".to_string(), "c.example".to_string()]
        );
    }

    // ---- Bucket C — CLI body-construction (broker-fake-free) ----
    //
    // The full `main.rs` Bus dispatch path is exercised by the live
    // verification gauntlet (§live-verification step 1); broker-fake
    // integration tests are a Phase 2.5 follow-up.

    #[test]
    fn cli_defaults_value_builds_full_schema_body() {
        // Bucket C `defaults_value_builds_full_schema_body` — the CLI's
        // body construction relies on this returning a complete object
        // keyed by every schema field.
        let v = defaults_value("example.com");
        let obj = v.as_object().expect("object");
        assert_eq!(obj.len(), schema_field_names().len());
        for field in schema_field_names() {
            assert!(
                obj.contains_key(*field),
                "defaults_value missing CLI-required field {field:?}"
            );
        }
    }

    #[test]
    fn cli_validate_fqdn_rejects_malformed_locally() {
        // Bucket C `cli_domain_add_invalid_fqdn_rejected_locally` —
        // the CLI calls `validate_fqdn` before the Bus roundtrip so
        // a typo surfaces without a broker hop.
        assert!(validate_fqdn("bad..name").is_err());
        assert!(validate_fqdn("EXAMPLE.ORG").is_err());
        assert!(validate_fqdn("").is_err());
        assert!(validate_fqdn("example.com").is_ok());
    }

    #[test]
    fn validate_fqdn_rejects_malformed() {
        for bad in [
            "",
            ".example.com",
            "example.com.",
            "example..org",
            "EXAMPLE.org",
            "kan_ary.org",
            "-leading.org",
            "trailing-.org",
        ] {
            assert!(validate_fqdn(bad).is_err(), "{bad} should fail");
        }
        // Long label
        let long_label = "a".repeat(64);
        assert!(validate_fqdn(&long_label).is_err());
        // Long overall
        let long = "a.".repeat(127) + "ab";
        assert!(validate_fqdn(&long).is_err());
    }

    // --- Phase 3 Bucket A: `view` typed-projection tests ---

    fn record_from_value(domain: &str, value: PropValue) -> Record {
        Record {
            key: key(domain),
            value,
            version: Version::zero(),
            nseq: cosmix_props::record::Nseq::zero(),
            lifecycle: None,
        }
    }

    #[test]
    fn view_returns_defaults_value_shape() {
        let rec = record_from_value("example.com", defaults_value("example.com"));
        let v = view(&rec).expect("defaults_value parses");
        assert!(v.enabled);
        assert_eq!(v.role, "authoritative");
        assert_eq!(v.primary_hostname, None);
        assert_eq!(v.helo_identity, None);
        assert_eq!(v.message_id_host, None);
        assert!(v.relay_recipients.is_empty());
        assert_eq!(v.dkim_selector_active, None);
        assert!(v.dkim_selectors.is_empty());
    }

    #[test]
    fn view_extracts_dkim_selector_active() {
        let body = defaults_obj_mut("example.com", |m| {
            m.insert(
                "dkim_selectors".into(),
                PropValue::List(vec![PropValue::String("2026a".into())]),
            );
            m.insert(
                "dkim_selector_active".into(),
                PropValue::String("2026a".into()),
            );
        });
        let rec = record_from_value("example.com", body);
        let v = view(&rec).expect("ok");
        assert_eq!(v.dkim_selector_active, Some("2026a".into()));
        assert_eq!(v.dkim_selectors, vec!["2026a".to_string()]);
    }

    #[test]
    fn view_extracts_dkim_selectors_preserves_order() {
        let body = defaults_obj_mut("example.com", |m| {
            m.insert(
                "dkim_selectors".into(),
                PropValue::List(vec![
                    PropValue::String("2026a".into()),
                    PropValue::String("2026b".into()),
                ]),
            );
        });
        let rec = record_from_value("example.com", body);
        let v = view(&rec).expect("ok");
        assert_eq!(
            v.dkim_selectors,
            vec!["2026a".to_string(), "2026b".to_string()]
        );
        assert_eq!(v.dkim_selector_active, None);
    }

    #[test]
    fn view_rejects_missing_dkim_selectors() {
        let body = defaults_obj_mut("example.com", |m| {
            m.remove("dkim_selectors");
        });
        let rec = record_from_value("example.com", body);
        let err = view(&rec).expect_err("missing dkim_selectors should fail");
        assert!(
            err.to_string().contains("dkim_selectors missing"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn view_extracts_helo_identity() {
        let body = defaults_obj_mut("example.com", |m| {
            m.insert(
                "helo_identity".into(),
                PropValue::String("mail.example.com".into()),
            );
        });
        let rec = record_from_value("example.com", body);
        let v = view(&rec).expect("ok");
        assert_eq!(v.helo_identity, Some("mail.example.com".into()));
    }

    #[test]
    fn view_extracts_primary_hostname() {
        let body = defaults_obj_mut("example.com", |m| {
            m.insert(
                "primary_hostname".into(),
                PropValue::String("mx.example.com".into()),
            );
        });
        let rec = record_from_value("example.com", body);
        let v = view(&rec).expect("ok");
        assert_eq!(v.primary_hostname, Some("mx.example.com".into()));
    }

    #[test]
    fn view_extracts_message_id_host() {
        let body = defaults_obj_mut("example.com", |m| {
            m.insert(
                "message_id_host".into(),
                PropValue::String("msgid.example.com".into()),
            );
        });
        let rec = record_from_value("example.com", body);
        let v = view(&rec).expect("ok");
        assert_eq!(v.message_id_host, Some("msgid.example.com".into()));
    }

    #[test]
    fn view_extracts_relay_recipients() {
        let body = defaults_obj_mut("example.com", |m| {
            m.insert(
                "relay_recipients".into(),
                PropValue::List(vec![
                    PropValue::String("alice@example.com".into()),
                    PropValue::String("bob@example.com".into()),
                ]),
            );
        });
        let rec = record_from_value("example.com", body);
        let v = view(&rec).expect("ok");
        assert_eq!(
            v.relay_recipients,
            vec![
                "alice@example.com".to_string(),
                "bob@example.com".to_string()
            ]
        );
    }

    #[test]
    fn view_rejects_missing_required_field() {
        // Drop `enabled` from a defaults_value-derived body. The typed
        // view refuses to silently default a required schema field —
        // a row in this state is corruption (the hook would have
        // rejected the write).
        let body = defaults_obj_mut("example.com", |m| {
            m.remove("enabled");
        });
        let rec = record_from_value("example.com", body);
        let err = view(&rec).expect_err("missing enabled should fail");
        assert!(
            err.to_string().contains("enabled missing"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn view_rejects_wrong_role_type() {
        let body = defaults_obj_mut("example.com", |m| {
            m.insert("role".into(), PropValue::Int(7));
        });
        let rec = record_from_value("example.com", body);
        let err = view(&rec).expect_err("non-string role should fail");
        assert!(
            err.to_string().contains("role has unexpected variant"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn view_rejects_missing_relay_recipients() {
        let body = defaults_obj_mut("example.com", |m| {
            m.remove("relay_recipients");
        });
        let rec = record_from_value("example.com", body);
        let err = view(&rec).expect_err("missing relay_recipients should fail");
        assert!(
            err.to_string().contains("relay_recipients missing"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn view_rejects_null_relay_recipients() {
        let body = defaults_obj_mut("example.com", |m| {
            m.insert("relay_recipients".into(), PropValue::Null);
        });
        let rec = record_from_value("example.com", body);
        let err = view(&rec).expect_err("null relay_recipients should fail");
        assert!(
            err.to_string()
                .contains("relay_recipients has unexpected variant"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn view_rejects_non_object_body() {
        let rec = record_from_value("example.com", PropValue::String("not-an-object".into()));
        let err = view(&rec).expect_err("non-object body should fail");
        assert!(
            err.to_string().contains("not an Object"),
            "unexpected error: {err}"
        );
    }
}
