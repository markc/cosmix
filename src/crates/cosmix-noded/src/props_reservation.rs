//! SPEC 12 §15.5 — broker reservation of property event topics.
//!
//! The property substrate publishes record/audit events on
//! per-service topics, but those topics are not free for any peer to
//! interact with. Reservation rules (SPEC 12 v0.1.2 BLOCKER fix):
//!
//! - `topic.publish` for `<svc>.props.changed`,
//!   `<svc>.props.records.changed`, and `<svc>.props.audit` is accepted only
//!   from the owning service
//!   (`<svc>` of the topic prefix) — anyone else faking publishes
//!   would let one daemon spoof another's audit stream.
//! - Direct `topic.subscribe` to the private records/audit topics is refused
//!   from **every** peer including the owning service itself. Subscribers go
//!   through the higher-level `<svc>.props.watch` /
//!   `<svc>.props.audit.watch` verbs, which
//!   re-check the capability vocabulary (§7.1) every time. Letting
//!   peers subscribe directly to the raw topic would bypass that.
//!   Flat `<svc>.props.changed` remains publicly subscribable.
//! - `topic.clear` on private topics is refused for any peer other than the
//!   owning service — only the owner can rewind its own retained snapshot.
//! - `topic.list` filters private reserved-prefix topics out of responses
//!   for any peer that is not the registered owning service, so
//!   reserved topics are not even discoverable.
//! - Private-topic `topic.subscriber_count` is refused for non-owning peers
//!   (cardinality is a side channel the owning service is allowed
//!   to read, third parties are not).
//!
//! `peer_id` here is the registered service name when the connection
//! has gone through `noded.register`; otherwise the connection's
//! synthesised anonymous id (which never matches a service name).
//! That mapping is established in `noded::handle_noded_command`; the
//! helpers below are pure and reused by unit tests.

/// Suffixes of reserved topic names. Order matters only for
/// matching — both are checked. Kept as private constants so callers
/// must go through [`reserved_owner`].
const PRIVATE_SUFFIXES: &[&str] = &[".props.records.changed", ".props.audit"];
const PUBLIC_CHANGED_SUFFIX: &str = ".props.changed";

/// If `name` is a reserved property-substrate topic, return the
/// owning service's name (the prefix before the matched suffix).
/// Otherwise return `None`.
///
/// Service names are allowed to contain dots (the matcher is greedy
/// on the suffix, not on the prefix), so e.g. `maild.r2.props.audit`
/// resolves to owner `maild.r2`.
pub(crate) fn reserved_owner(name: &str) -> Option<&str> {
    for suffix in PRIVATE_SUFFIXES {
        if let Some(owner) = name.strip_suffix(suffix)
            && !owner.is_empty()
        {
            return Some(owner);
        }
    }
    None
}

/// Owner of any property event topic whose publisher is authoritative. The
/// flat `<svc>.props.changed` topic remains directly subscribable; only its
/// publish side is reserved. Record/audit topics retain their stricter grant
/// rules through [`reserved_owner`].
pub(crate) fn publisher_owner(name: &str) -> Option<&str> {
    reserved_owner(name).or_else(|| {
        name.strip_suffix(PUBLIC_CHANGED_SUFFIX)
            .filter(|owner| !owner.is_empty())
    })
}

/// True iff this peer may publish to `name`. Non-reserved topics
/// always return true (this gate only narrows reserved-topic
/// access). Reserved topics require `peer_id == owner`.
pub(crate) fn may_publish(name: &str, peer_id: &str) -> bool {
    match publisher_owner(name) {
        Some(owner) => owner == peer_id,
        None => true,
    }
}

/// Private reserved-topic `subscribe` is refused unconditionally — even the
/// owning service goes through the higher-level `<svc>.props.watch`
/// surface so the capability vocabulary (§7.1) is re-checked.
pub(crate) fn may_subscribe(name: &str) -> bool {
    reserved_owner(name).is_none()
}

/// Private reserved-topic `clear` requires `peer_id == owner`.
pub(crate) fn may_clear(name: &str, peer_id: &str) -> bool {
    match reserved_owner(name) {
        Some(owner) => owner == peer_id,
        None => true,
    }
}

/// Private reserved-topic `subscriber_count` requires `peer_id == owner`
/// (count is a side channel for the owning service only).
pub(crate) fn may_read_count(name: &str, peer_id: &str) -> bool {
    match reserved_owner(name) {
        Some(owner) => owner == peer_id,
        None => true,
    }
}

/// True iff `topic.list` should include `name` for a peer with the
/// given `peer_id`. Non-reserved topics are always visible; reserved
/// topics are visible only to their owning service.
pub(crate) fn visible_in_list(name: &str, peer_id: &str) -> bool {
    match reserved_owner(name) {
        Some(owner) => owner == peer_id,
        None => true,
    }
}

/// SPEC 12 §15.5 / C10b defense-in-depth — when publishing to a
/// reserved props-substrate topic, strip every caller-supplied Bus
/// header from the inner message and force `command = <topic>`. The
/// legitimate publisher (`cosmix_props::dispatcher`) already
/// emits exactly this shape; this helper makes it a broker-side
/// invariant so a buggy or malicious owner cannot inject spoofed
/// `command` / `from` / `to` / `id` / `type` headers that the
/// granted subscriber's `cosmix-lib-client` reader_loop would
/// dispatch as a routed Bus command. The `topic_*` headers (stamped
/// by the broker on every publish) survive because the broker
/// re-applies them after this canonicalisation.
///
/// Returns `None` when `topic` is not reserved (caller passes the
/// publish body through unchanged) or when the inner fails to parse
/// (broker will reject downstream as `malformed_payload`). On the
/// reserved path with a parseable inner, returns the re-serialised
/// canonical wire bytes.
pub(crate) fn canonicalize_reserved_inner(topic: &str, inner_wire: &str) -> Option<String> {
    reserved_owner(topic)?;
    let mut inner = match cosmix_bus::bus::parse(inner_wire) {
        Ok(m) => m,
        Err(_) => return None,
    };
    inner.headers.clear();
    inner.set("command", topic);
    Some(inner.to_wire())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_records_changed_topic() {
        assert_eq!(reserved_owner("maild.props.records.changed"), Some("maild"));
    }

    #[test]
    fn detects_audit_topic() {
        assert_eq!(reserved_owner("maild.props.audit"), Some("maild"));
    }

    #[test]
    fn detects_public_changed_publisher_owner_without_making_it_private() {
        assert_eq!(publisher_owner("interact.props.changed"), Some("interact"));
        assert_eq!(reserved_owner("interact.props.changed"), None);
        assert!(may_subscribe("interact.props.changed"));
    }

    #[test]
    fn dotted_service_name_resolves_to_full_prefix() {
        assert_eq!(
            reserved_owner("maild.r2.props.records.changed"),
            Some("maild.r2")
        );
    }

    #[test]
    fn non_reserved_topic_returns_none() {
        // Plain user topics under the same service prefix must not be
        // collateral-damaged by the reservation gate.
        assert_eq!(reserved_owner("maild.props.custom"), None);
        assert_eq!(reserved_owner("maild.heartbeat"), None);
        assert_eq!(reserved_owner("props.records.changed"), None);
        assert_eq!(reserved_owner(""), None);
    }

    #[test]
    fn suffix_match_alone_is_not_enough() {
        // Empty owner prefix must not match (no service to own it).
        assert_eq!(reserved_owner(".props.audit"), None);
        assert_eq!(reserved_owner(".props.records.changed"), None);
    }

    #[test]
    fn publish_allowed_only_from_owner() {
        assert!(may_publish("maild.props.audit", "maild"));
        assert!(!may_publish("maild.props.audit", "webd"));
        assert!(!may_publish("maild.props.records.changed", "anon-42"));
        assert!(may_publish("interact.props.changed", "interact"));
        assert!(!may_publish("interact.props.changed", "musicd"));
        assert!(!may_publish("interact.props.changed", "anon-42"));
        // Non-reserved topics: anyone may publish.
        assert!(may_publish("noded.heartbeat", "anon-42"));
    }

    #[test]
    fn subscribe_refused_even_for_owner() {
        // Critical: owning service does NOT get a back door — it must
        // route through <svc>.props.watch, which re-checks capability.
        assert!(!may_subscribe("maild.props.audit"));
        assert!(!may_subscribe("maild.props.records.changed"));
        assert!(!may_subscribe("any.props.audit"));
        // Non-reserved topics unaffected.
        assert!(may_subscribe("noded.props.changed"));
    }

    #[test]
    fn clear_allowed_only_from_owner() {
        assert!(may_clear("maild.props.audit", "maild"));
        assert!(!may_clear("maild.props.audit", "webd"));
        assert!(may_clear("custom.topic", "anyone"));
    }

    #[test]
    fn subscriber_count_allowed_only_from_owner() {
        assert!(may_read_count("maild.props.audit", "maild"));
        assert!(!may_read_count("maild.props.audit", "webd"));
        assert!(may_read_count("custom.topic", "anyone"));
    }

    #[test]
    fn list_filters_reserved_topics_for_non_owners() {
        assert!(visible_in_list("maild.props.audit", "maild"));
        assert!(!visible_in_list("maild.props.audit", "webd"));
        assert!(!visible_in_list("maild.props.records.changed", "anon-1"));
        assert!(visible_in_list("noded.props.changed", "anyone"));
    }

    #[test]
    fn canonicalize_strips_spoofed_routing_headers_on_reserved_topic() {
        // Build an inner that a malicious owner might craft: a
        // routable Bus command with a spoofed `from` and a victim
        // payload. The granted subscriber's reader_loop would
        // otherwise dispatch this as a real incoming command.
        let mut spoof = cosmix_bus::bus::BusMessage::new();
        spoof.set("command", "maild.delete_all");
        spoof.set("from", "innocent-peer");
        spoof.set("to", "victim");
        spoof.set("type", "request");
        spoof.set("id", "evil-1");
        spoof.body = r#"{"namespace":"accounts","records":[]}"#.to_string();

        let canon = canonicalize_reserved_inner("maild.props.records.changed", &spoof.to_wire())
            .expect("reserved topic should canonicalize");
        let reparsed = cosmix_bus::bus::parse(&canon).expect("reparse");
        // Only `command` survives, set to the topic name. Body is
        // preserved so the namespace filter still matches.
        assert_eq!(
            reparsed.get("command"),
            Some("maild.props.records.changed"),
            "command must be the topic name, not the spoofed verb",
        );
        assert_eq!(reparsed.get("from"), None);
        assert_eq!(reparsed.get("to"), None);
        assert_eq!(reparsed.get("id"), None);
        assert_eq!(reparsed.get("type"), None);
        assert!(reparsed.body.contains(r#""namespace":"accounts""#));
    }

    #[test]
    fn canonicalize_passes_through_non_reserved_topic() {
        let mut msg = cosmix_bus::bus::BusMessage::new();
        msg.set("command", "user.evt");
        msg.body = r#"{"x":1}"#.to_string();
        assert!(canonicalize_reserved_inner("maild.heartbeat", &msg.to_wire()).is_none());
    }

    #[test]
    fn canonicalize_returns_none_for_unparseable_inner() {
        // Broker downstream raises malformed_payload; canonicaliser
        // declines to invent shape it doesn't have.
        assert!(canonicalize_reserved_inner("maild.props.audit", "not an bus message").is_none());
    }
}
