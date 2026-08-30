//! SPEC 07 §3/§4 publish helpers — wire-message construction for the
//! `world.<svc>` retained snapshot and `<svc>.props.changed` event
//! family.
//!
//! Two daemons (noded and indexd) emit identical Bus wire shapes via
//! different transports — noded as the broker calls `broker.publish` on
//! its own `SubscriptionBroker`; indexd as a peer calls the
//! `topic.publish` RPC. The *trigger* (mutation hook vs periodic
//! diff) and the *transport* (broker handle vs NodedClient) are
//! per-daemon concerns; only the message shape is shared, and that's
//! what this module owns.
//!
//! Builders return an `BusMessage` rather than a wire string so
//! callers can attach extra headers (`name`, `retain`) before
//! rendering. The in-process broker path does not need topic/retain headers on the
//! inner message because those are arguments to `broker.publish`; the
//! peer path *does* need them on the outer `topic.publish` request,
//! not on this inner message — the broker re-parses the inner body
//! and routes by its own headers.

use crate::path::PropPath;
use crate::value::PropValue;
use cosmix_bus::bus::BusMessage;
use serde_json::Value as Json;

/// Topic name carrying a daemon's full retained property snapshot.
pub fn world_topic(svc: &str) -> String {
    format!("world.{svc}")
}

/// Topic name carrying a daemon's per-leaf props.changed event stream.
pub fn props_changed_topic(svc: &str) -> String {
    format!("{svc}.props.changed")
}

/// Build the inner Bus message for a `world.<svc>` retained publish.
///
/// `command` header is set to `svc` so subscribers can `on <svc> do …`
/// without a separate routing convention. Body is the JSON-serialised
/// (redacted) snapshot. Caller renders `to_wire()` and supplies the
/// topic/retain wrapper at the transport layer.
pub fn build_world_message(svc: &str, snapshot: &PropValue) -> BusMessage {
    let body = serde_json::to_string(&Json::from(snapshot)).unwrap_or_else(|_| "null".to_string());
    let mut m = BusMessage::new();
    m.set("command", svc);
    m.body = body;
    m
}

/// Build the inner Bus message for a single `<svc>.props.changed`
/// event.
///
/// Body shape per SPEC 07 §3.10: `{path, old, new, ts, cause}`.
/// Headers `command=props.changed`, `path=<path>`, `cause=<cause>` are
/// set so subscribers can filter without parsing the body.
pub fn build_props_changed_message(
    path: &PropPath,
    old: &PropValue,
    new: &PropValue,
    cause: &str,
) -> BusMessage {
    let body = serde_json::json!({
        "path": path.as_str(),
        "old": Json::from(old),
        "new": Json::from(new),
        "ts": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "cause": cause,
    })
    .to_string();

    let mut m = BusMessage::new();
    m.set("command", "props.changed");
    m.set("path", path.as_str());
    m.set("cause", cause);
    m.body = body;
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::build_snapshot;

    #[test]
    fn world_topic_format() {
        assert_eq!(world_topic("noded"), "world.noded");
        assert_eq!(world_topic("indexd"), "world.indexd");
    }

    #[test]
    fn props_changed_topic_format() {
        assert_eq!(props_changed_topic("noded"), "noded.props.changed");
        assert_eq!(props_changed_topic("indexd"), "indexd.props.changed");
    }

    #[test]
    fn build_world_message_shape() {
        let snap = build_snapshot([(
            PropPath::new("config.bind").unwrap(),
            PropValue::from("a:1"),
        )]);
        let m = build_world_message("noded", &snap);
        assert_eq!(m.get("command"), Some("noded"));
        let body: Json = serde_json::from_str(&m.body).unwrap();
        assert_eq!(body["config"]["bind"], "a:1");
    }

    #[test]
    fn build_props_changed_message_shape() {
        let p = PropPath::new("lifecycle.model_loaded").unwrap();
        let m = build_props_changed_message(
            &p,
            &PropValue::Bool(true),
            &PropValue::Bool(false),
            "snapshot",
        );
        assert_eq!(m.get("command"), Some("props.changed"));
        assert_eq!(m.get("path"), Some("lifecycle.model_loaded"));
        assert_eq!(m.get("cause"), Some("snapshot"));
        let body: Json = serde_json::from_str(&m.body).unwrap();
        assert_eq!(body["path"], "lifecycle.model_loaded");
        assert_eq!(body["old"], true);
        assert_eq!(body["new"], false);
        assert_eq!(body["cause"], "snapshot");
        assert!(body["ts"].as_str().unwrap().contains('T'));
    }
}
