//! Integration tests for `register_log_namespace` and the hook
//! enforcement points (plan §4 / §4.1). These run against a real
//! `SqliteStore::new(...)` over an in-memory connection and a real
//! `PropsRouter`, so they exercise the same code paths a citizen
//! daemon will hit at boot.

use std::collections::BTreeMap;
use std::sync::Arc;

use cosmix_log_props::{
    INVALID_FILTER_PREFIX, UNDELETABLE_PREFIX, UNKNOWN_ENUM_VARIANT_PREFIX, register_log_namespace,
};
use cosmix_props::bus::mutation::PropsRouter;
use cosmix_props::record::{Actor, RecordKey, Version};
use cosmix_props::runtime::{DeleteOpts, SetOpts};
use cosmix_props::sqlite::SqliteStore;
use cosmix_props::store::MergeMode;
use cosmix_props::value::PropValue;
use rusqlite::Connection;

fn build_store(service: &str) -> Arc<SqliteStore> {
    let conn = Connection::open_in_memory().expect("open in-memory sqlite");
    Arc::new(SqliteStore::new(service, conn).expect("init store schema"))
}

fn singleton_key() -> RecordKey {
    RecordKey::singleton(cosmix_log_props::namespace_name())
}

fn set_opts(merge: MergeMode) -> SetOpts {
    SetOpts {
        expected_version: Some(Version::zero()),
        merge,
        actor: Actor::operator("test").expect("valid actor"),
        cause: None,
        ts_ms: 0,
    }
}

fn body(pairs: &[(&str, &str)]) -> PropValue {
    let mut m = BTreeMap::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), PropValue::String((*v).to_string()));
    }
    PropValue::Object(m)
}

#[tokio::test(flavor = "multi_thread")]
async fn registers_cleanly_against_propsrouter() {
    let store = build_store("maild");
    let mut router = PropsRouter::new("maild");
    let runtime = register_log_namespace(&mut router, &store)
        .expect("register log namespace once")
        .runtime()
        .clone();
    assert_eq!(runtime.spec().name.as_str(), "log");
}

#[tokio::test(flavor = "multi_thread")]
async fn re_registering_is_a_conflict() {
    let store = build_store("maild");
    let mut router = PropsRouter::new("maild");
    register_log_namespace(&mut router, &store).expect("first registration");
    let err = match register_log_namespace(&mut router, &store) {
        Ok(_) => panic!("second registration must conflict on the router"),
        Err(e) => e,
    };
    assert!(
        format!("{err}").contains("log"),
        "error should mention the namespace: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_with_valid_payload_succeeds() {
    let store = build_store("maild");
    let mut router = PropsRouter::new("maild");
    let runtime = register_log_namespace(&mut router, &store)
        .unwrap()
        .runtime()
        .clone();

    let value = body(&[
        ("level", "debug"),
        ("filter", "cosmix_log=debug"),
        ("format", "json"),
        ("applied_at", "2026-05-21T00:00:00Z"),
        ("applied_by", "props"),
        ("source", "debug,cosmix_log=debug"),
    ]);
    runtime
        .set(singleton_key(), value, set_opts(MergeMode::Replace))
        .await
        .expect("valid log row should commit");
}

#[tokio::test(flavor = "multi_thread")]
async fn set_with_malformed_filter_is_rejected() {
    let store = build_store("maild");
    let mut router = PropsRouter::new("maild");
    let runtime = register_log_namespace(&mut router, &store)
        .unwrap()
        .runtime()
        .clone();

    let value = body(&[
        ("level", "info"),
        ("filter", "this is not a valid envfilter ::: ==="),
        ("format", "human"),
        ("applied_at", ""),
        ("applied_by", "default"),
        ("source", ""),
    ]);
    let err = runtime
        .set(singleton_key(), value, set_opts(MergeMode::Replace))
        .await
        .expect_err("malformed filter must be rejected by before_set");
    let msg = format!("{err}");
    assert!(
        msg.contains(INVALID_FILTER_PREFIX),
        "error should carry the invalid-filter prefix: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_with_unknown_level_is_rejected() {
    let store = build_store("maild");
    let mut router = PropsRouter::new("maild");
    let runtime = register_log_namespace(&mut router, &store)
        .unwrap()
        .runtime()
        .clone();

    let value = body(&[
        ("level", "verbose"),
        ("filter", ""),
        ("format", "human"),
        ("applied_at", ""),
        ("applied_by", "default"),
        ("source", ""),
    ]);
    let err = runtime
        .set(singleton_key(), value, set_opts(MergeMode::Replace))
        .await
        .expect_err("unknown level variant must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains(UNKNOWN_ENUM_VARIANT_PREFIX),
        "error should carry the unknown-enum prefix: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_is_refused() {
    let store = build_store("maild");
    let mut router = PropsRouter::new("maild");
    let runtime = register_log_namespace(&mut router, &store)
        .unwrap()
        .runtime()
        .clone();

    // Materialise the row first so delete has something to address.
    runtime
        .set(
            singleton_key(),
            body(&[
                ("level", "info"),
                ("filter", ""),
                ("format", "human"),
                ("applied_at", ""),
                ("applied_by", "default"),
                ("source", ""),
            ]),
            set_opts(MergeMode::Replace),
        )
        .await
        .expect("seed row");

    let err = runtime
        .delete(
            singleton_key(),
            DeleteOpts {
                expected_version: None,
                actor: Actor::operator("test").expect("valid actor"),
                cause: None,
                ts_ms: 1,
            },
        )
        .await
        .expect_err("delete must be rejected on this singleton");
    let msg = format!("{err}");
    assert!(
        msg.contains(UNDELETABLE_PREFIX),
        "error should carry the undeletable prefix: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn set_carries_the_service_prefix_in_capability_names() {
    // The capability set returned by `auth_policy("webd")` must scope
    // to `<service>.log`. This is what stops a misconfigured peer
    // holding `props.write:maild.log` from silencing webd's logging.
    let webd_policy = cosmix_log_props::auth_policy("webd");
    let caps = webd_policy.resolve(&Default::default());
    let names: Vec<String> = caps.iter().map(|c| c.as_str().to_string()).collect();
    assert!(
        names.iter().any(|n| n == "props.read:webd.log"),
        "expected props.read:webd.log in {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "props.write:webd.log"),
        "expected props.write:webd.log in {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("maild.log")),
        "webd policy must not contain maild.log caps: {names:?}"
    );
}
