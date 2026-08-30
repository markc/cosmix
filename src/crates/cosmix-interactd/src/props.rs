//! The read-only SPEC-07 props surface for cosmix-interactd (`interact.props.*`).
//!
//! An OWNED snapshot implements `PropTree` (the filesd/indexd pattern — no lock
//! held inside the trait impl), built fresh per request from the in-process
//! `interactions` collection. Conformance is **L2**: `get`/`list`/`describe`
//! plus `props.watch` discovery and `interact.props.changed` lifecycle events.
//!
//! The collection is exposed as `notifications.<handle>.*` plus `stats.*` and
//! `lifecycle.*`. Handles are opaque hex tokens (`main.rs` mints them), so they
//! form valid single path segments; any handle that somehow does not is skipped
//! defensively rather than panicking the daemon.

use std::collections::BTreeMap;

use cosmix_interaction_schema::{DialogStateV1, NotifyRecord, NotifyState, Urgency};
use cosmix_props_core::publish::build_props_changed_message;
use cosmix_props_core::tree::build_snapshot;
use cosmix_props_core::{PropDescribe, PropPath, PropTree, PropType, PropValue};

use crate::PropsEvent;
use crate::state::DialogPropsRecord;

pub const PROPS_CHANGED_TOPIC: &str = "interact.props.changed";

/// An owned snapshot of the daemon's live interactions.
pub struct InteractionsProps {
    leaves: Vec<(PropPath, PropValue)>,
}

impl InteractionsProps {
    pub fn from_records(
        records: &BTreeMap<String, NotifyRecord>,
        dialogs: &BTreeMap<String, DialogPropsRecord>,
        event_seq: u64,
    ) -> Self {
        let live = records.values().filter(|r| !r.state.is_terminal()).count();

        let mut leaves: Vec<(PropPath, PropValue)> =
            Vec::with_capacity(4 + records.len() * 6 + dialogs.len() * 4);
        push(
            &mut leaves,
            "lifecycle.props_level",
            PropValue::from("L2".to_string()),
        );
        push(&mut leaves, "stats.live", PropValue::from(live as u64));
        push(
            &mut leaves,
            "lifecycle.event_seq",
            PropValue::from(event_seq),
        );
        push(
            &mut leaves,
            "stats.total",
            PropValue::from(records.len() as u64),
        );

        for record in records.values() {
            let base = format!("notifications.{}", record.handle.as_str());
            push(
                &mut leaves,
                &format!("{base}.origin"),
                PropValue::from(record.origin.clone()),
            );
            push(
                &mut leaves,
                &format!("{base}.state"),
                PropValue::from(state_str(record.state).to_string()),
            );
            push(
                &mut leaves,
                &format!("{base}.summary"),
                PropValue::from(record.summary.clone()),
            );
            // ms since epoch as a STRING leaf: PropValue numeric variants serialize
            // as JSON numbers → f64-precision loss for a large ms tip on a Mix/JS
            // consumer (same reasoning as filesd's modseq).
            push(
                &mut leaves,
                &format!("{base}.created_at_ms"),
                PropValue::from(record.created_at_ms.to_string()),
            );
            push(
                &mut leaves,
                &format!("{base}.urgency"),
                PropValue::from(urgency_str(record.effective_urgency).to_string()),
            );
            if let Some(key) = &record.dedupe_key {
                push(
                    &mut leaves,
                    &format!("{base}.dedupe_key"),
                    PropValue::from(key.clone()),
                );
            }
        }

        for record in dialogs.values() {
            let base = format!("dialogs.{}", record.handle.as_str());
            push(
                &mut leaves,
                &format!("{base}.origin"),
                PropValue::from(record.origin.clone()),
            );
            push(
                &mut leaves,
                &format!("{base}.state"),
                PropValue::from(dialog_state_str(record.state).to_string()),
            );
            push(
                &mut leaves,
                &format!("{base}.created_at_ms"),
                PropValue::from(record.created_at_ms.to_string()),
            );
            if let Some(fraction) = record.progress_fraction {
                push(
                    &mut leaves,
                    &format!("{base}.progress_fraction"),
                    PropValue::from(fraction),
                );
            }
        }

        InteractionsProps { leaves }
    }
}

impl PropTree for InteractionsProps {
    fn snapshot(&self) -> PropValue {
        build_snapshot(self.leaves.clone())
    }

    fn list(&self) -> Vec<PropPath> {
        self.leaves.iter().map(|(p, _)| p.clone()).collect()
    }

    fn describe(&self, p: &PropPath) -> Option<PropDescribe> {
        use PropType::{Number, String as Str};
        let d = match p.as_str() {
            "lifecycle.props_level" => {
                PropDescribe::leaf(p.clone(), Str, "SPEC 07 conformance level.")
            }
            "lifecycle.event_seq" => PropDescribe::leaf(
                p.clone(),
                Number,
                "Current daemon-session lifecycle event sequence watermark.",
            )
            .with_transient(true),
            "stats.live" => {
                PropDescribe::leaf(p.clone(), Number, "Non-terminal (live) notifications.")
                    .with_transient(true)
            }
            "stats.total" => PropDescribe::leaf(
                p.clone(),
                Number,
                "All notifications retained this session (live + terminal).",
            )
            .with_transient(true),
            _ => {
                if !self.leaves.iter().any(|(path, _)| path == p) {
                    return None;
                }
                let leaf = p.as_str().rsplit('.').next()?;
                let description = match leaf {
                    "origin" => "Broker-authenticated creating service.",
                    "state" => "Current interaction lifecycle state.",
                    "summary" => "Human-visible notification summary.",
                    "created_at_ms" => "Creation time as Unix epoch milliseconds.",
                    "urgency" => "Effective urgency after broker policy.",
                    "dedupe_key" => "Caller-supplied coalescing key.",
                    "progress_fraction" => "Current determinate dialog progress fraction.",
                    _ => return None,
                };
                let prop_type = if leaf == "progress_fraction" {
                    Number
                } else {
                    Str
                };
                PropDescribe::leaf(p.clone(), prop_type, description)
            }
        };
        Some(d)
    }
}

/// Build the standard SPEC-07 `props.changed` message for one lifecycle edge.
///
/// `seq` orders transitions within this daemon session. Publisher loss uses a
/// separate [`gap_message`] so a consumer never applies stale queued events
/// after reseeding.
pub fn transition_message(event: &PropsEvent) -> cosmix_bus::bus::BusMessage {
    match event {
        PropsEvent::Notification(transition) => {
            let path = PropPath::new(format!(
                "notifications.{}.state",
                transition.handle.as_str()
            ))
            .expect("server-minted notification handles are valid props segments");
            let old = transition
                .old
                .map(|state| PropValue::from(state_str(state).to_string()))
                .unwrap_or(PropValue::Null);
            let new = PropValue::from(state_str(transition.new).to_string());
            changed_message(&path, old, new, "notification.lifecycle", transition.seq)
        }
        PropsEvent::Dialog(transition) => {
            if transition.cause == cosmix_interaction_broker::DialogTransitionCause::ProgressUpdate
                && transition.old_progress_fraction != transition.new_progress_fraction
            {
                let path = PropPath::new(format!(
                    "dialogs.{}.progress_fraction",
                    transition.handle.as_str()
                ))
                .expect("server-minted dialog handles are valid props segments");
                let old = transition
                    .old_progress_fraction
                    .map(PropValue::from)
                    .unwrap_or(PropValue::Null);
                let new = transition
                    .new_progress_fraction
                    .map(PropValue::from)
                    .unwrap_or(PropValue::Null);
                return changed_message(
                    &path,
                    old,
                    new,
                    dialog_cause_str(transition.cause),
                    transition.seq,
                );
            }
            let path = PropPath::new(format!("dialogs.{}.state", transition.handle.as_str()))
                .expect("server-minted dialog handles are valid props segments");
            let old = transition
                .old
                .map(|state| PropValue::from(dialog_state_str(state).to_string()))
                .unwrap_or(PropValue::Null);
            let new = transition
                .new
                .map(|state| PropValue::from(dialog_state_str(state).to_string()))
                .unwrap_or(PropValue::Null);
            changed_message(
                &path,
                old,
                new,
                dialog_cause_str(transition.cause),
                transition.seq,
            )
        }
        PropsEvent::Resync { seq, snapshot } => {
            let mut message = cosmix_bus::bus::BusMessage::new();
            message.set("command", "props.changed");
            message.set("event_seq", &seq.to_string());
            message.set("resync", "true");
            message.set("cause", "dialog.transition_overflow");
            message.body = serde_json::json!({
                "seq": seq,
                "gap": false,
                "resync": true,
                "cause": "dialog.transition_overflow",
                "snapshot": snapshot,
            })
            .to_string();
            message
        }
    }
}

fn changed_message(
    path: &PropPath,
    old: PropValue,
    new: PropValue,
    cause: &str,
    seq: u64,
) -> cosmix_bus::bus::BusMessage {
    let mut message = build_props_changed_message(path, &old, &new, cause);
    message.set("event_seq", &seq.to_string());
    let mut body: serde_json::Value =
        serde_json::from_str(&message.body).expect("props-core changed body is valid JSON");
    body["seq"] = serde_json::json!(seq);
    body["gap"] = serde_json::json!(false);
    message.body = body.to_string();
    message
}

/// Explicit control frame emitted after bounded publisher loss. The publisher
/// discards all older queued events first, so `through_seq` is the last sequence
/// a consumer must regard as covered by its subsequent snapshot reseed.
pub fn gap_message(through_seq: u64, lost_count: u64) -> cosmix_bus::bus::BusMessage {
    let mut message = cosmix_bus::bus::BusMessage::new();
    message.set("command", "props.changed");
    message.set("event_seq", &through_seq.to_string());
    message.set("gap", "true");
    message.set("cause", "publisher.loss");
    message.body = serde_json::json!({
        "seq": through_seq,
        "gap": true,
        "lost_count": lost_count,
        "cause": "publisher.loss",
    })
    .to_string();
    message
}

/// Push a leaf, skipping (with a diagnostic) any path that is not a valid
/// `PropPath` — a runtime handle should always be valid, so this never fires in
/// practice, but a malformed one must not panic the daemon.
fn push(leaves: &mut Vec<(PropPath, PropValue)>, path: &str, value: PropValue) {
    match PropPath::new(path) {
        Ok(p) => leaves.push((p, value)),
        Err(_) => eprintln!("cosmix-interactd: [props] skipping invalid path {path:?}"),
    }
}

/// The wire string for a state (matches the schema's `snake_case` serde).
fn state_str(s: NotifyState) -> &'static str {
    match s {
        NotifyState::Queued => "queued",
        NotifyState::Shown => "shown",
        NotifyState::Dismissed => "dismissed",
        NotifyState::Expired => "expired",
        NotifyState::ActionInvoked => "action_invoked",
        NotifyState::Failed => "failed",
    }
}

pub(crate) fn dialog_state_str(s: DialogStateV1) -> &'static str {
    match s {
        DialogStateV1::Queued => "queued",
        DialogStateV1::Presenting => "presenting",
        DialogStateV1::Presented => "presented",
        DialogStateV1::CancelRequested => "cancel-requested",
        DialogStateV1::Resolved => "resolved",
        DialogStateV1::Cancelled => "cancelled",
        DialogStateV1::Expired => "expired",
        DialogStateV1::Failed => "failed",
    }
}

fn dialog_cause_str(cause: cosmix_interaction_broker::DialogTransitionCause) -> &'static str {
    use cosmix_interaction_broker::DialogTransitionCause;
    match cause {
        DialogTransitionCause::Open => "dialog.open",
        DialogTransitionCause::Present => "dialog.present",
        DialogTransitionCause::MarkPresented => "dialog.mark_presented",
        DialogTransitionCause::Resolve => "dialog.resolve",
        DialogTransitionCause::Fail => "dialog.fail",
        DialogTransitionCause::Cancel => "dialog.cancel",
        DialogTransitionCause::ProgressUpdate => "dialog.progress_update",
        DialogTransitionCause::ProgressComplete => "dialog.progress_complete",
        DialogTransitionCause::ProgressCancel => "dialog.progress_cancel",
        DialogTransitionCause::Replace => "dialog.replace",
        DialogTransitionCause::Release => "dialog.release",
        DialogTransitionCause::Expire => "dialog.expire",
        DialogTransitionCause::Evict => "dialog.evict",
        DialogTransitionCause::Withdraw => "dialog.withdraw",
        DialogTransitionCause::Quarantine => "dialog.quarantine",
    }
}

/// The wire string for an urgency (matches the schema's `lowercase` serde).
fn urgency_str(u: Urgency) -> &'static str {
    match u {
        Urgency::Low => "low",
        Urgency::Normal => "normal",
        Urgency::Critical => "critical",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::PropsStateTransition;
    use cosmix_interaction_schema::NotifyHandle;
    use serde_json::Value;

    fn record(handle: &str, origin: &str, state: NotifyState) -> NotifyRecord {
        NotifyRecord {
            handle: NotifyHandle(handle.into()),
            origin: origin.into(),
            state,
            created_at_ms: 1_700_000_000_000,
            summary: "hi".into(),
            effective_urgency: Urgency::Normal,
            urgency_override: None,
            dedupe_key: None,
        }
    }

    #[test]
    fn snapshot_nests_notifications_and_counts_live() {
        let mut records = BTreeMap::new();
        records.insert("n1".into(), record("n1", "musicd", NotifyState::Shown));
        records.insert("n2".into(), record("n2", "filesd", NotifyState::Dismissed));
        let props = InteractionsProps::from_records(&records, &BTreeMap::new(), 7);
        let snap: Value = serde_json::to_value(props.snapshot()).unwrap();

        assert_eq!(snap["stats"]["live"], 1);
        assert_eq!(snap["stats"]["total"], 2);
        assert_eq!(snap["lifecycle"]["event_seq"], 7);
        assert_eq!(snap["notifications"]["n1"]["origin"], "musicd");
        assert_eq!(snap["notifications"]["n1"]["state"], "shown");
        assert_eq!(snap["notifications"]["n1"]["urgency"], "normal");
        assert_eq!(snap["notifications"]["n2"]["state"], "dismissed");
        assert!(
            !serde_json::to_string(&snap)
                .unwrap()
                .contains("owner_token"),
            "props must never expose the mutation capability"
        );
        // created_at_ms is a precision-safe string
        assert_eq!(
            snap["notifications"]["n1"]["created_at_ms"],
            "1700000000000"
        );
    }

    #[test]
    fn empty_collection_has_zero_counts() {
        let props = InteractionsProps::from_records(&BTreeMap::new(), &BTreeMap::new(), 0);
        let snap: Value = serde_json::to_value(props.snapshot()).unwrap();
        assert_eq!(snap["stats"]["live"], 0);
        assert_eq!(snap["stats"]["total"], 0);
    }

    #[test]
    fn every_listed_leaf_supports_get_and_describe() {
        let mut records = BTreeMap::new();
        let mut notification = record("n1", "musicd", NotifyState::Shown);
        notification.dedupe_key = Some("transport".into());
        records.insert("n1".into(), notification);
        let props = InteractionsProps::from_records(&records, &BTreeMap::new(), 1);

        for path in props.list() {
            for verb in ["get", "describe"] {
                let args = serde_json::json!({ "path": path.as_str() });
                let response =
                    cosmix_props_core::bus::dispatch_props(&props, verb, Some(&args), true);
                assert_eq!(
                    response.rc, 0,
                    "{verb} failed for {path}: {}",
                    response.body
                );
            }
        }
    }

    #[test]
    fn lifecycle_transition_uses_standard_changed_shape() {
        let message = transition_message(&PropsEvent::Notification(PropsStateTransition {
            seq: 7,
            handle: NotifyHandle("n1".into()),
            old: Some(NotifyState::Queued),
            new: NotifyState::Shown,
        }));
        assert_eq!(message.get("command"), Some("props.changed"));
        assert_eq!(message.get("path"), Some("notifications.n1.state"));
        let body: Value = serde_json::from_str(&message.body).unwrap();
        assert_eq!(body["old"], "queued");
        assert_eq!(body["new"], "shown");
        assert_eq!(body["seq"], 7);
        assert_eq!(body["gap"], false);
    }

    #[test]
    fn lifecycle_transition_reports_bounded_publisher_loss() {
        let message = gap_message(9, 3);
        assert_eq!(message.get("event_seq"), Some("9"));
        assert_eq!(message.get("gap"), Some("true"));
        let body: Value = serde_json::from_str(&message.body).unwrap();
        assert_eq!(body["gap"], true);
        assert_eq!(body["lost_count"], 3);
    }

    #[test]
    fn dialog_snapshot_projects_only_non_content_fields() {
        let mut dialogs = BTreeMap::new();
        dialogs.insert(
            "d1".into(),
            DialogPropsRecord {
                handle: NotifyHandle("d1".into()),
                origin: "musicd".into(),
                state: DialogStateV1::Presented,
                created_at_ms: 1_700_000_000_001,
                progress_fraction: Some(0.25),
            },
        );
        let props = InteractionsProps::from_records(&BTreeMap::new(), &dialogs, 4);
        let snapshot: Value = serde_json::to_value(props.snapshot()).unwrap();
        let dialog = &snapshot["dialogs"]["d1"];

        assert_eq!(dialog["origin"], "musicd");
        assert_eq!(dialog["state"], "presented");
        assert_eq!(dialog["created_at_ms"], "1700000000001");
        assert_eq!(dialog["progress_fraction"], 0.25);
        let object = dialog.as_object().unwrap();
        for forbidden in [
            "title",
            "message",
            "answer",
            "value",
            "token",
            "owner_token",
        ] {
            assert!(!object.contains_key(forbidden), "leaked {forbidden}");
        }
    }

    #[test]
    fn dialog_transition_and_overflow_resync_use_props_changed_topic_shape() {
        let transition = PropsEvent::Dialog(crate::state::DialogPropsTransition {
            seq: 8,
            handle: NotifyHandle("d1".into()),
            old: Some(DialogStateV1::Queued),
            new: Some(DialogStateV1::Presenting),
            cause: cosmix_interaction_broker::DialogTransitionCause::Present,
            old_progress_fraction: None,
            new_progress_fraction: None,
        });
        let message = transition_message(&transition);
        assert_eq!(message.get("path"), Some("dialogs.d1.state"));
        let body: Value = serde_json::from_str(&message.body).unwrap();
        assert_eq!(body["old"], "queued");
        assert_eq!(body["new"], "presenting");

        let resync = transition_message(&PropsEvent::Resync {
            seq: 9,
            snapshot: serde_json::json!({"dialogs": {"d1": {"state": "presenting"}}}),
        });
        assert_eq!(resync.get("resync"), Some("true"));
        let body: Value = serde_json::from_str(&resync.body).unwrap();
        assert_eq!(body["resync"], true);
        assert_eq!(body["snapshot"]["dialogs"]["d1"]["state"], "presenting");

        let progress =
            transition_message(&PropsEvent::Dialog(crate::state::DialogPropsTransition {
                seq: 10,
                handle: NotifyHandle("d1".into()),
                old: Some(DialogStateV1::Presented),
                new: Some(DialogStateV1::Presented),
                cause: cosmix_interaction_broker::DialogTransitionCause::ProgressUpdate,
                old_progress_fraction: Some(0.25),
                new_progress_fraction: Some(0.5),
            }));
        assert_eq!(progress.get("path"), Some("dialogs.d1.progress_fraction"));
        let body: Value = serde_json::from_str(&progress.body).unwrap();
        assert_eq!(body["old"], 0.25);
        assert_eq!(body["new"], 0.5);
    }
}
