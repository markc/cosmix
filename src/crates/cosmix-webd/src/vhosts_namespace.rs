//! SPEC-12 property substrate for webd: the `vhosts` namespace.
//!
//! Phase 3 reshape — this file replaces the v0
//! `props_publisher.rs` projection. The substrate role flips: the
//! namespace is no longer a read-only mirror of runtime vhost state
//! (the `has_cms` / `has_jmap` / `has_ws` / `has_docs` booleans
//! moved out and the ergonomic `webd.routes.list` verb retains that
//! projection surface). The namespace is now the declarative
//! source-of-truth for vhost configuration — the columns mirror
//! `[[webd.vhost]]` rows plus per-fqdn ACME / cert state owned by the
//! daemon.
//!
//! ## Owner column
//!
//! Schema fields split into two ownership classes:
//!
//! * **caller** — `fqdn`, `enabled`, `www_dir`, `aliases`,
//!   `acme_provider`, `acme_challenge`, `acme_contact_email`,
//!   `tls_cert_path`, `tls_key_path`. Writable through caller-facing
//!   `webd.props.set namespace=vhosts`.
//! * **daemon** — `source`, `cert_blob_id`, `key_blob_id`,
//!   `not_after`, `last_attempt`, `last_error_count`, `last_error`.
//!   Writable only via `Runtime::set_with_origin(.., WriteOrigin::backend())`.
//!   The validating hook lands in Commit 2.
//!
//! Three fields are also marked `secret: true` (`cert_blob_id`,
//! `key_blob_id`, `last_error`) so the substrate redacts them without
//! the `props.read:webd.vhosts:secrets` capability.
//!
//! ## C1 scope (commits 1a / 1b / 1c / 1d)
//!
//! Per the converged plan
//! ([`_doc/planned/webd-vhosts-phase3.md`] §"Commit 1" + §"Commit 2"
//! producer half) this file carries:
//!
//! * The full Phase 3 schema (declarative shape; field-level enum +
//!   secret annotations).
//! * `register_vhosts_namespace(router, store)` mirroring the
//!   `register_log_namespace` shape from
//!   [`cosmix-lib-log/src/props.rs:324`]. Constructs the provisioner
//!   event channel internally and returns the `Receiver` to the
//!   caller. No row-seeding — C2-in-this-breakdown (rev-6 plan
//!   §"Commit 3") lands the bootstrap-upsert path that materialises
//!   rows from resolved vhost config.
//! * `VhostsNamespaceHooks` with full lifecycle wiring (landed in
//!   C1c + C1d):
//!   - `before_set` validating rules (C1c): caller writes to
//!     daemon-owned columns, reserved enum variants, multi-SAN
//!     aliases, ACME-vs-manual-TLS mutual exclusion.
//!   - `after_set` enqueues [`NamespaceEvent::VhostSet`] on a bounded
//!     mpsc to the ACME provisioner (C1d). On Full/Closed the event
//!     is dropped + WARN-logged; the C4 recovery contract (snapshot-
//!     reconcile path, lands with consumer arms in §"Commit 4a/4b")
//!     is what makes dropped events recoverable. C1d alone has no
//!     reconcile tick yet — see the bisect contract below.
//!   - `before_delete` reversible preflight only (C1d) — Phase 3 has
//!     no held refs to validate, but the arm exists for future
//!     phases (e.g. a vhost referenced by a CMS-namespace row).
//!   - `after_delete` enqueues [`NamespaceEvent::VhostRemoved`] on
//!     the same mpsc (C1d). Provisioner consumer arms land in
//!     commits §"Commit 4a" / §"Commit 4b" of the rev-6 plan.
//! * Six-cap `AuthPolicy` including the `:secrets` variant and the
//!   `props.write:webd.vhosts` write cap (re-introduced in C1c now
//!   that the validating hook can defend it — see
//!   [`auth_policy`] for the BLOCKER-fix history).
//! * `dispatch_props` — unchanged Bus verb-suffix bridge from the
//!   v0 file.
//!
//! ## Bisect contract — non-deployable mid-commit states
//!
//! Per the rev-6 plan §"Commit sequence" boilerplate: a bisect into
//! C1d sees a node where `webd.props.set namespace=vhosts ...`
//! accepts the row and persists it, but no cert is issued (no
//! provisioner reaction because the consumer arms haven't landed).
//! This is benign for review — reviewers can exercise the hooks +
//! channel wiring without a running provisioner — but it is **not a
//! deployable state**. Operators must wait for C4b for a node they
//! can fully control via Bus.

use std::sync::Arc;

use anyhow::Result;
use cosmix_props::sqlite::{JsonValuesMapping, SqliteStore};
use cosmix_props::{
    AuthPolicy, Capability, CapabilitySet, Cardinality, FieldSchema, FieldType, HookCtx, HookError,
    HookFuture, HookHandler, Hooks, MergeMode, NamespaceName, NamespaceSpec, PeerIdentity,
    PropValue, PropertySchema, PropsRouter, Runtime, StorageBackendKind,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

const NAMESPACE: &str = "vhosts";

/// Daemon-owned columns. A caller-origin `props.set` that names any of
/// these is rejected by [`VhostsNamespaceHooks::before_set`] with the
/// `operator_wrote_daemon_field` validation error. Backend-origin
/// writes (`Runtime::set_with_origin(.., WriteOrigin::backend())`)
/// bypass the check unchanged — that's the route the bootstrap
/// upsert and the provisioner writeback use. Listed in the order
/// the rev-6 plan §"Owner table" pins, so a regression that drops a
/// field reads as a single-row diff against the doc.
const DAEMON_OWNED_FIELDS: &[&str] = &[
    "source",
    "cert_blob_id",
    "key_blob_id",
    "not_after",
    "last_attempt",
    "last_error_count",
    "last_error",
];

/// Wire prefix the CLI parses against the validation message body —
/// Phase 1's `validation_error` payload has no typed `error_code`
/// slot for daemon-specific codes, so the prefix is the operator's
/// affordance. Same pattern as maild's
/// `account_overrides::ORPHAN_OVERRIDE_PREFIX`.
const OPERATOR_WROTE_DAEMON_FIELD_PREFIX: &str = "operator_wrote_daemon_field";

/// Bounded mpsc capacity for the provisioner event channel —
/// startup-burst headroom plus a margin. C3's bootstrap upsert walks
/// every `[[webd.vhost]]` config row at startup and fires one
/// `VhostSet` event per row; with the substrate's per-row write
/// latency this is the worst-case burst. 128 absorbs the typical
/// deployment size (≤ a few dozen vhosts per webd) plus operator
/// `vhost.add` traffic without backpressure, but stays small enough
/// that sustained backpressure surfaces immediately as the drop
/// telemetry the operator can see in the WARN log.
///
/// **Drop recovery is the C4 contract, not the C1d behaviour.** Until
/// C4a/C4b wire the consumer arms, the receiver is parked in main.rs
/// and no reconcile tick runs against this channel — a dropped event
/// at C1d stays dropped until the consumer side lands. C4's
/// `run_forever` snapshot-reconcile path is the recovery mechanism
/// the chosen capacity assumes: the namespace is the durable source
/// of truth, and a dropped event recovers via the next provisioner
/// reconcile tick (which reads `webd.vhosts` via `props.list` and
/// diffs against `self.plans`). The bisect contract in the module
/// docstring is what makes this difference safe — the C1d-only build
/// is explicitly not deployable.
pub const PROVISIONER_EVENT_CHANNEL_CAPACITY: usize = 128;

/// Events published by the `webd.vhosts` namespace's lifecycle hooks
/// for the ACME provisioner observer to consume.
///
/// **C1d scope.** Producer-side only: [`VhostsNamespaceHooks::after_set`]
/// emits `VhostSet` after a row commits durably, and
/// [`VhostsNamespaceHooks::after_delete`] emits `VhostRemoved` after a
/// tombstone commits. The matching `select!` arms on the provisioner's
/// `run_forever` loop land in commits C2 / C3 (rev-6 plan §"Commit
/// 4a" / §"Commit 4b"). Until then the bounded `mpsc::Receiver` is
/// held by `main.rs` to keep the channel open; events that fit in
/// the buffer pile up and the rest are dropped + warned. **The
/// substrate is the source of truth** — the C4 recovery contract
/// (see [`PROVISIONER_EVENT_CHANNEL_CAPACITY`]) makes at-most-once
/// delivery with reconcile-on-drop the published wire contract. At
/// C1d alone the recovery half does not run yet; the bisect contract
/// further down is what makes shipping the producer-only build safe.
///
/// The `row` is carried as a boxed [`VhostRow`] (not raw `PropValue`)
/// so consumers don't repeat the serde round-trip per event; the
/// allocation is amortised against the ACME issuance work that
/// follows. `Box` keeps the enum variant size from dominating the
/// `mpsc::Receiver`'s ringbuffer — `VhostRow` is a wide struct
/// (≥20 Option fields), so an unboxed variant would balloon the
/// queue footprint.
///
/// `dead_code` allow: C1d lands the producer (after_set / after_delete
/// constructs these variants); the consumer arms (rev-6 plan §"Commit
/// 4a" / §"Commit 4b") pattern-match on the fields then. Without the
/// allow, every cargo build between C1d and the C4a/b lift generates
/// noise for fields the plan intentionally stages.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum NamespaceEvent {
    /// A vhost row was created or updated. Carries the post-commit
    /// merged row (Patch + old, or Replace verbatim), so consumers
    /// see the same final state the substrate persisted — never the
    /// raw patch fragment a Patch body contained.
    VhostSet {
        /// The canonical FQDN — equal to the record key.
        fqdn: String,
        /// Full committed row, post-merge.
        row: Box<VhostRow>,
    },
    /// A vhost row was deleted (tombstone committed). The provisioner
    /// observer arm (C4b) runs the idempotent cleanup: archive
    /// `acme/<fqdn>/live/` to `acme/<fqdn>/archive/<ts>/`, drop the
    /// plan, rebuild the SNI resolver arc-swap. Cleanup is
    /// state-driven not event-driven, so by the C4 recovery contract
    /// (see [`PROVISIONER_EVENT_CHANNEL_CAPACITY`]) a dropped
    /// `VhostRemoved` recovers on the next snapshot-reconcile tick
    /// (which detects "plan in memory but row gone from the
    /// namespace" and runs the cleanup branch). At C1d alone the
    /// reconcile tick has not landed, so the contract is named
    /// post-C4 behaviour, not C1d behaviour.
    VhostRemoved {
        /// The FQDN of the deleted row.
        fqdn: String,
    },
}

/// `acme_provider` enum variants — `null` is encoded as the absent
/// field rather than a string variant, so the surface stays
/// `letsencrypt_prod | letsencrypt_staging`.
fn acme_provider_variants() -> Vec<String> {
    vec!["letsencrypt_prod".into(), "letsencrypt_staging".into()]
}

/// `acme_challenge` enum variants — `dns01` is reserved-but-refused
/// until a later phase; C2's validating hook rejects it explicitly.
fn acme_challenge_variants() -> Vec<String> {
    vec!["http01".into()]
}

/// `source` enum variants — stamped by the daemon to mark whether a
/// row originated from bootstrap config materialisation or from an
/// Bus-runtime write. Caller-direct writes that include this field
/// are rejected by the C2 hook.
fn source_variants() -> Vec<String> {
    vec!["config_bootstrap".into(), "bus_runtime".into()]
}

/// Rust projection of one substrate row, used for serde round-trip
/// tests and downstream Rust consumers that prefer typed access over
/// reaching into `PropValue::Object`. `#[serde(deny_unknown_fields)]`
/// guards against a future schema bump silently accepting a row from
/// an older daemon version that carries an unknown column.
///
/// `dead_code` allow: C1 lands the projection + serde tests; the
/// non-test consumers (C2's validating hook, C3's bootstrap-upsert
/// path) wire in then. Without the allow, every cargo build between
/// C1 and C3 generates noise for items the plan intentionally stages.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VhostRow {
    pub fqdn: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub www_dir: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Daemon-stamped. `None` until the first backend-origin write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acme_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acme_challenge: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acme_contact_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_cert_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_key_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cert_blob_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_blob_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_attempt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_count: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[allow(dead_code)]
fn default_enabled() -> bool {
    true
}

/// Phase 3 schema. Field order matches the plan's owner-table column
/// order; daemon-owned + secret annotations land here, validation
/// hooks land in C2.
pub fn schema() -> PropertySchema {
    PropertySchema::new(vec![
        FieldSchema {
            name: "fqdn".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Canonical primary FQDN — also the substrate row's primary key.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "enabled".into(),
            ty: FieldType::Bool,
            default: Some(PropValue::Bool(true)),
            secret: false,
            help: "Default true. Disabled vhosts skip dispatch and skip ACME ticks.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "www_dir".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Required. Path absolute or rooted at webd.www_root.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "aliases".into(),
            ty: FieldType::List {
                item: Box::new(FieldType::String),
            },
            default: Some(PropValue::List(Vec::new())),
            secret: false,
            help: "Reserved for multi-SAN. Non-empty rejected until that phase lands.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "source".into(),
            ty: FieldType::Enum {
                variants: source_variants(),
            },
            default: None,
            secret: false,
            help: "Daemon-owned. config_bootstrap | bus_runtime. Stamped via \
                   WriteOrigin::backend() path only."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "acme_provider".into(),
            ty: FieldType::Enum {
                variants: acme_provider_variants(),
            },
            default: None,
            secret: false,
            help: "letsencrypt_prod | letsencrypt_staging. Absent = no ACME for this vhost.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "acme_challenge".into(),
            ty: FieldType::Enum {
                variants: acme_challenge_variants(),
            },
            default: None,
            secret: false,
            help: "http01 only (dns01 reserved). Absent = no ACME for this vhost.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "acme_contact_email".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Required when acme_provider set.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "tls_cert_path".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Phase-1-style manual cert. Mutually exclusive with acme_*.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "tls_key_path".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Pairs with tls_cert_path.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "cert_blob_id".into(),
            ty: FieldType::String,
            default: None,
            secret: true,
            help: "Daemon-owned. Opaque path to acme/<fqdn>/live/fullchain.pem.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "key_blob_id".into(),
            ty: FieldType::String,
            default: None,
            secret: true,
            help: "Daemon-owned. Opaque path to acme/<fqdn>/live/privkey.pem.".into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "not_after".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Daemon-owned. RFC 3339 cert expiry from the most recent issuance/renewal. \
                   Non-secret — cert expiry leaks via the TLS handshake anyway."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "last_attempt".into(),
            ty: FieldType::String,
            default: None,
            secret: false,
            help: "Daemon-owned. RFC 3339 timestamp of the last provisioner tick against this \
                   vhost. Non-secret — operator-observable status."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "last_error_count".into(),
            ty: FieldType::I64,
            default: None,
            secret: false,
            help: "Daemon-owned. Backoff counter (1..5 maps to BACKOFF_SCHEDULE). \
                   Non-secret — a single integer, no diagnostic surface."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
        FieldSchema {
            name: "last_error".into(),
            ty: FieldType::String,
            default: None,
            secret: true,
            help: "Daemon-owned. Last issuance failure message. Secret — ACME error bodies can \
                   carry response payloads, account context, or rate-limit URLs."
                .into(),
            since: None,
            until: None,
            validators: Vec::new(),
        },
    ])
}

/// Capability surface per plan §"Capability split". For v0.1 every
/// reachable peer (WG `/24` trust domain) gets the issued set,
/// matching the maild Phase 1/2/3 posture; cross-mesh authz narrows
/// later without renaming the caps.
///
/// **C1c surface — read + write, validated.** C1b withheld
/// `props.write:webd.vhosts` because [`VhostsNamespaceHooks`] was a
/// no-op stub: an open write cap against an empty hook would have
/// let any in-mesh caller persist Phase 3 rows that bypass every
/// intended invariant. C1c lands the validating `before_set`
/// (daemon-owned column rejection on caller-origin, reserved enum
/// rejection, multi-SAN aliases rejection, ACME-vs-manual-TLS
/// mutual exclusion) and re-introduces the write cap in the same
/// commit — closing the substrate's "schema-vs-hook" race window
/// inside one diff so a bisect can never land on a node where the
/// cap is open and the hook is empty. Pin-tested by
/// [`tests::auth_policy_grants_write_cap_with_validating_hook`].
///
/// Backend-origin writes (`Runtime::set_with_origin(_,
/// WriteOrigin::backend())`) are unaffected — those are direct Rust
/// API calls that do not flow through `AuthPolicy`. C3's
/// bootstrap-upsert path uses that route. The validating hook
/// branches on `ctx.origin`: caller writes that name a daemon-owned
/// column are rejected with `operator_wrote_daemon_field`, while
/// backend-origin writes bypass that arm — the cross-field rules
/// (rules 2-5) run on every write regardless of origin.
pub fn auth_policy(service: &str) -> AuthPolicy {
    let read_public = Capability::from(format!("props.read:{service}.vhosts"));
    let read_secrets = Capability::from(format!("props.read:{service}.vhosts:secrets"));
    let describe_public = Capability::from(format!("props.describe:{service}.vhosts:public"));
    let describe_full = Capability::from(format!("props.describe:{service}.vhosts:full"));
    let audit = Capability::from(format!("props.audit:{service}.vhosts"));
    let write = Capability::from(format!("props.write:{service}.vhosts"));
    // C5 — narrow cap gating the `webd.acme.renew` ergonomic verb
    // ([`crate::bus::vhost_verbs`]). NOT a `props.*` cap because the
    // verb is daemon-internal kick semantics, not a substrate row
    // mutation — the post-renewal namespace writeback runs under
    // webd's own service-tier credential rather than the caller's.
    // Naming follows the spec's "cross-mesh authz narrows later
    // without renaming caps" rule: the wire shape stays stable when
    // the policy graduates from "every WG peer" to a tighter scope.
    let acme_renew = Capability::from(format!("webd.acme.renew:{service}.vhosts"));
    // Narrow cap gating the `webd.session.revoke` ergonomic verb
    // ([`crate::bus::session_verbs`]) — cookie-path session revocation
    // (2026-07 audit). Same non-`props.*` rationale as `acme_renew`:
    // it mutates runtime security state (the per-vhost `session_epochs`
    // table), not a substrate row.
    let session_revoke = Capability::from(format!("webd.session.revoke:{service}.vhosts"));
    let caps: CapabilitySet = [
        read_public,
        read_secrets,
        describe_public,
        describe_full,
        audit,
        write,
        acme_renew,
        session_revoke,
    ]
    .into_iter()
    .collect();
    AuthPolicy::new(move |_peer: &PeerIdentity| caps.clone())
}

/// Build the `NamespaceSpec` — collection cardinality keyed on `fqdn`,
/// SqliteTable storage on the shared `__props_values` table,
/// validating hooks landed in C1c.
///
/// **`require_version = true`** is a security boundary, not just an
/// ergonomic preference. The substrate fetches `ctx.old` once at
/// hook time (runtime.rs:251) and the SQLite commit re-reads + merges
/// under the storage transaction (sqlite.rs:907-908). Without an
/// optimistic-concurrency anchor, two concurrent caller patches can
/// both pass `before_set` against the same snapshot and combine into
/// a row that violates mutual exclusion (e.g. patch A adds
/// `acme_*`, patch B adds `tls_cert_path`, both see a disabled
/// no-mode prior, both commit, the second wins and persists ACME +
/// manual TLS together). Requiring `if_version` collapses the race
/// to optimistic-concurrency failure on the second writer; the
/// caller refetches, the hook re-validates against the merged
/// reality. This is the SPEC 12 §4.4 guidance for namespaces with
/// automation writers — webd's ACME provisioner is exactly that.
/// Pinned by [`tests::caller_set_without_if_version_rejected`].
pub fn spec(service: &str, hooks: Hooks) -> NamespaceSpec {
    let mut s = NamespaceSpec::new(
        namespace_name(),
        schema(),
        Cardinality::Collection {
            primary_key_field: "fqdn".to_string(),
        },
        StorageBackendKind::SqliteTable {
            table: "__props_values".into(),
        },
    );
    s.auth = auth_policy(service);
    s.hooks = hooks;
    s.require_version = true;
    s
}

/// Canonical [`NamespaceName`] for the `webd.vhosts` substrate
/// namespace. `pub(crate)` so the ACME provisioner (C4a) can construct
/// [`cosmix_props::RecordKey::collection`] values for the daemon-side
/// writeback of `cert_blob_id` / `key_blob_id` / `not_after` columns
/// after a successful fresh-issuance, without re-hardcoding the
/// namespace string.
pub(crate) fn namespace_name() -> NamespaceName {
    NamespaceName::new(NAMESPACE).expect("constant namespace name is valid")
}

/// Hook handler for the `webd.vhosts` namespace.
///
/// Carries an `mpsc::Sender<NamespaceEvent>` to the ACME provisioner
/// (consumer arms land in commits §"Commit 4a" / §"Commit 4b" of the
/// rev-6 plan). C1d-shipped state: producer-side fully wired, hooks
/// fire `try_send` (never `.send().await` — never block the
/// substrate's commit thread on a slow provisioner). Drops on
/// Full/Closed are logged via `tracing::warn!`; the C4 recovery
/// contract (see [`PROVISIONER_EVENT_CHANNEL_CAPACITY`]) makes the
/// snapshot-reconcile path the durable recovery once consumer arms
/// land. At C1d alone the channel has no consumer and no reconcile
/// tick runs against it — namespace state is still canonical, but
/// recovery from a drop awaits C4.
///
/// ### `before_set` rules (C1c)
///
/// 1. **Daemon-owned columns are caller-immutable.** When
///    `ctx.origin.is_caller()`, naming any of [`DAEMON_OWNED_FIELDS`]
///    in the body returns
///    `validation_error: operator_wrote_daemon_field: <field>`. The
///    backend-origin path (`Runtime::set_with_origin(..,
///    WriteOrigin::backend())`) bypasses this check because the
///    daemon legitimately owns those columns — that's the route
///    C3's bootstrap upsert and the ACME provisioner writeback use.
///
/// 2. **`fqdn` body field matches the record key.** Prevents
///    `webd.props.set namespace=vhosts key=a.example` with body
///    `{"fqdn":"b.example"}` from writing a row whose key disagrees
///    with the primary-key field.
///
/// 3. **`aliases` reserved for multi-SAN.** Non-empty rejected with
///    a Phase-4 pointer; legitimate today is an absent or empty
///    list.
///
/// 4. **`acme_challenge='dns01'` reserved.** Rejected with a
///    Phase-4 pointer; the schema enum currently only lists
///    `http01` but lib-props' enum enforcement is the placeholder
///    shape (see `namespace.rs:20`), so the hook is the real
///    enforcement point and must stay until validators wire in.
///
/// 5. **ACME-vs-manual-TLS mutual exclusion.** Computed against
///    the *merged effective row* (so a Patch that flips
///    `tls_cert_path` against a row already carrying `acme_*`
///    rejects). Rules:
///    - Both `acme_provider` AND `tls_cert_path` set → reject (both).
///    - `acme_provider` set requires `acme_challenge` + non-empty
///      `acme_contact_email`.
///    - `tls_cert_path` set requires `tls_key_path`.
///    - Enabled vhosts (the default) must have one of the two
///      modes configured. Disabled vhosts may have neither so the
///      operator can stage a row before configuring it.
///
/// All rejection messages are constructed via
/// [`HookError::validation`] so they project to the
/// `validation_error` wire code per SPEC 12 §6.5. Cross-field rules
/// run against the *merged effective row* (Patch + old, or full
/// Replace) so a partial body cannot sneak past by carrying only
/// one half of the violation.
///
/// ### `after_set` (C1d)
///
/// Constructs [`NamespaceEvent::VhostSet`] from `ctx.new` (post-merge
/// committed row) and `try_send`s it on the bounded channel. The
/// committed row is guaranteed to deserialise to [`VhostRow`] because
/// `before_set` + lib-props' schema validators already ran; a serde
/// failure here would indicate a substrate bug and is logged + the
/// event dropped. On `try_send` Full/Closed, log + drop. Returns
/// `Ok(())` regardless — `after_set` failures don't roll back the
/// durable write (SPEC 12 §6.5 line 1066-1068), and the substrate is
/// the source of truth.
///
/// ### `before_delete` (C1d)
///
/// **Reversible preflight only.** Phase 3 has no held references to
/// validate (a vhost row deletion has no foreign-key children today),
/// so this arm returns `Ok(())` unconditionally. The arm exists for
/// future phases (e.g. CMS-namespace row referencing a vhost FQDN)
/// where deletes must check held refs without touching destructive
/// state.
///
/// **Crucial invariant:** destructive cleanup (cert archive, SNI
/// resolver flip) is deferred to `after_delete`. The rev-1 BLOCKER
/// shape put destructive work in `before_delete` and allowed the
/// SQLite delete to roll back on hook timeout — that creates an
/// unrecoverable state (cert archived, SNI flipped, row still
/// present). Moving destructive work to `after_delete` with
/// idempotent recovery means the row is gone first (the durable
/// source of truth), and cleanup converges to the right state from
/// any partial-failure point.
///
/// ### `after_delete` (C1d)
///
/// Constructs [`NamespaceEvent::VhostRemoved`] from `ctx.key.key` and
/// `try_send`s it. Same Full/Closed handling as `after_set`. Returns
/// `Ok(())` regardless: a propagated error here would surface as a
/// delete-RPC failure to the caller AFTER the row is already gone —
/// the wrong UX. By the C4 recovery contract (see
/// [`PROVISIONER_EVENT_CHANNEL_CAPACITY`]) the provisioner's
/// snapshot-reconcile path detects "plan in memory but row gone
/// from the namespace" and runs the cleanup branch on the next
/// tick, so dropped events recover via state-driven
/// reconciliation. At C1d alone that reconcile arm has not landed;
/// the bisect contract in the module doc is what makes the
/// producer-only build safe in the meantime.
pub struct VhostsNamespaceHooks {
    events_tx: mpsc::Sender<NamespaceEvent>,
}

impl VhostsNamespaceHooks {
    /// Construct a hook handler that emits [`NamespaceEvent`]s on the
    /// supplied bounded sender. The receiver lives in `main.rs` for
    /// C1d (parked binding); commits §"4a"/§"4b" of the rev-6 plan
    /// move it into the provisioner's `run_forever` `select!`.
    pub fn new(events_tx: mpsc::Sender<NamespaceEvent>) -> Self {
        Self { events_tx }
    }
}

/// Materialise the effective row a `before_set` should validate
/// against. For `Replace` (or any create-new where `ctx.old` is
/// absent) the body IS the effective row. For `Patch` against an
/// existing Object, shallow-merge the body on top of the prior
/// (mirroring `cosmix_props::sqlite::merge_patch`). Returns
/// `None` if the body isn't an Object — the caller rejects that
/// case with its own typed error.
fn effective_row(ctx: &HookCtx) -> Option<BTreeMap<String, PropValue>> {
    let new_obj = match &ctx.new {
        Some(PropValue::Object(m)) => m,
        _ => return None,
    };
    match (&ctx.merge, &ctx.old) {
        (Some(MergeMode::Patch), Some(PropValue::Object(old_obj))) => {
            let mut merged = old_obj.clone();
            for (k, v) in new_obj {
                merged.insert(k.clone(), v.clone());
            }
            Some(merged)
        }
        _ => Some(new_obj.clone()),
    }
}

/// Extract a `&str` from an Object field, returning `None` if the
/// field is absent OR the value is not a `String`. The "wrong type"
/// case is left for lib-props' schema typecheck — the hook only
/// cares whether the field exists with a usable shape.
fn field_as_str<'a>(obj: &'a BTreeMap<String, PropValue>, key: &str) -> Option<&'a str> {
    match obj.get(key) {
        Some(PropValue::String(s)) => Some(s.as_str()),
        _ => None,
    }
}

impl HookHandler for VhostsNamespaceHooks {
    fn before_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            // The body must be an Object — a non-Object on a
            // Collection-cardinality namespace is a wire shape the
            // substrate would reject downstream anyway; the typed
            // error here surfaces the violation at the hook instead
            // of in commit_set.
            let new_obj = match &ctx.new {
                Some(PropValue::Object(m)) => m,
                Some(other) => {
                    return Err(HookError::validation(format!(
                        "webd.vhosts row must be an Object, got {}",
                        other.type_name(),
                    )));
                }
                None => {
                    // `before_set` is always invoked with `Some(value)`
                    // (runtime.rs constructs `new: Some(value.clone())`);
                    // this arm is defensive.
                    return Ok(());
                }
            };

            // Rule 1 — daemon-owned columns. Caller-origin writes
            // never touch these; backend-origin (bootstrap, vhost.add,
            // provisioner writeback) bypasses unchanged.
            if ctx.origin.is_caller() {
                for &field in DAEMON_OWNED_FIELDS {
                    if new_obj.contains_key(field) {
                        return Err(HookError::validation(format!(
                            "{OPERATOR_WROTE_DAEMON_FIELD_PREFIX}: \
                             {field} is owned by webd and cannot be set through \
                             props.set; the daemon writes it via the \
                             Runtime::set_with_origin backend path (bootstrap, \
                             webd.vhost.add, ACME provisioner writeback)",
                        )));
                    }
                }
            }

            // Rule 2 — `fqdn` body field, if present, must match the
            // record key. Patch bodies that omit `fqdn` skip this
            // rule; a Replace body for a Collection namespace is
            // already required by lib-props' validate_primary_key
            // to carry a matching PK field, but the hook duplicates
            // the check to make the error reach the caller with the
            // wire vocabulary the operator sees in CLI output.
            if let Some(fqdn) = field_as_str(new_obj, "fqdn")
                && fqdn != ctx.key.key.as_str()
            {
                return Err(HookError::validation(format!(
                    "fqdn body field {fqdn:?} does not match record key {:?}; \
                     the substrate uses the wire key as the canonical fqdn — \
                     either omit the body field or align them",
                    ctx.key.key,
                )));
            }

            // Rule 3 — `aliases` reserved for multi-SAN.
            if let Some(PropValue::List(items)) = new_obj.get("aliases")
                && !items.is_empty()
            {
                return Err(HookError::validation(
                    "aliases is reserved for multi-SAN issuance and must be \
                     empty until that phase lands; see \
                     _doc/planned/webd-vhosts.md §multi-SAN",
                ));
            }

            // Rule 4 — `acme_challenge='dns01'` reserved for Phase 4.
            if let Some("dns01") = field_as_str(new_obj, "acme_challenge") {
                return Err(HookError::validation(
                    "acme_challenge='dns01' is reserved for Phase 4 (DNS-01 \
                     challenge flow) and not yet implemented; webd issues \
                     http-01 challenges only in Phase 3",
                ));
            }

            // Rule 5 — mutual exclusion + required-paired-fields,
            // evaluated against the merged effective row so a Patch
            // can't sneak past by carrying only one half of the
            // violation.
            let effective = effective_row(ctx).expect(
                "ctx.new was already validated as Object above; \
                 effective_row must produce Some",
            );

            let acme_provider = field_as_str(&effective, "acme_provider");
            let acme_challenge = field_as_str(&effective, "acme_challenge");
            let acme_email = field_as_str(&effective, "acme_contact_email");
            let tls_cert_path = field_as_str(&effective, "tls_cert_path");
            let tls_key_path = field_as_str(&effective, "tls_key_path");
            let enabled = match effective.get("enabled") {
                Some(PropValue::Bool(b)) => *b,
                _ => true, // schema default
            };

            let has_acme = acme_provider.is_some();
            let has_manual_tls = tls_cert_path.is_some();

            // 5a — both modes set: ambiguous, reject.
            if has_acme && has_manual_tls {
                return Err(HookError::validation(
                    "acme_provider and tls_cert_path are mutually exclusive; \
                     pick ACME-issued certs OR an operator-managed pair per vhost",
                ));
            }

            // 5b — enabled vhost must have one mode. Disabled rows
            // can be staged with neither (operator authoring path).
            if enabled && !has_acme && !has_manual_tls {
                return Err(HookError::validation(
                    "enabled vhost must configure either acme_provider + \
                     acme_challenge + acme_contact_email, OR tls_cert_path + \
                     tls_key_path; set enabled=false to stage a row before \
                     configuring it",
                ));
            }

            // 5c — ACME mode requires its companion fields.
            if has_acme {
                if acme_challenge.is_none() {
                    return Err(HookError::validation(
                        "acme_provider set requires acme_challenge (currently \
                         http01 only)",
                    ));
                }
                if acme_email.map(str::is_empty).unwrap_or(true) {
                    return Err(HookError::validation(
                        "acme_provider set requires a non-empty \
                         acme_contact_email (used as the ACME account contact)",
                    ));
                }
            }

            // 5d — manual TLS requires both halves of the pair.
            if has_manual_tls && tls_key_path.is_none() {
                return Err(HookError::validation(
                    "tls_cert_path set requires tls_key_path",
                ));
            }

            // Rule 6 — row-shape gate. The merged effective row MUST
            // deserialise to the typed `VhostRow`. The substrate's
            // FieldSchema validators are still placeholders (validators
            // vectors are empty in `schema()` above), so without this
            // check a caller could submit `{fqdn: "x", enabled: false}`
            // — passes all cross-field rules (5b allows disabled with
            // no TLS mode), commits to storage, but is missing
            // `www_dir` (a non-optional VhostRow field). `after_set`
            // would then fail deserialisation and silently drop the
            // producer event while the storage row sits orphaned. The
            // bisect contract (no corruption between substages) needs
            // this gate to hold *before* the storage commit.
            //
            // Built against the merged effective row so a Patch can't
            // smuggle an invalid final shape past a previously-complete
            // baseline (lib-props applies the same merge for the
            // commit; we mirror it here so the check evaluates exactly
            // what storage will persist).
            //
            // `deny_unknown_fields` on VhostRow is the second gate this
            // closes — an operator typo like `acme_provdier` would
            // commit cleanly today and re-surface as a soft-error log
            // in `after_set`.
            let effective_value = PropValue::Object(effective);
            if let Err(e) = vhost_row_from_value(&effective_value) {
                return Err(HookError::validation(format!(
                    "row shape rejected before commit: {e}. \
                     This guards the after_set producer; a row that \
                     reaches storage but fails VhostRow deserialisation \
                     would emit no provisioner event and sit orphaned. \
                     See vhosts_namespace.rs::VhostRow for the required \
                     field set and `deny_unknown_fields` rule",
                )));
            }

            Ok(())
        })
    }

    fn after_set<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            // Deserialise the post-commit row to the typed VhostRow.
            // `ctx.new` here is the substrate's merged final value
            // (runtime.rs:313 sets `new: Some(record.value.clone())`
            // where `record.value` is `commit_set`'s final_value
            // post-Patch-merge); both Replace and Patch land here as
            // the full row. With C1c rule 6 (row-shape gate in
            // before_set) this serde call cannot fail on legitimate
            // substrate state — a failure here indicates a substrate
            // bug (validator/storage divergence), so log + drop the
            // event. The C4 recovery contract reconciles the orphan
            // once consumer arms land; at C1d alone it stays
            // orphaned until C4, which is the bisect-contract trade.
            let value = match ctx.new.as_ref() {
                Some(v) => v,
                None => {
                    // runtime.rs always passes Some on after_set;
                    // defensive.
                    return Ok(());
                }
            };
            let row = match vhost_row_from_value(value) {
                Ok(r) => Box::new(r),
                Err(e) => {
                    tracing::warn!(
                        target: "webd::vhosts_namespace",
                        fqdn = %ctx.key.key,
                        error = %e,
                        "after_set: committed row failed VhostRow deserialisation \
                         despite before_set rule 6 row-shape gate; substrate bug — \
                         dropping provisioner event (recovery awaits the C4 \
                         snapshot-reconcile contract; see \
                         PROVISIONER_EVENT_CHANNEL_CAPACITY)",
                    );
                    return Ok(());
                }
            };

            let event = NamespaceEvent::VhostSet {
                fqdn: ctx.key.key.to_string(),
                row,
            };

            // try_send: NEVER block the substrate's commit thread on
            // a slow provisioner. On Full/Closed the event is dropped
            // + logged; the namespace is the durable source of truth
            // and the C4 recovery contract (snapshot-reconcile path,
            // see PROVISIONER_EVENT_CHANNEL_CAPACITY) is the recovery
            // mechanism once consumer arms land.
            match self.events_tx.try_send(event) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(
                        target: "webd::vhosts_namespace",
                        fqdn = %ctx.key.key,
                        capacity = PROVISIONER_EVENT_CHANNEL_CAPACITY,
                        "after_set: provisioner event channel full — VhostSet dropped; \
                         recovery is the C4 contract (snapshot-reconcile reads \
                         webd.vhosts via props.list once consumer arms land and \
                         diffs against self.plans)",
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    // The receiver is held by `main.rs` for daemon
                    // lifetime; if it drops mid-flight the
                    // provisioner cannot react until daemon restart.
                    // Surface as ERROR (not WARN) — this is a
                    // wiring-time bug, not normal backpressure.
                    tracing::error!(
                        target: "webd::vhosts_namespace",
                        fqdn = %ctx.key.key,
                        "after_set: provisioner event channel closed — receiver dropped; \
                         namespace state is canonical but the provisioner cannot react \
                         until daemon restart. This indicates a startup-wiring bug.",
                    );
                }
            }
            Ok(())
        })
    }

    fn before_delete<'a>(&'a self, _ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async {
            // Reversible preflight only. Phase 3 has no held refs to
            // validate (a vhost row deletion has no FK children
            // today), so this arm returns Ok unconditionally. The
            // arm exists for future phases where a CMS-namespace row
            // might reference a vhost FQDN and require a "delete the
            // CMS row first" preflight.
            //
            // CRUCIAL: destructive cleanup (cert archive, SNI
            // resolver flip) is in `after_delete`, not here — see
            // the struct docstring for the rev-1 BLOCKER rationale.
            Ok(())
        })
    }

    fn after_delete<'a>(&'a self, ctx: &'a HookCtx) -> HookFuture<'a, ()> {
        Box::pin(async move {
            let event = NamespaceEvent::VhostRemoved {
                fqdn: ctx.key.key.to_string(),
            };

            // try_send: see after_set for the rationale. Same
            // Full/Closed handling. Returns Ok(()) regardless — a
            // propagated error here would surface to the caller as a
            // delete-RPC failure AFTER the row is gone, which is the
            // wrong UX. By the C4 recovery contract the provisioner's
            // snapshot-reconcile path detects "plan in memory but row
            // gone from the namespace" and runs the cleanup branch;
            // at C1d alone that reconcile arm has not landed.
            match self.events_tx.try_send(event) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    tracing::warn!(
                        target: "webd::vhosts_namespace",
                        fqdn = %ctx.key.key,
                        capacity = PROVISIONER_EVENT_CHANNEL_CAPACITY,
                        "after_delete: provisioner event channel full — VhostRemoved \
                         dropped; recovery is the C4 contract (snapshot-reconcile \
                         detects 'plan in memory but row gone' once consumer arms \
                         land and runs the cleanup branch — archive \
                         acme/<fqdn>/live/, drop plan, rebuild arc-swap)",
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    tracing::error!(
                        target: "webd::vhosts_namespace",
                        fqdn = %ctx.key.key,
                        "after_delete: provisioner event channel closed — receiver \
                         dropped; cert artefacts at acme/<fqdn>/live/ leak until \
                         daemon restart. This indicates a startup-wiring bug.",
                    );
                }
            }
            Ok(())
        })
    }
}

/// Wire the `vhosts` namespace into the supplied router + store and
/// construct the provisioner event channel.
///
/// Idempotent against the same `(service, store)` pair through
/// `SqliteStore::register_namespace`. The returned `Arc<Runtime>` is
/// held by the caller for the lifetime of the process — the
/// per-runtime dispatcher in [`crate::bus::run`] subscribes to it.
///
/// The returned `mpsc::Receiver<NamespaceEvent>` is the consumer end
/// of the bounded channel the hooks emit on; C1d parks it in
/// `main.rs` (binding held for daemon lifetime) so the channel stays
/// open and `try_send` only returns `Full` (never `Closed`) under
/// normal operation. Commits §"Commit 4a" / §"Commit 4b" of the
/// rev-6 plan move the receiver into the ACME provisioner's
/// `run_forever` `select!`.
///
/// **No row seeding** — C2's bootstrap upsert path (rev-6 §"Commit
/// 3") materialises rows from resolved vhost config via
/// `Runtime::set_with_origin(.., WriteOrigin::backend())`. Until
/// that lands, `webd.props.list namespace=vhosts` returns an empty
/// list; the ergonomic `webd.routes.list` continues to project the
/// runtime vhost map.
pub fn register_vhosts_namespace(
    router: &mut PropsRouter,
    store: &Arc<SqliteStore>,
) -> Result<(Arc<Runtime>, mpsc::Receiver<NamespaceEvent>)> {
    let service = router.service().to_string();
    let (events_tx, events_rx) = mpsc::channel(PROVISIONER_EVENT_CHANNEL_CAPACITY);
    let hooks = Hooks::new(VhostsNamespaceHooks::new(events_tx));
    let spec = spec(&service, hooks);
    store
        .register_namespace(&spec, Arc::new(JsonValuesMapping::new(namespace_name())))
        .map_err(|e| anyhow::anyhow!("register vhosts namespace in store: {e}"))?;
    let runtime = Arc::new(Runtime::new(router.service(), spec, store.clone()));
    router
        .register(runtime.clone())
        .map_err(|e| anyhow::anyhow!("register vhosts runtime on router: {e}"))?;
    Ok((runtime, events_rx))
}

/// Bridge an `IncomingCommand` whose verb has the `webd.props.`
/// prefix stripped to the `PropsRouter::dispatch` surface, then
/// project the `PropsResponse` back to the `(rc, body)` shape the
/// Bus loop needs.
///
/// Mirrors `cosmix-maild/src/bus/mod.rs::dispatch_props` — `PeerIdentity`
/// carries the registered sender as `service_name`; Unix peer creds
/// and WireGuard fields stay `None` (noded doesn't expose them today).
pub async fn dispatch_props(
    router: &PropsRouter,
    suffix: &str,
    cmd: &cosmix_client::IncomingCommand,
) -> (u8, String) {
    let mut msg = cosmix_bus::bus::BusMessage::new();
    for (k, v) in &cmd.headers {
        msg.set(k, v);
    }
    msg.body = cmd.body.clone();
    let peer = PeerIdentity {
        service_name: if cmd.from.is_empty() {
            None
        } else {
            Some(cmd.from.clone())
        },
        ..Default::default()
    };
    let resp = router.dispatch(suffix, &msg, &peer).await;
    let rc: u8 = u8::try_from(resp.rc.max(0)).unwrap_or(10);
    (rc, resp.body)
}

/// Construct a `VhostRow` from a substrate `PropValue::Object`. Used
/// by Rust consumers that prefer typed access; the wire surface
/// stays the canonical `PropValue` shape. Returns `Err` only when
/// serde rejects an unknown / mistyped field — the
/// `deny_unknown_fields` guard means a future schema bump that adds a
/// column without updating `VhostRow` errors loudly rather than
/// silently dropping the new column.
///
/// Called from `VhostsNamespaceHooks::after_set` to materialise the
/// committed row into [`NamespaceEvent::VhostSet`] for the provisioner
/// consumer; also useful for other Rust callers that want typed
/// access to a row read back from the namespace.
pub fn vhost_row_from_value(v: &PropValue) -> serde_json::Result<VhostRow> {
    let json = serde_json::to_value(v)?;
    serde_json::from_value(json)
}

/// Symmetric — render a `VhostRow` back to a substrate value.
#[allow(dead_code)]
pub fn vhost_row_to_value(row: &VhostRow) -> serde_json::Result<PropValue> {
    let json = serde_json::to_value(row)?;
    serde_json::from_value(json)
}

// `serde_json::Value` round-trip: we go through JSON because that's
// the wire shape the substrate canonicalises against. A direct
// `PropValue::Object(BTreeMap<...>)` from a serde derive would
// require a custom Serializer — gain not worth it for v0.

/// Snapshot every committed row in the `webd.vhosts` namespace as a
/// typed `Vec<VhostRow>`.
///
/// Used by C3e's [`crate::vhost_directory::from_namespace_rows`] to
/// build the initial host-routing directory from the
/// post-bootstrap substrate state. Reads via `Runtime::store().list(..)`
/// (SPEC 12 §6.4 `list` surface) so the snapshot is atomic against
/// concurrent writes — `bootstrap_upsert_from_config` has already
/// completed by the time this runs at startup.
///
/// **Fail-fast on deserialisation.** A row that fails
/// [`vhost_row_from_value`] is a substrate-invariant violation: the
/// `before_set` rule-6 row-shape gate runs on every committed write
/// (caller- *and* backend-origin), so a malformed row reaching storage
/// indicates either a substrate bug or an older daemon binary
/// persisted a row that the current `VhostRow` shape rejects (e.g.
/// `deny_unknown_fields` rejecting a removed column on downgrade). In
/// either case the operator needs to see the failure at boot — a
/// silent skip would yield a directory missing a vhost row the
/// operator can still see via `webd.props.list`, which is the worst
/// possible diagnostic for "why isn't site X serving."
pub async fn snapshot_rows(runtime: &Runtime) -> Result<Vec<VhostRow>> {
    let snap = runtime
        .store()
        .list(&namespace_name())
        .await
        .map_err(|e| anyhow::anyhow!("webd.vhosts list failed: {e}"))?;
    let mut rows: Vec<VhostRow> = Vec::with_capacity(snap.value.len());
    for record in snap.value {
        let row = vhost_row_from_value(&record.value).map_err(|e| {
            anyhow::anyhow!(
                "webd.vhosts row {key:?} failed VhostRow deserialisation: {e}. \
                 This is a substrate-invariant violation — before_set rule 6 \
                 rejects bad shapes on write, so a malformed committed row \
                 indicates a substrate bug or schema-downgrade. Refusing to \
                 build the directory from a partial snapshot.",
                key = record.key.key,
            )
        })?;
        // Defensive: `before_set` rule 2 enforces `row.fqdn ==
        // ctx.key.key` on every committed write (caller- *and*
        // backend-origin), so a committed row with a key/body
        // mismatch is a substrate-invariant violation in the same
        // class as bad serde shape. Re-check here so the directory
        // build trusts the per-row `fqdn` as canonical.
        if record.key.key.as_str() != row.fqdn {
            return Err(anyhow::anyhow!(
                "webd.vhosts row key/body mismatch — record.key.key={:?} but \
                 row.fqdn={:?}. before_set rule 2 rejects this on write, so \
                 a committed mismatch indicates a substrate bug. Refusing \
                 to build the directory.",
                record.key.key,
                row.fqdn,
            ));
        }
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_props::{Actor, PeerIdentity, RecordKey, SetOpts, Version, WriteOrigin};
    use rusqlite::Connection;

    fn open_test_store() -> Arc<SqliteStore> {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )
        .expect("pragmas");
        Arc::new(SqliteStore::new("webd", conn).expect("store"))
    }

    fn new_router() -> PropsRouter {
        PropsRouter::new("webd")
    }

    /// Build a `VhostsNamespaceHooks` instance for direct unit-level
    /// tests against `HookHandler` arms. C1d replaced the unit-struct
    /// `VhostsNamespaceHooks` with a struct carrying an mpsc sender,
    /// so direct hook tests need a channel; we hand back the receiver
    /// so tests that care about the emitted event can drain it.
    ///
    /// **Lifetime note.** The receiver IS the lifeline — dropping it
    /// closes the channel and `try_send` from the hook will then
    /// return `Closed`. Tests that don't care about the event must
    /// keep the receiver bound for the test's scope (`_events_rx`),
    /// not let it drop. The before_set arm is the safe exception:
    /// it never reaches `try_send`, so test sites that only exercise
    /// before_set are unaffected by binding lifetime; the after_set /
    /// after_delete sites in this module bind the receiver explicitly.
    fn hooks_with_channel() -> (VhostsNamespaceHooks, mpsc::Receiver<NamespaceEvent>) {
        let (tx, rx) = mpsc::channel(PROVISIONER_EVENT_CHANNEL_CAPACITY);
        (VhostsNamespaceHooks::new(tx), rx)
    }

    #[test]
    fn register_vhosts_namespace_clean() {
        let store = open_test_store();
        let mut router = new_router();
        let (runtime, _events_rx) = register_vhosts_namespace(&mut router, &store)
            .expect("clean register on a fresh store + router succeeds");
        // The runtime carries the same service name + namespace we
        // expect to reach over the Bus wire. A regression that
        // registers under `webd.routes` or `webd.props` would silently
        // re-bucket dispatch — the wire surface check below is the
        // canary.
        assert_eq!(runtime.spec().name.as_str(), NAMESPACE);
    }

    #[test]
    fn register_conflict_when_already_registered() {
        // Second `register` against the same router must fail —
        // SPEC 12 §15.6 register-once invariant. The substrate's
        // double-register error surfaces as the daemon refusing to
        // start, not a silent shadow.
        let store = open_test_store();
        let mut router = new_router();
        let (_runtime, _events_rx) =
            register_vhosts_namespace(&mut router, &store).expect("first register");
        // `Arc<Runtime>` doesn't implement `Debug`, so the typical
        // `expect_err` shortcut won't compile. Match on the result
        // explicitly — same intent, no Debug bound on the success arm.
        let err = match register_vhosts_namespace(&mut router, &store) {
            Ok(_) => panic!("second register must fail"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        // Either the store layer ("namespace already registered") or
        // the router layer ("runtime already registered") may surface
        // first depending on idempotence implementation. We assert
        // *something* identifying the conflict reaches the caller.
        assert!(
            msg.contains("already") || msg.contains("register") || msg.contains("vhosts"),
            "conflict message should identify the double-register, got: {msg}",
        );
    }

    #[test]
    fn auth_policy_caps_scope_to_webd_prefix() {
        let policy = auth_policy("webd");
        let caps = policy.resolve(&PeerIdentity::default());
        // Every cap must be scoped to `webd.vhosts` — a regression
        // that emits `webd.routes` or omits the service prefix would
        // silently widen authorisation. Verify by string-shape against
        // each declared cap.
        let want = [
            "props.read:webd.vhosts",
            "props.read:webd.vhosts:secrets",
            "props.describe:webd.vhosts:public",
            "props.describe:webd.vhosts:full",
            "props.audit:webd.vhosts",
            "props.write:webd.vhosts",
            // C5 — narrow cap for `webd.acme.renew`. Granted to
            // every WG peer in v0.1 alongside the other webd caps;
            // cross-mesh authz narrows later without renaming.
            "webd.acme.renew:webd.vhosts",
        ];
        for w in want {
            assert!(
                caps.contains(&Capability::from(w.to_string())),
                "missing cap {w}; got {caps:?}",
            );
        }
    }

    /// C1c re-introduces `props.write:webd.vhosts` together with the
    /// validating hook (the pair landed in a single commit so a
    /// bisect can never reach a node where the cap is open and the
    /// hook is empty — that was the C1b BLOCKER shape). The hook is
    /// load-bearing for write authority; this test asserts the cap
    /// is present and is the BLOCKER fix sibling to
    /// [`before_set_*`] hook tests below.
    #[test]
    fn auth_policy_grants_write_cap_with_validating_hook() {
        let policy = auth_policy("webd");
        let caps = policy.resolve(&PeerIdentity::default());
        let write_cap = Capability::from("props.write:webd.vhosts".to_string());
        assert!(
            caps.contains(&write_cap),
            "C1c grants the write cap — validating hook defends it. Got: {caps:?}",
        );
    }

    fn full_row() -> VhostRow {
        VhostRow {
            fqdn: "example.com".into(),
            enabled: true,
            www_dir: "/srv/www/example".into(),
            aliases: Vec::new(),
            source: Some("config_bootstrap".into()),
            acme_provider: Some("letsencrypt_prod".into()),
            acme_challenge: Some("http01".into()),
            acme_contact_email: Some("ops@example.com".into()),
            tls_cert_path: None,
            tls_key_path: None,
            cert_blob_id: Some("acme/example.com/live/fullchain.pem".into()),
            key_blob_id: Some("acme/example.com/live/privkey.pem".into()),
            not_after: Some("2026-08-01T00:00:00Z".into()),
            last_attempt: Some("2026-05-25T01:00:00Z".into()),
            last_error_count: Some(0),
            last_error: None,
        }
    }

    #[test]
    fn vhost_row_serde_round_trips() {
        let row = full_row();
        let json = serde_json::to_string(&row).expect("serialise");
        let back: VhostRow = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(row, back);

        // Round-trip through PropValue too — the substrate wire form
        // — so `vhost_row_from_value` and `vhost_row_to_value` are
        // load-bearing for Rust consumers.
        let pv = vhost_row_to_value(&row).expect("to PropValue");
        let back: VhostRow = vhost_row_from_value(&pv).expect("from PropValue");
        assert_eq!(row, back);
    }

    #[test]
    fn vhost_row_deny_unknown_fields_rejects_legacy_extra_key() {
        // A row from the v0 mirror schema carrying the old
        // `has_cms` field must NOT deserialise into a Phase 3
        // `VhostRow` — otherwise an older daemon's row would
        // silently shed its presence-of fields on the new daemon
        // without surfacing a schema-version mismatch.
        let legacy = serde_json::json!({
            "fqdn": "example.com",
            "enabled": true,
            "www_dir": "/srv/www/example",
            "has_cms": true,
        });
        let err = serde_json::from_value::<VhostRow>(legacy)
            .expect_err("unknown field has_cms must be rejected by deny_unknown_fields");
        assert!(
            err.to_string().contains("has_cms") || err.to_string().contains("unknown"),
            "error should identify the offending field, got: {err}",
        );
    }

    /// The substrate redacts fields annotated `secret: true` when the
    /// caller's capability set lacks `:secrets`. We exercise this at
    /// the schema level — a smoke test that the secret flag survived
    /// the schema builder. The full caller-visible redaction
    /// behaviour is tested in `cosmix-lib-props-store` itself; here we
    /// only pin that webd's schema annotated the three secret
    /// columns correctly.
    #[test]
    fn secrets_capability_redacts_cert_blob_id_without_secrets() {
        let s = schema();
        let by_name: BTreeMap<&str, &FieldSchema> =
            s.fields.iter().map(|f| (f.name.as_str(), f)).collect();
        for name in ["cert_blob_id", "key_blob_id", "last_error"] {
            let f = by_name
                .get(name)
                .unwrap_or_else(|| panic!("schema must declare {name}"));
            assert!(f.secret, "{name} must be secret:true in the schema");
        }
        for name in [
            "fqdn",
            "enabled",
            "www_dir",
            "aliases",
            "source",
            "acme_provider",
            "acme_challenge",
            "acme_contact_email",
            "tls_cert_path",
            "tls_key_path",
            "not_after",
            "last_attempt",
            "last_error_count",
        ] {
            let f = by_name
                .get(name)
                .unwrap_or_else(|| panic!("schema must declare {name}"));
            assert!(
                !f.secret,
                "{name} must be non-secret in the schema (Phase 3 capability split)",
            );
        }
    }

    /// End-to-end: register the vhosts namespace, install an
    /// in-process `DispatchPublisher`, spawn the per-runtime
    /// dispatcher, drive a backend-origin `set`, and assert the
    /// `records.changed` event reaches the publisher. This pins the
    /// gauntlet step that motivated the granter + publisher wiring —
    /// without it the watch verb silently no-ops.
    ///
    /// The in-process publisher is a simple channel-backed sink; the
    /// production `NodedPropsPublisher` (in `bus/props_publisher.rs`)
    /// has its own wire-shape test there.
    #[tokio::test]
    async fn props_watch_dispatcher_delivers_records_changed() {
        use cosmix_props::DispatchPublisher;
        use std::future::Future;
        use std::pin::Pin;
        use std::sync::Mutex;

        struct ChannelPublisher {
            tx: tokio::sync::mpsc::UnboundedSender<(String, String)>,
        }
        impl DispatchPublisher for ChannelPublisher {
            fn publish<'a>(
                &'a self,
                topic: &'a str,
                body: String,
            ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
                let tx = self.tx.clone();
                let topic = topic.to_string();
                Box::pin(async move {
                    let _ = tx.send((topic, body));
                    Ok(())
                })
            }
        }

        let store = open_test_store();
        let mut router = new_router();
        let (runtime, _events_rx) =
            register_vhosts_namespace(&mut router, &store).expect("register");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
        let publisher: Arc<dyn DispatchPublisher> = Arc::new(ChannelPublisher { tx });
        let _handle = cosmix_props::spawn_dispatcher(Arc::clone(&runtime), Arc::clone(&publisher))
            .await
            .expect("dispatcher spawn");

        // Use a complete-shape body — C1c's validating hook
        // rejects "enabled vhost with no TLS mode" even on the
        // backend-origin path (the mutual-exclusion rule is
        // origin-independent; only the daemon-owned-column check
        // gates on origin). A minimal ACME bundle is the
        // shortest path to a valid row.
        let key = RecordKey::collection(namespace_name(), "example.com");
        let mut obj = BTreeMap::new();
        obj.insert("fqdn".into(), PropValue::String("example.com".into()));
        obj.insert("enabled".into(), PropValue::Bool(true));
        obj.insert(
            "www_dir".into(),
            PropValue::String("/srv/www/example".into()),
        );
        obj.insert(
            "acme_provider".into(),
            PropValue::String("letsencrypt_prod".into()),
        );
        obj.insert("acme_challenge".into(), PropValue::String("http01".into()));
        obj.insert(
            "acme_contact_email".into(),
            PropValue::String("ops@example.com".into()),
        );
        runtime
            .set_with_origin(
                key,
                PropValue::Object(obj),
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::service("webd"),
                    cause: Some("test".into()),
                    ts_ms: 0,
                },
                WriteOrigin::backend(),
            )
            .await
            .expect("set");

        // Drain one event with a short deadline — the dispatcher
        // publishes on every commit. A timeout here means the
        // dispatcher never woke up, which is exactly the silent
        // no-op the maild-pattern wiring exists to prevent.
        let recv = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("dispatcher must publish within 2s");
        let (topic, body) = recv.expect("channel must not close");
        assert!(
            topic.contains("records.changed"),
            "expected records.changed topic, got {topic}",
        );
        assert!(
            body.contains("example.com"),
            "expected body to mention the row key, got {body}",
        );

        // Silence the captured Mutex import in case the closure
        // future doesn't need it after layout shifts.
        let _ = std::any::TypeId::of::<Mutex<()>>();
    }

    // -- C1c validating hook tests --
    //
    // Direct unit tests against `VhostsNamespaceHooks::before_set`.
    // They build a `HookCtx` by hand rather than driving
    // `runtime.set(..)`, which is intentional: the substrate's
    // schema-level enum check (lib-props' validators wiring) is
    // currently a placeholder shape, so the hook's `dns01` rejection
    // is the real enforcement point and would otherwise be hidden
    // by a future schema-time rejection in an end-to-end test.
    // Unit-level coverage pins the hook behaviour against that
    // upgrade path.

    fn hook_ctx_caller_replace(fqdn: &str, body: BTreeMap<String, PropValue>) -> HookCtx {
        HookCtx {
            key: RecordKey::collection(namespace_name(), fqdn.to_string()),
            old: None,
            new: Some(PropValue::Object(body)),
            version: Version::zero(),
            actor: Actor::operator("test-operator"),
            merge: Some(MergeMode::Replace),
            origin: WriteOrigin::caller(),
        }
    }

    fn hook_ctx_backend_replace(fqdn: &str, body: BTreeMap<String, PropValue>) -> HookCtx {
        let mut ctx = hook_ctx_caller_replace(fqdn, body);
        ctx.origin = WriteOrigin::backend();
        ctx
    }

    fn minimal_acme_body(fqdn: &str) -> BTreeMap<String, PropValue> {
        let mut b = BTreeMap::new();
        b.insert("fqdn".into(), PropValue::String(fqdn.into()));
        b.insert("enabled".into(), PropValue::Bool(true));
        b.insert("www_dir".into(), PropValue::String("/srv/www/x".into()));
        b.insert(
            "acme_provider".into(),
            PropValue::String("letsencrypt_prod".into()),
        );
        b.insert("acme_challenge".into(), PropValue::String("http01".into()));
        b.insert(
            "acme_contact_email".into(),
            PropValue::String("ops@example.com".into()),
        );
        b
    }

    fn minimal_manual_tls_body(fqdn: &str) -> BTreeMap<String, PropValue> {
        let mut b = BTreeMap::new();
        b.insert("fqdn".into(), PropValue::String(fqdn.into()));
        b.insert("enabled".into(), PropValue::Bool(true));
        b.insert("www_dir".into(), PropValue::String("/srv/www/x".into()));
        b.insert(
            "tls_cert_path".into(),
            PropValue::String("/etc/cosmix/webd/certs/x.crt".into()),
        );
        b.insert(
            "tls_key_path".into(),
            PropValue::String("/etc/cosmix/webd/keys/x.key".into()),
        );
        b
    }

    /// Happy paths: the two complete configurations the substrate
    /// supports today (ACME and manual) both pass `before_set`. This
    /// is the canary that future rule additions don't accidentally
    /// reject the valid surface.
    #[tokio::test]
    async fn before_set_accepts_complete_acme_body() {
        let (h, _events_rx) = hooks_with_channel();
        let ctx = hook_ctx_caller_replace("example.com", minimal_acme_body("example.com"));
        h.before_set(&ctx)
            .await
            .expect("complete ACME row accepted");
    }

    #[tokio::test]
    async fn before_set_accepts_complete_manual_tls_body() {
        let (h, _events_rx) = hooks_with_channel();
        let ctx = hook_ctx_caller_replace("example.com", minimal_manual_tls_body("example.com"));
        h.before_set(&ctx)
            .await
            .expect("complete manual TLS row accepted");
    }

    /// Rule 1 — one parametric test per daemon-owned field. Caller
    /// origin + the field present in the body must reject with the
    /// `operator_wrote_daemon_field` prefix the CLI parses against.
    #[tokio::test]
    async fn before_set_rejects_caller_writes_to_daemon_owned_fields() {
        let (h, _events_rx) = hooks_with_channel();
        // (field name, sentinel value) — sentinels chosen for shape
        // typing; the hook short-circuits on key presence so the
        // value itself is incidental.
        let cases: &[(&str, PropValue)] = &[
            ("source", PropValue::String("config_bootstrap".into())),
            (
                "cert_blob_id",
                PropValue::String("acme/x/live/fullchain.pem".into()),
            ),
            (
                "key_blob_id",
                PropValue::String("acme/x/live/privkey.pem".into()),
            ),
            (
                "not_after",
                PropValue::String("2026-08-01T00:00:00Z".into()),
            ),
            (
                "last_attempt",
                PropValue::String("2026-05-26T00:00:00Z".into()),
            ),
            ("last_error_count", PropValue::Int(3)),
            ("last_error", PropValue::String("oops".into())),
        ];
        for (field, sentinel) in cases {
            let mut body = minimal_acme_body("example.com");
            body.insert((*field).into(), sentinel.clone());
            let ctx = hook_ctx_caller_replace("example.com", body);
            let err = h
                .before_set(&ctx)
                .await
                .expect_err(&format!("caller write to {field} must reject"));
            let msg = err.message();
            assert!(
                msg.starts_with(OPERATOR_WROTE_DAEMON_FIELD_PREFIX),
                "expected {OPERATOR_WROTE_DAEMON_FIELD_PREFIX} prefix in {msg}",
            );
            assert!(
                msg.contains(*field),
                "message must name the field {field}: {msg}"
            );
        }
    }

    /// Rule 1 complement — the backend-origin path bypasses the
    /// daemon-owned-column check. The ACME provisioner writeback
    /// and C3's bootstrap upsert depend on this. Without it the
    /// daemon could not stamp the columns it owns.
    #[tokio::test]
    async fn before_set_backend_origin_writes_daemon_fields_through() {
        let (h, _events_rx) = hooks_with_channel();
        let mut body = minimal_acme_body("example.com");
        body.insert("source".into(), PropValue::String("bus_runtime".into()));
        body.insert(
            "cert_blob_id".into(),
            PropValue::String("acme/example.com/live/fullchain.pem".into()),
        );
        body.insert("last_error_count".into(), PropValue::Int(0));
        let ctx = hook_ctx_backend_replace("example.com", body);
        h.before_set(&ctx).await.expect(
            "backend origin must bypass the daemon-owned-column check; this is the route \
             bootstrap and the provisioner writeback use",
        );
    }

    /// Rule 2 — fqdn body field must match the wire key.
    #[tokio::test]
    async fn before_set_rejects_fqdn_mismatch_with_key() {
        let (h, _events_rx) = hooks_with_channel();
        let mut body = minimal_acme_body("b.example.com");
        body.insert("fqdn".into(), PropValue::String("b.example.com".into()));
        let ctx = hook_ctx_caller_replace("a.example.com", body);
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("fqdn/key mismatch must reject");
        let msg = err.message();
        assert!(
            msg.contains("a.example.com") && msg.contains("b.example.com"),
            "rejection must name both sides of the mismatch: {msg}",
        );
    }

    /// Rule 3 — aliases reserved.
    #[tokio::test]
    async fn before_set_rejects_non_empty_aliases() {
        let (h, _events_rx) = hooks_with_channel();
        let mut body = minimal_acme_body("example.com");
        body.insert(
            "aliases".into(),
            PropValue::List(vec![PropValue::String("www.example.com".into())]),
        );
        let ctx = hook_ctx_caller_replace("example.com", body);
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("non-empty aliases must reject");
        assert!(
            err.message().contains("multi-SAN"),
            "rejection must point at the multi-SAN deferred path: {}",
            err.message(),
        );
    }

    /// Rule 3 — empty aliases is fine (the schema default).
    #[tokio::test]
    async fn before_set_accepts_empty_aliases_list() {
        let (h, _events_rx) = hooks_with_channel();
        let mut body = minimal_acme_body("example.com");
        body.insert("aliases".into(), PropValue::List(Vec::new()));
        let ctx = hook_ctx_caller_replace("example.com", body);
        h.before_set(&ctx).await.expect("empty aliases must pass");
    }

    /// Rule 4 — dns01 reserved for Phase 4.
    #[tokio::test]
    async fn before_set_rejects_dns01_acme_challenge() {
        let (h, _events_rx) = hooks_with_channel();
        let mut body = minimal_acme_body("example.com");
        body.insert("acme_challenge".into(), PropValue::String("dns01".into()));
        let ctx = hook_ctx_caller_replace("example.com", body);
        let err = h.before_set(&ctx).await.expect_err("dns01 must reject");
        assert!(
            err.message().contains("dns01") && err.message().contains("Phase 4"),
            "rejection must point at Phase 4: {}",
            err.message(),
        );
    }

    /// Rule 5a — both ACME and manual TLS configured: ambiguous, reject.
    #[tokio::test]
    async fn before_set_rejects_both_acme_and_manual_tls() {
        let (h, _events_rx) = hooks_with_channel();
        let mut body = minimal_acme_body("example.com");
        body.insert(
            "tls_cert_path".into(),
            PropValue::String("/etc/cosmix/webd/certs/x.crt".into()),
        );
        body.insert(
            "tls_key_path".into(),
            PropValue::String("/etc/cosmix/webd/keys/x.key".into()),
        );
        let ctx = hook_ctx_caller_replace("example.com", body);
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("acme + manual TLS together must reject");
        assert!(
            err.message().contains("mutually exclusive"),
            "rejection must say 'mutually exclusive': {}",
            err.message(),
        );
    }

    /// Rule 5b — enabled vhost with neither mode rejected.
    #[tokio::test]
    async fn before_set_rejects_enabled_vhost_with_no_tls_mode() {
        let (h, _events_rx) = hooks_with_channel();
        let mut body = BTreeMap::new();
        body.insert("fqdn".into(), PropValue::String("example.com".into()));
        body.insert("enabled".into(), PropValue::Bool(true));
        body.insert("www_dir".into(), PropValue::String("/srv/www/x".into()));
        let ctx = hook_ctx_caller_replace("example.com", body);
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("enabled + no TLS mode must reject");
        assert!(
            err.message().contains("acme_provider") && err.message().contains("tls_cert_path"),
            "rejection must list both modes: {}",
            err.message(),
        );
    }

    /// Rule 5b complement — `enabled=false` rows stage cleanly with
    /// neither mode (operator authoring path).
    #[tokio::test]
    async fn before_set_accepts_disabled_vhost_with_no_tls_mode() {
        let (h, _events_rx) = hooks_with_channel();
        let mut body = BTreeMap::new();
        body.insert("fqdn".into(), PropValue::String("example.com".into()));
        body.insert("enabled".into(), PropValue::Bool(false));
        body.insert("www_dir".into(), PropValue::String("/srv/www/x".into()));
        let ctx = hook_ctx_caller_replace("example.com", body);
        h.before_set(&ctx)
            .await
            .expect("disabled rows may stage with no TLS mode");
    }

    /// Rule 6 — row-shape gate. A disabled row missing the required
    /// `www_dir` field passes every cross-field rule above (rule 5b
    /// explicitly allows disabled + no TLS mode) yet fails the
    /// `VhostRow` deserialise. Without rule 6 it would commit, then
    /// `after_set` would fail `vhost_row_from_value` and silently
    /// drop the producer event — the orphaned-row bug Codex flagged
    /// against C1d rev-1. This test pins the gate.
    #[tokio::test]
    async fn before_set_rejects_disabled_row_missing_required_www_dir() {
        let (h, _events_rx) = hooks_with_channel();
        let mut body = BTreeMap::new();
        body.insert("fqdn".into(), PropValue::String("example.com".into()));
        body.insert("enabled".into(), PropValue::Bool(false));
        // No www_dir — would pass all cross-field rules but fail
        // VhostRow serde.
        let ctx = hook_ctx_caller_replace("example.com", body);
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("row shape gate must reject missing www_dir");
        match err {
            HookError::Validation { message } => {
                assert!(
                    message.contains("row shape rejected") || message.contains("www_dir"),
                    "shape-gate error must mention the rejection cause, got: {message}",
                );
            }
            other => panic!("expected HookError::Validation, got {other:?}"),
        }
    }

    /// Rule 6 — `deny_unknown_fields` gate. The substrate's field
    /// schema currently has empty validators; without the row-shape
    /// gate, an operator typo on a column name (e.g.
    /// `acme_provdier`) would land in storage and silently fail
    /// `after_set` deserialisation. This pins the typo-rejection
    /// half of the gate.
    #[tokio::test]
    async fn before_set_rejects_unknown_field_typo() {
        let (h, _events_rx) = hooks_with_channel();
        let mut body = minimal_acme_body("example.com");
        // Intentional typo. `VhostRow` has `#[serde(deny_unknown_fields)]`.
        body.insert(
            "acme_provdier".into(),
            PropValue::String("letsencrypt_prod".into()),
        );
        let ctx = hook_ctx_caller_replace("example.com", body);
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("row shape gate must reject unknown fields");
        match err {
            HookError::Validation { message } => {
                assert!(
                    message.contains("row shape rejected")
                        || message.contains("acme_provdier")
                        || message.contains("unknown field"),
                    "deny_unknown_fields error must surface the typo, got: {message}",
                );
            }
            other => panic!("expected HookError::Validation, got {other:?}"),
        }
    }

    /// Rule 5c — ACME mode missing `acme_challenge` is rejected.
    #[tokio::test]
    async fn before_set_rejects_acme_without_challenge() {
        let (h, _events_rx) = hooks_with_channel();
        let mut body = minimal_acme_body("example.com");
        body.remove("acme_challenge");
        let ctx = hook_ctx_caller_replace("example.com", body);
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("acme_provider without acme_challenge must reject");
        assert!(
            err.message().contains("acme_challenge"),
            "rejection must name acme_challenge: {}",
            err.message(),
        );
    }

    /// Rule 5c — ACME mode with empty contact email is rejected.
    #[tokio::test]
    async fn before_set_rejects_acme_with_empty_contact_email() {
        let (h, _events_rx) = hooks_with_channel();
        let mut body = minimal_acme_body("example.com");
        body.insert(
            "acme_contact_email".into(),
            PropValue::String(String::new()),
        );
        let ctx = hook_ctx_caller_replace("example.com", body);
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("empty acme_contact_email must reject");
        assert!(
            err.message().contains("acme_contact_email"),
            "rejection must name acme_contact_email: {}",
            err.message(),
        );
    }

    /// Rule 5d — manual TLS pair half-present rejected.
    #[tokio::test]
    async fn before_set_rejects_tls_cert_without_key_path() {
        let (h, _events_rx) = hooks_with_channel();
        let mut body = minimal_manual_tls_body("example.com");
        body.remove("tls_key_path");
        let ctx = hook_ctx_caller_replace("example.com", body);
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("tls_cert_path without tls_key_path must reject");
        assert!(
            err.message().contains("tls_key_path"),
            "rejection must name tls_key_path: {}",
            err.message(),
        );
    }

    /// Cross-field rules evaluate the *merged* effective row, not
    /// just the patch body. A Patch that adds `tls_cert_path` to a
    /// row already carrying `acme_provider` must still trip the
    /// mutual-exclusion rule, even though the patch body itself
    /// only mentions the manual half.
    #[tokio::test]
    async fn before_set_patch_against_existing_acme_row_rejects_added_tls_cert_path() {
        let (h, _events_rx) = hooks_with_channel();
        // Prior row: ACME-configured.
        let mut prior = BTreeMap::new();
        prior.insert("fqdn".into(), PropValue::String("example.com".into()));
        prior.insert("enabled".into(), PropValue::Bool(true));
        prior.insert("www_dir".into(), PropValue::String("/srv/www/x".into()));
        prior.insert(
            "acme_provider".into(),
            PropValue::String("letsencrypt_prod".into()),
        );
        prior.insert("acme_challenge".into(), PropValue::String("http01".into()));
        prior.insert(
            "acme_contact_email".into(),
            PropValue::String("ops@example.com".into()),
        );
        // Patch: add the manual half.
        let mut patch = BTreeMap::new();
        patch.insert(
            "tls_cert_path".into(),
            PropValue::String("/etc/cosmix/webd/certs/x.crt".into()),
        );
        patch.insert(
            "tls_key_path".into(),
            PropValue::String("/etc/cosmix/webd/keys/x.key".into()),
        );
        let ctx = HookCtx {
            key: RecordKey::collection(namespace_name(), "example.com".to_string()),
            old: Some(PropValue::Object(prior)),
            new: Some(PropValue::Object(patch)),
            version: Version::zero(),
            actor: Actor::operator("test-operator"),
            merge: Some(MergeMode::Patch),
            origin: WriteOrigin::caller(),
        };
        let err = h
            .before_set(&ctx)
            .await
            .expect_err("patch that creates mutual-exclusion violation must reject");
        assert!(
            err.message().contains("mutually exclusive"),
            "rejection must call out mutual exclusion against the merged row: {}",
            err.message(),
        );
    }

    /// Patch round-trip: a benign patch to a complete prior row
    /// (e.g. flip `enabled=false`) must pass. Pins that the merged
    /// view doesn't accidentally short-circuit on the patch's
    /// missing TLS-mode fields.
    #[tokio::test]
    async fn before_set_patch_against_complete_row_accepts_benign_flip() {
        let (h, _events_rx) = hooks_with_channel();
        let prior_obj = minimal_acme_body("example.com");
        let mut patch = BTreeMap::new();
        patch.insert("enabled".into(), PropValue::Bool(false));
        let ctx = HookCtx {
            key: RecordKey::collection(namespace_name(), "example.com".to_string()),
            old: Some(PropValue::Object(prior_obj)),
            new: Some(PropValue::Object(patch)),
            version: Version::zero(),
            actor: Actor::operator("test-operator"),
            merge: Some(MergeMode::Patch),
            origin: WriteOrigin::caller(),
        };
        h.before_set(&ctx).await.expect("benign patch must pass");
    }

    /// Concurrent-patch race guard: a caller `set` without
    /// `if_version` is rejected up front. This is the substrate's
    /// optimistic-concurrency surface — without it, two concurrent
    /// patches that each pass `before_set` against the same prior
    /// snapshot can combine into a row that violates mutual
    /// exclusion (patch A adds acme_*, patch B adds tls_cert_path,
    /// both see a disabled no-mode row, both commit, second wins
    /// with both modes set). `require_version = true` collapses the
    /// race; this test pins the wiring.
    #[tokio::test]
    async fn caller_set_without_if_version_rejected() {
        use cosmix_props::RuntimeError;
        let store = open_test_store();
        let mut router = new_router();
        let (runtime, _events_rx) =
            register_vhosts_namespace(&mut router, &store).expect("register");

        let body = minimal_acme_body("noversion.example.com");
        let key = RecordKey::collection(namespace_name(), "noversion.example.com");
        let err = runtime
            .set(
                key,
                PropValue::Object(body),
                SetOpts {
                    // No `expected_version` — the substrate must
                    // refuse before any hook or storage IO.
                    expected_version: None,
                    merge: MergeMode::Replace,
                    actor: Actor::operator("test-operator"),
                    cause: Some("test".into()),
                    ts_ms: 0,
                },
            )
            .await
            .expect_err("require_version must reject sets without if_version");
        // The substrate surfaces the missing-version case as
        // `RuntimeError::Hook(HookError::Validation { ... })` with
        // a message that mentions `if_version`. Keep the assertion
        // shape-only (don't pin the exact text) so a lib-props
        // copy-edit doesn't break this regression.
        match err {
            RuntimeError::Hook(HookError::Validation { ref message }) => {
                assert!(
                    message.contains("if_version") || message.contains("version"),
                    "rejection must mention if_version/version, got: {message}",
                );
            }
            other => panic!("expected Hook(Validation), got {other:?}"),
        }
    }

    /// End-to-end via `Runtime`: a caller-origin set that violates
    /// the hook fails BEFORE the storage commit fires (no row
    /// lands, no event publishes). This is the "validation
    /// rejection round-trips and leaves the namespace untouched"
    /// bucket from the rev-6 plan's V-H test list.
    #[tokio::test]
    async fn runtime_set_rejection_leaves_namespace_untouched() {
        use cosmix_props::RuntimeError;
        let store = open_test_store();
        let mut router = new_router();
        let (runtime, _events_rx) =
            register_vhosts_namespace(&mut router, &store).expect("register");

        // Caller-origin write that names a daemon-owned column —
        // the hook must reject before commit.
        let mut body = minimal_acme_body("rejected.example.com");
        body.insert("source".into(), PropValue::String("bus_runtime".into()));
        let key = RecordKey::collection(namespace_name(), "rejected.example.com");
        let err = runtime
            .set(
                key.clone(),
                PropValue::Object(body),
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::operator("test-operator"),
                    cause: Some("test".into()),
                    ts_ms: 0,
                },
            )
            .await
            .expect_err("caller-origin write to daemon column must reject");
        assert!(
            matches!(err, RuntimeError::Hook(HookError::Validation { .. })),
            "expected RuntimeError::Hook(Validation), got {err:?}",
        );

        // The namespace must NOT carry the rejected row.
        match runtime.store().get(&key).await {
            Err(cosmix_props::StoreError::NotFound) => {}
            Ok(snap) => panic!(
                "rejected row leaked into the namespace: version={:?}",
                snap.value.version
            ),
            Err(e) => panic!("unexpected error reading the rejected key: {e}"),
        }
    }

    // -- C1d producer-side mpsc plumbing tests --
    //
    // These pin the wiring from the lifecycle hooks into the bounded
    // `NamespaceEvent` channel. The consumer half (provisioner
    // `run_forever` select! arms) lands in C4a/C4b — until then the
    // receiver is parked in `main.rs` for the daemon's lifetime.
    //
    // The bisect contract from the module docstring: a build at any
    // intermediate commit must compile + pass tests but the system as
    // a whole is not deployable until C4. These tests verify the
    // producer half is correct in isolation.

    /// Direct unit: a successful `after_set` enqueues `VhostSet`
    /// carrying the freshly merged row, on the per-namespace channel.
    /// `ctx.new` is the post-Patch merged value (lib-props
    /// `runtime.rs` line 313 / `sqlite.rs` line ~907) — the hook
    /// deserialises it back through `vhost_row_from_value` so the
    /// downstream consumer sees a typed `VhostRow`, not raw
    /// `PropValue`.
    #[tokio::test]
    async fn after_set_enqueues_vhost_set_event() {
        let (h, mut rx) = hooks_with_channel();
        let body = minimal_acme_body("a.example.com");
        let ctx = hook_ctx_caller_replace("a.example.com", body);
        h.after_set(&ctx).await.expect("after_set never errors");
        let ev = rx
            .recv()
            .await
            .expect("after_set must enqueue exactly one event");
        match ev {
            NamespaceEvent::VhostSet { fqdn, row } => {
                assert_eq!(fqdn, "a.example.com");
                assert_eq!(row.fqdn, "a.example.com");
                assert!(row.enabled);
            }
            NamespaceEvent::VhostRemoved { fqdn } => {
                panic!("expected VhostSet, got VhostRemoved {{ fqdn: {fqdn} }}")
            }
        }
    }

    /// Direct unit: when the bounded channel is full, `try_send`
    /// returns `Full`; the hook MUST log + swallow, never propagate
    /// or block. The contract is that `after_set` always returns
    /// `Ok(())` — the C4 drop-then-reconcile recovery contract (once
    /// consumer arms land) is the recovery mechanism, not
    /// back-pressure on the writer.
    #[tokio::test]
    async fn after_set_on_full_channel_drops_event() {
        // Build a 1-slot channel so we can fill it deterministically.
        // We can't use `PROVISIONER_EVENT_CHANNEL_CAPACITY` here
        // because 128 sets per test would be flaky against the
        // mpsc scheduler; the rule under test is "Full → drop, not
        // block", independent of capacity value.
        let (tx, mut rx) = mpsc::channel(1);
        let h = VhostsNamespaceHooks::new(tx);
        let body = minimal_acme_body("a.example.com");
        let ctx_a = hook_ctx_caller_replace("a.example.com", body.clone());
        let ctx_b = hook_ctx_caller_replace("b.example.com", body);
        // First enqueue fills the slot.
        h.after_set(&ctx_a)
            .await
            .expect("first enqueue succeeds (channel had a slot)");
        // Second enqueue MUST NOT block; MUST NOT propagate; the
        // event is dropped, log entry is the operator's signal.
        h.after_set(&ctx_b)
            .await
            .expect("second enqueue swallows Full and returns Ok");
        // Drain proves the first event is the surviving one — i.e.
        // the second was dropped, not the first.
        let ev = rx.recv().await.expect("first event survived");
        match ev {
            NamespaceEvent::VhostSet { fqdn, .. } => assert_eq!(fqdn, "a.example.com"),
            NamespaceEvent::VhostRemoved { fqdn } => {
                panic!("expected VhostSet, got VhostRemoved {{ fqdn: {fqdn} }}")
            }
        }
    }

    /// Direct unit: `before_delete` is a reversible-preflight no-op
    /// in C1d. Destructive cleanup (cert blob removal, ACME order
    /// teardown) lives in `after_delete` — putting it in
    /// `before_delete` would leave the system in an unrecoverable
    /// state if the storage commit subsequently failed (rev-1
    /// BLOCKER finding from cold review).
    #[tokio::test]
    async fn before_delete_is_noop() {
        let (h, mut rx) = hooks_with_channel();
        let ctx = HookCtx {
            key: RecordKey::collection(namespace_name(), "a.example.com"),
            old: Some(PropValue::Object(minimal_acme_body("a.example.com"))),
            new: None,
            version: Version::zero(),
            actor: Actor::operator("test-operator"),
            merge: None,
            origin: WriteOrigin::caller(),
        };
        h.before_delete(&ctx)
            .await
            .expect("before_delete is a no-op");
        // No event must have been queued by the preflight.
        assert!(
            rx.try_recv().is_err(),
            "before_delete must not enqueue any NamespaceEvent",
        );
    }

    /// Direct unit: `after_delete` enqueues `VhostRemoved` carrying
    /// the key's leaf as the fqdn. `after_delete` is propagating in
    /// lib-props (`runtime.rs` line 447), so the hook MUST always
    /// return Ok — anything else would unwind the delete and leave
    /// the substrate row resurrected.
    #[tokio::test]
    async fn after_delete_enqueues_vhost_removed_event() {
        let (h, mut rx) = hooks_with_channel();
        let ctx = HookCtx {
            key: RecordKey::collection(namespace_name(), "a.example.com"),
            old: Some(PropValue::Object(minimal_acme_body("a.example.com"))),
            new: None,
            version: Version::zero(),
            actor: Actor::operator("test-operator"),
            merge: None,
            origin: WriteOrigin::caller(),
        };
        h.after_delete(&ctx)
            .await
            .expect("after_delete never errors");
        let ev = rx
            .recv()
            .await
            .expect("after_delete must enqueue VhostRemoved");
        match ev {
            NamespaceEvent::VhostRemoved { fqdn } => assert_eq!(fqdn, "a.example.com"),
            NamespaceEvent::VhostSet { fqdn, .. } => {
                panic!("expected VhostRemoved, got VhostSet {{ fqdn: {fqdn} }}")
            }
        }
    }

    /// End-to-end: a successful `runtime.set(..)` through the full
    /// substrate pipeline (validation, storage commit, lifecycle
    /// fan-out) results in a `VhostSet` event reaching the receiver.
    /// This is the load-bearing test — if the namespace registers
    /// hooks under the wrong runtime or the dispatcher skips the
    /// after_set callback, the unit tests above can still pass while
    /// the running daemon silently emits no events.
    #[tokio::test]
    async fn runtime_set_propagates_vhost_set_event() {
        let store = open_test_store();
        let mut router = new_router();
        let (runtime, mut events_rx) =
            register_vhosts_namespace(&mut router, &store).expect("register");

        let body = minimal_acme_body("e2e-set.example.com");
        let key = RecordKey::collection(namespace_name(), "e2e-set.example.com");
        runtime
            .set(
                key,
                PropValue::Object(body),
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::operator("test-operator"),
                    cause: Some("test".into()),
                    ts_ms: 0,
                },
            )
            .await
            .expect("set commits");

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv())
            .await
            .expect("event must arrive within 2s")
            .expect("channel must not close");
        match ev {
            NamespaceEvent::VhostSet { fqdn, row } => {
                assert_eq!(fqdn, "e2e-set.example.com");
                assert_eq!(row.fqdn, "e2e-set.example.com");
            }
            NamespaceEvent::VhostRemoved { fqdn } => {
                panic!("expected VhostSet, got VhostRemoved {{ fqdn: {fqdn} }}")
            }
        }
    }

    /// End-to-end: a successful `runtime.delete(..)` propagates a
    /// `VhostRemoved` event. Symmetric counterpart to the set test
    /// above; the same wiring-canary rationale applies.
    #[tokio::test]
    async fn runtime_delete_propagates_vhost_removed_event() {
        use cosmix_props::DeleteOpts;
        let store = open_test_store();
        let mut router = new_router();
        let (runtime, mut events_rx) =
            register_vhosts_namespace(&mut router, &store).expect("register");

        // Seed a row to delete. Origin=caller; `expected_version =
        // Some(Version::zero())` because of `require_version`.
        let body = minimal_acme_body("e2e-del.example.com");
        let key = RecordKey::collection(namespace_name(), "e2e-del.example.com");
        runtime
            .set(
                key.clone(),
                PropValue::Object(body),
                SetOpts {
                    expected_version: Some(Version::zero()),
                    merge: MergeMode::Replace,
                    actor: Actor::operator("test-operator"),
                    cause: Some("seed".into()),
                    ts_ms: 0,
                },
            )
            .await
            .expect("seed set");

        // Drain the set event so we can pin the delete event below
        // without ordering ambiguity.
        let _set_ev = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv())
            .await
            .expect("set event must arrive")
            .expect("channel open");

        // The seed commit advanced the version from zero — read it
        // back so the delete can use the current version.
        let snap = runtime.store().get(&key).await.expect("seed row exists");
        let cur_version = snap.value.version;

        runtime
            .delete(
                key,
                DeleteOpts {
                    expected_version: Some(cur_version),
                    actor: Actor::operator("test-operator"),
                    cause: Some("test".into()),
                    ts_ms: 0,
                },
            )
            .await
            .expect("delete commits");

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), events_rx.recv())
            .await
            .expect("delete event must arrive within 2s")
            .expect("channel must not close");
        match ev {
            NamespaceEvent::VhostRemoved { fqdn } => assert_eq!(fqdn, "e2e-del.example.com"),
            NamespaceEvent::VhostSet { fqdn, .. } => {
                panic!("expected VhostRemoved, got VhostSet {{ fqdn: {fqdn} }}")
            }
        }
    }
}
