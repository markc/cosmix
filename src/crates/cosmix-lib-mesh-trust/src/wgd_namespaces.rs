//! Draft `NamespaceSpec` values for the five `wgd.*` namespaces that
//! back the cross-mesh authz machinery (plan §4, §5, §7, §9, §10).
//!
//! These are **drafts**, not yet registered with a running wgd. P0-C6
//! commits to the schema shape; the wgd-side registration (matching
//! `AuthPolicy`, validation hooks, storage backend wiring) is a
//! later, wgd-owned change.
//!
//! Each spec carries:
//!
//! - The exact field list from the plan's §N table.
//! - `Cardinality::Collection` with the plan's PK field.
//! - `StorageBackendKind::SqliteTable` with a conservative table name
//!   `wgd_<name>` — wgd is free to override at registration.
//! - `AuthPolicy::deny_all()` as a placeholder. wgd's registration
//!   replaces this with the real capability mapping (operators +
//!   combinator lookups). Drafting the *auth* is wgd's concern; this
//!   crate only commits to the *schema*.
//!
//! The conversion between these wire records and the core
//! [`TrustedMesh`] / [`Grant`] / [`CapabilitySet`] types the
//! combinator consumes lives in the subscriber (P0-C5); this module
//! is the schema definition only.

use cosmix_props::{
    AuthPolicy, Cardinality, FieldSchema, FieldType, NamespaceName, NamespaceSpec, PropertySchema,
    StorageBackendKind,
};

fn primitive(name: &str, ty: FieldType, help: &str) -> FieldSchema {
    FieldSchema {
        name: name.to_owned(),
        ty,
        default: None,
        secret: false,
        help: help.to_owned(),
        since: None,
        until: None,
        validators: Vec::new(),
    }
}

fn optional(name: &str, inner: FieldType, help: &str) -> FieldSchema {
    primitive(
        name,
        FieldType::Option {
            inner: Box::new(inner),
        },
        help,
    )
}

fn list_of(name: &str, item: FieldType, help: &str) -> FieldSchema {
    primitive(
        name,
        FieldType::List {
            item: Box::new(item),
        },
        help,
    )
}

fn name(unqualified: &str) -> NamespaceName {
    NamespaceName::new(unqualified)
        .unwrap_or_else(|e| panic!("invalid namespace name {unqualified:?}: {e}"))
}

fn sqlite_table(table: &str) -> StorageBackendKind {
    StorageBackendKind::SqliteTable {
        table: table.to_owned(),
    }
}

/// Plan §4 — `wgd.trusted_meshes`. Collection, PK = `mesh_fqdn`.
pub fn trusted_meshes_spec() -> NamespaceSpec {
    let fields = vec![
        primitive(
            "mesh_fqdn",
            FieldType::String,
            "PK. FQDN at which _cosmix-mesh is published.",
        ),
        primitive(
            "current_pubkey",
            FieldType::Bytes,
            "Ed25519 public key (32 bytes); base64 on the wire.",
        ),
        primitive(
            "current_selector",
            FieldType::String,
            "DNS selector label (e.g. \"2026q3\").",
        ),
        optional(
            "previous_pubkey",
            FieldType::Bytes,
            "Prior key, valid during grace window.",
        ),
        optional("previous_selector", FieldType::String, "Prior selector."),
        optional(
            "previous_pubkey_valid_until",
            FieldType::String,
            "RFC 3339 hard deadline after which previous_pubkey is rejected. \
             Mandatory whenever previous_pubkey is set.",
        ),
        primitive("trust_added_at", FieldType::String, "RFC 3339."),
        primitive(
            "trust_added_by",
            FieldType::String,
            "Operator peer-id form.",
        ),
        primitive(
            "trust_label",
            FieldType::String,
            "Free-form operator label.",
        ),
        primitive(
            "last_verified_at",
            FieldType::String,
            "RFC 3339 of last successful pubkey refresh.",
        ),
        primitive(
            "max_pubkey_age_secs",
            FieldType::U64,
            "Per-mesh freshness override. Default 7*86400.",
        ),
        primitive(
            "enabled",
            FieldType::Bool,
            "If false, all grants under this mesh are inert.",
        ),
        optional("notes", FieldType::String, "Free-form."),
    ];
    NamespaceSpec::new(
        name("trusted_meshes"),
        PropertySchema::new(fields),
        Cardinality::Collection {
            primary_key_field: "mesh_fqdn".to_owned(),
        },
        sqlite_table("wgd_trusted_meshes"),
    )
}

/// Plan §5 — `wgd.grants`. Collection, PK = `grant_id` (ULID).
pub fn grants_spec() -> NamespaceSpec {
    let fields = vec![
        primitive("grant_id", FieldType::String, "PK (ULID)."),
        primitive(
            "mesh_fqdn",
            FieldType::String,
            "The granted principal. FK to trusted_meshes.",
        ),
        list_of(
            "capabilities",
            FieldType::String,
            "SPEC-12 §7.1 capability tokens.",
        ),
        primitive("valid_from", FieldType::String, "RFC 3339, mandatory."),
        primitive(
            "valid_until",
            FieldType::String,
            "RFC 3339, mandatory. No perpetual grants.",
        ),
        primitive("granted_by", FieldType::String, "Operator peer-id."),
        primitive("granted_at", FieldType::String, "RFC 3339."),
        primitive("reason", FieldType::String, "Operator-stated purpose."),
        primitive("enabled", FieldType::Bool, "Immediate-effect flag."),
        optional(
            "revocation_reason",
            FieldType::String,
            "Set when enabled flips to false.",
        ),
    ];
    NamespaceSpec::new(
        name("grants"),
        PropertySchema::new(fields),
        Cardinality::Collection {
            primary_key_field: "grant_id".to_owned(),
        },
        sqlite_table("wgd_grants"),
    )
}

/// Plan §9 — `wgd.cross_mesh_exposable`. Collection, PK =
/// `<owning_service>:<cap_token>` stored as `pk`.
pub fn cross_mesh_exposable_spec() -> NamespaceSpec {
    let fields = vec![
        primitive("pk", FieldType::String, "<owning_service>:<cap_token>."),
        primitive(
            "owning_service",
            FieldType::String,
            "Service registering the capability; must match the writer's \
             broker-attached service_name per SPEC-12 §7.3.",
        ),
        primitive(
            "cap_token",
            FieldType::String,
            "A SPEC-12 §7.1 capability token (props.read:<svc>.<ns> etc.).",
        ),
        primitive(
            "cap_namespace_prefix",
            FieldType::String,
            "<svc>.<ns> extracted from cap_token; redundant but stored for \
             fast prefix scans by the grant validator.",
        ),
        primitive("registered_at", FieldType::String, "RFC 3339."),
        primitive(
            "lifetime_kind",
            FieldType::Enum {
                variants: vec!["ephemeral".to_owned(), "persistent".to_owned()],
            },
            "ephemeral: cleared on service-start, repopulated by the service. \
             persistent: stays across restarts; only deleted by operator. \
             v1 default is ephemeral.",
        ),
        optional("notes", FieldType::String, "Free-form."),
    ];
    NamespaceSpec::new(
        name("cross_mesh_exposable"),
        PropertySchema::new(fields),
        Cardinality::Collection {
            primary_key_field: "pk".to_owned(),
        },
        sqlite_table("wgd_cross_mesh_exposable"),
    )
}

/// Plan §7 — `wgd.replay_nonces`. Collection, PK = `<caller_mesh>:<nonce>`
/// stored as `pk`.
pub fn replay_nonces_spec() -> NamespaceSpec {
    let fields = vec![
        primitive("pk", FieldType::String, "<caller_mesh>:<nonce>."),
        primitive("ts", FieldType::String, "RFC 3339, envelope ts."),
        primitive(
            "seen_at",
            FieldType::String,
            "RFC 3339, when wgd recorded it.",
        ),
        primitive(
            "expires_at",
            FieldType::String,
            "RFC 3339, seen_at + replay_window.",
        ),
    ];
    NamespaceSpec::new(
        name("replay_nonces"),
        PropertySchema::new(fields),
        Cardinality::Collection {
            primary_key_field: "pk".to_owned(),
        },
        sqlite_table("wgd_replay_nonces"),
    )
}

/// Plan §10 — `wgd.cross_mesh_audit`. Collection, PK = `audit_id` (ULID).
pub fn cross_mesh_audit_spec() -> NamespaceSpec {
    let fields = vec![
        primitive("audit_id", FieldType::String, "PK (ULID)."),
        primitive("ts", FieldType::String, "RFC 3339, when the call arrived."),
        primitive(
            "transport",
            FieldType::Enum {
                variants: vec!["cross_mesh_https".to_owned()],
            },
            "v1 has one value; reserved so a future cross-mesh Bus transport \
             gets its own.",
        ),
        optional(
            "caller_mesh",
            FieldType::String,
            "From envelope. Null on rejected_parse.",
        ),
        optional(
            "caller_node_id",
            FieldType::String,
            "From envelope, raw. Null on rejected_parse.",
        ),
        optional(
            "caller_selector",
            FieldType::String,
            "From envelope. Null on rejected_parse.",
        ),
        primitive("target_service", FieldType::String, "Receiving service."),
        primitive(
            "target_endpoint",
            FieldType::String,
            "The hit endpoint path.",
        ),
        optional(
            "command",
            FieldType::String,
            "Attempted Bus command. Null on rejected_parse.",
        ),
        primitive(
            "result",
            FieldType::Enum {
                variants: vec![
                    "pending".to_owned(),
                    "pending_orphan".to_owned(),
                    "accepted".to_owned(),
                    "rejected_audience".to_owned(),
                    "rejected_parse".to_owned(),
                    "rejected_no_trust".to_owned(),
                    "rejected_disabled_trust".to_owned(),
                    "rejected_stale_trust".to_owned(),
                    "rejected_bad_signature".to_owned(),
                    "rejected_stale_envelope".to_owned(),
                    "rejected_replayed".to_owned(),
                    "rejected_no_matching_grant".to_owned(),
                    "rejected_auth_denied".to_owned(),
                ],
            },
            "Outcome. pending is reserved pre-dispatch; pending_orphan is set \
             by the stuck-row sweep.",
        ),
        list_of(
            "grant_ids",
            FieldType::String,
            "Set when result=accepted if the dispatcher hook returned them; \
             empty otherwise.",
        ),
        list_of(
            "effective_caps",
            FieldType::String,
            "Intersected cap set actually carried into the dispatcher \
             (verifier-computed).",
        ),
        primitive("latency_ms", FieldType::U64, "Verifier wall-clock latency."),
    ];
    NamespaceSpec::new(
        name("cross_mesh_audit"),
        PropertySchema::new(fields),
        Cardinality::Collection {
            primary_key_field: "audit_id".to_owned(),
        },
        sqlite_table("wgd_cross_mesh_audit"),
    )
}

/// Convenience: return all five drafts in plan order. wgd's registration
/// path can iterate this to register against its property runtime.
///
/// `auth` on every spec is [`AuthPolicy::deny_all`]; wgd MUST replace
/// per the per-namespace policy described in plan §4–§10 before
/// registering.
pub fn all_specs() -> [NamespaceSpec; 5] {
    [
        trusted_meshes_spec(),
        grants_spec(),
        replay_nonces_spec(),
        cross_mesh_exposable_spec(),
        cross_mesh_audit_spec(),
    ]
}

// `AuthPolicy::deny_all` is referenced through the module docs above but
// not used at construction time — every spec carries it implicitly via
// `NamespaceSpec::new`. The import below keeps the reference live for
// `cargo doc` cross-linking.
#[allow(dead_code)]
fn _auth_policy_doc_anchor() -> AuthPolicy {
    AuthPolicy::deny_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk_field(spec: &NamespaceSpec) -> &str {
        match &spec.cardinality {
            Cardinality::Collection { primary_key_field } => primary_key_field.as_str(),
            Cardinality::Singleton { .. } => panic!("expected collection"),
        }
    }

    fn field_type<'a>(spec: &'a NamespaceSpec, name: &str) -> &'a FieldType {
        &spec
            .schema
            .field(name)
            .unwrap_or_else(|| panic!("missing field {name}"))
            .ty
    }

    #[test]
    fn each_spec_is_a_collection_with_pk_present_in_schema() {
        for spec in all_specs() {
            let pk = pk_field(&spec).to_owned();
            assert!(
                spec.schema.field(&pk).is_some(),
                "{}: pk field {pk:?} not in schema",
                spec.fqn("wgd")
            );
        }
    }

    #[test]
    fn fqns_match_plan() {
        // Plan §4/§5/§7/§9/§10 — fully-qualified namespace names that
        // the combinator and operator tooling will reference verbatim.
        let specs = all_specs();
        let fqns: Vec<String> = specs.iter().map(|s| s.fqn("wgd")).collect();
        assert_eq!(
            fqns,
            [
                "wgd.trusted_meshes",
                "wgd.grants",
                "wgd.replay_nonces",
                "wgd.cross_mesh_exposable",
                "wgd.cross_mesh_audit",
            ]
        );
    }

    #[test]
    fn trusted_meshes_schema_shape() {
        let s = trusted_meshes_spec();
        assert!(matches!(field_type(&s, "mesh_fqdn"), FieldType::String));
        assert!(matches!(field_type(&s, "current_pubkey"), FieldType::Bytes));
        // Three "previous_*" fields are all optional — the rotation
        // grace window can be entirely empty.
        for f in [
            "previous_pubkey",
            "previous_selector",
            "previous_pubkey_valid_until",
        ] {
            assert!(
                matches!(field_type(&s, f), FieldType::Option { .. }),
                "{f} should be Option"
            );
        }
        assert!(matches!(
            field_type(&s, "max_pubkey_age_secs"),
            FieldType::U64
        ));
        assert!(matches!(field_type(&s, "enabled"), FieldType::Bool));
        assert_eq!(pk_field(&s), "mesh_fqdn");
    }

    #[test]
    fn grants_schema_shape() {
        let s = grants_spec();
        assert!(matches!(field_type(&s, "grant_id"), FieldType::String));
        // capabilities is the load-bearing list — verify the inner
        // type so the subscriber's record→core conversion has a stable
        // target.
        match field_type(&s, "capabilities") {
            FieldType::List { item } => {
                assert!(matches!(**item, FieldType::String));
            }
            other => panic!("capabilities should be list<string>, got {other:?}"),
        }
        assert!(matches!(field_type(&s, "valid_from"), FieldType::String));
        assert!(matches!(field_type(&s, "valid_until"), FieldType::String));
        assert!(matches!(field_type(&s, "enabled"), FieldType::Bool));
        assert!(matches!(
            field_type(&s, "revocation_reason"),
            FieldType::Option { .. }
        ));
        assert_eq!(pk_field(&s), "grant_id");
    }

    #[test]
    fn cross_mesh_exposable_lifetime_kind_is_ephemeral_persistent() {
        let s = cross_mesh_exposable_spec();
        match field_type(&s, "lifetime_kind") {
            FieldType::Enum { variants } => {
                assert_eq!(variants, &["ephemeral".to_owned(), "persistent".to_owned()]);
            }
            other => panic!("lifetime_kind should be enum, got {other:?}"),
        }
        assert_eq!(pk_field(&s), "pk");
    }

    #[test]
    fn replay_nonces_minimal_shape() {
        let s = replay_nonces_spec();
        for f in ["pk", "ts", "seen_at", "expires_at"] {
            assert!(matches!(field_type(&s, f), FieldType::String));
        }
        assert_eq!(s.schema.fields.len(), 4);
        assert_eq!(pk_field(&s), "pk");
    }

    #[test]
    fn cross_mesh_audit_result_variants_match_plan() {
        let s = cross_mesh_audit_spec();
        match field_type(&s, "result") {
            FieldType::Enum { variants } => {
                for required in [
                    "pending",
                    "pending_orphan",
                    "accepted",
                    "rejected_audience",
                    "rejected_parse",
                    "rejected_no_trust",
                    "rejected_disabled_trust",
                    "rejected_stale_trust",
                    "rejected_bad_signature",
                    "rejected_stale_envelope",
                    "rejected_replayed",
                    "rejected_no_matching_grant",
                    "rejected_auth_denied",
                ] {
                    assert!(
                        variants.iter().any(|v| v == required),
                        "result enum missing {required}"
                    );
                }
            }
            other => panic!("result should be enum, got {other:?}"),
        }
        assert!(matches!(
            field_type(&s, "transport"),
            FieldType::Enum { .. }
        ));
        assert!(matches!(field_type(&s, "latency_ms"), FieldType::U64));
        // The three caller_* envelope fields are optional because
        // rejected_parse can fire before they can be extracted.
        for f in [
            "caller_mesh",
            "caller_node_id",
            "caller_selector",
            "command",
        ] {
            assert!(
                matches!(field_type(&s, f), FieldType::Option { .. }),
                "{f} should be Option"
            );
        }
        assert_eq!(pk_field(&s), "audit_id");
    }

    #[test]
    fn all_specs_default_to_sqlite_table_under_wgd_prefix() {
        for spec in all_specs() {
            match &spec.storage {
                StorageBackendKind::SqliteTable { table } => {
                    assert!(
                        table.starts_with("wgd_"),
                        "{}: table {table:?} should be wgd_-prefixed",
                        spec.fqn("wgd")
                    );
                }
                other => panic!("expected SqliteTable, got {other:?}"),
            }
        }
    }
}
