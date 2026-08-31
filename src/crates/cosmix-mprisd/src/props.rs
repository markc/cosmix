//! Read-only SPEC-07/SPEC-12 property projection for mpris.props.*.

use cosmix_props_core::tree::build_snapshot;
use cosmix_props_core::{PropDescribe, PropPath, PropTree, PropType, PropValue};

use crate::core::{MprisSnapshot, PlayerSnapshot};

pub struct MprisProps {
    leaves: Vec<(PropPath, PropValue)>,
}

impl MprisProps {
    pub fn new(
        snapshot: &MprisSnapshot,
        event_seq: u64,
        publisher_loss: u64,
        controls_dropped: u64,
        now_us: u64,
    ) -> Self {
        let mut leaves = Vec::with_capacity(15 + snapshot.players.len() * 20);
        push(&mut leaves, "lifecycle.props_level", "L2".into());
        push(&mut leaves, "lifecycle.event_seq", event_seq.into());
        push(
            &mut leaves,
            "lifecycle.publisher_loss",
            publisher_loss.into(),
        );
        push(&mut leaves, "controls.dropped", controls_dropped.into());
        push(
            &mut leaves,
            "players.list",
            snapshot
                .players
                .values()
                .map(|player| player.name.clone())
                .collect::<Vec<_>>()
                .into(),
        );
        push(
            &mut leaves,
            "players.scan_revision",
            snapshot.scan_revision.into(),
        );
        push(
            &mut leaves,
            "players.scan_complete",
            snapshot.scan_complete.into(),
        );
        push(
            &mut leaves,
            "active.name",
            snapshot
                .active
                .as_ref()
                .and_then(|key| snapshot.players.get(key))
                .map_or(PropValue::Null, |player| player.name.clone().into()),
        );
        push(
            &mut leaves,
            "active.key",
            snapshot
                .active
                .clone()
                .map_or(PropValue::Null, PropValue::from),
        );
        for player in snapshot.players.values() {
            push_player(&mut leaves, player, now_us);
        }
        Self { leaves }
    }
}

impl PropTree for MprisProps {
    fn snapshot(&self) -> PropValue {
        build_snapshot(self.leaves.clone())
    }

    fn list(&self) -> Vec<PropPath> {
        self.leaves.iter().map(|(path, _)| path.clone()).collect()
    }

    fn describe(&self, path: &PropPath) -> Option<PropDescribe> {
        if !self.leaves.iter().any(|(candidate, _)| candidate == path) {
            return None;
        }
        let leaf = path.as_str().rsplit('.').next()?;
        let mut description = match leaf {
            "props_level" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "SPEC-07 event conformance level.",
            ),
            "event_seq" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Monotonic event sequence for this daemon process.",
            ),
            "publisher_loss" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Cumulative publications lost during this daemon process.",
            ),
            "dropped" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Cumulative commands individually observed and dropped without reply because response capacity was exhausted or shutdown draining was in progress.",
            ),
            "list" => PropDescribe::leaf(
                path.clone(),
                PropType::List,
                "MPRIS well-known names, sorted by stable encoded key.",
            ),
            "scan_revision" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Daemon-session monotonic revision of the represented player scan.",
            ),
            "scan_complete" => PropDescribe::leaf(
                path.clone(),
                PropType::Bool,
                "Whether all players in scan_revision are resolved, unresponsive, or explicitly stale.",
            ),
            "key" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Stable bytewise encoding of the complete MPRIS well-known name.",
            ),
            "name" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "MPRIS well-known bus name; null when no player is active.",
            ),
            "identity" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Human-readable player identity from the MPRIS root interface.",
            ),
            "desktop_entry" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Desktop-entry basename advertised by the player.",
            ),
            "owner_epoch" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Daemon-session epoch for this well-known name's unique owner.",
            ),
            "unresponsive" => PropDescribe::leaf(
                path.clone(),
                PropType::Bool,
                "Whether the player scan could not complete a bounded D-Bus read.",
            ),
            "stale" => PropDescribe::leaf(
                path.clone(),
                PropType::Bool,
                "Whether the previous complete snapshot is retained after repeated scan races.",
            ),
            "playback_status" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Playback state: playing, paused, stopped, or unknown.",
            ),
            "title" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Current xesam:title metadata.",
            ),
            "artists" => PropDescribe::leaf(
                path.clone(),
                PropType::List,
                "Current xesam:artist metadata.",
            ),
            "album" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Current xesam:album metadata.",
            ),
            "length_us" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Current mpris:length in microseconds.",
            )
            .with_unit("microseconds"),
            "art_url" => PropDescribe::leaf(
                path.clone(),
                PropType::String,
                "Current mpris:artUrl metadata; not fetched by this daemon.",
            ),
            "computed_position_us" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Approximate read-time position computed from the last scan or Seeked observation; not polled.",
            )
            .with_unit("microseconds"),
            "position_observation_age_us" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "Monotonic age of the position basis used by computed_position_us.",
            )
            .with_unit("microseconds"),
            "rate" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "MPRIS playback rate used for position computation.",
            ),
            "volume" => PropDescribe::leaf(
                path.clone(),
                PropType::Number,
                "MPRIS volume where 1.0 is normal volume.",
            )
            .with_min(0.0),
            "can_play" | "can_pause" | "can_go_next" | "can_go_previous" | "can_seek"
            | "can_control" => PropDescribe::leaf(
                path.clone(),
                PropType::Bool,
                "MPRIS control capability advertised by the player.",
            ),
            _ => return None,
        };
        description.transient = matches!(
            leaf,
            "event_seq"
                | "publisher_loss"
                | "dropped"
                | "computed_position_us"
                | "position_observation_age_us"
        );
        Some(description)
    }
}

fn push_player(leaves: &mut Vec<(PropPath, PropValue)>, player: &PlayerSnapshot, now_us: u64) {
    let base = format!("players.by_id.{}", player.key);
    push(leaves, &format!("{base}.key"), player.key.clone().into());
    push(leaves, &format!("{base}.name"), player.name.clone().into());
    push(
        leaves,
        &format!("{base}.identity"),
        player.identity.clone().into(),
    );
    if let Some(value) = &player.desktop_entry {
        push(
            leaves,
            &format!("{base}.desktop_entry"),
            value.clone().into(),
        );
    }
    push(
        leaves,
        &format!("{base}.owner_epoch"),
        player.owner_epoch.into(),
    );
    push(
        leaves,
        &format!("{base}.unresponsive"),
        player.unresponsive.into(),
    );
    push(leaves, &format!("{base}.stale"), player.stale.into());
    push(
        leaves,
        &format!("{base}.playback_status"),
        player.playback_status.as_str().into(),
    );
    if let Some(value) = &player.metadata.title {
        push(leaves, &format!("{base}.title"), value.clone().into());
    }
    push(
        leaves,
        &format!("{base}.artists"),
        player.metadata.artists.clone().into(),
    );
    if let Some(value) = &player.metadata.album {
        push(leaves, &format!("{base}.album"), value.clone().into());
    }
    if let Some(value) = player.metadata.length_us {
        push(leaves, &format!("{base}.length_us"), value.into());
    }
    if let Some(value) = &player.metadata.art_url {
        push(leaves, &format!("{base}.art_url"), value.clone().into());
    }
    push(
        leaves,
        &format!("{base}.computed_position_us"),
        player.computed_position_us(now_us).into(),
    );
    push(
        leaves,
        &format!("{base}.position_observation_age_us"),
        now_us.saturating_sub(player.position_observed_at_us).into(),
    );
    push(leaves, &format!("{base}.rate"), player.rate.into());
    push(leaves, &format!("{base}.volume"), player.volume.into());
    for (leaf, value) in [
        ("can_play", player.can_play),
        ("can_pause", player.can_pause),
        ("can_go_next", player.can_go_next),
        ("can_go_previous", player.can_go_previous),
        ("can_seek", player.can_seek),
        ("can_control", player.can_control),
    ] {
        push(leaves, &format!("{base}.{leaf}"), value.into());
    }
}

fn push(leaves: &mut Vec<(PropPath, PropValue)>, path: &str, value: PropValue) {
    if let Ok(path) = PropPath::new(path) {
        leaves.push((path, value));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::Value;

    use super::*;
    use crate::core::{PlaybackStatus, PlayerMetadata, player_key};

    fn fixture() -> MprisSnapshot {
        let name = "org.mpris.MediaPlayer2.alpha";
        let key = player_key(name);
        let player = PlayerSnapshot {
            key: key.clone(),
            name: name.into(),
            owner: ":1.20".into(),
            owner_epoch: 4,
            unresponsive: false,
            stale: false,
            identity: "Alpha Player".into(),
            desktop_entry: Some("alpha".into()),
            playback_status: PlaybackStatus::Playing,
            metadata: PlayerMetadata {
                title: Some("Song".into()),
                artists: vec!["One".into(), "Two".into()],
                album: Some("Record".into()),
                length_us: Some(9_000_000),
                art_url: Some("file:///tmp/cover.png".into()),
            },
            position_us: 1_000_000,
            position_observed_at_us: 2_000_000,
            rate: 1.0,
            volume: 0.75,
            can_play: true,
            can_pause: true,
            can_go_next: true,
            can_go_previous: false,
            can_seek: true,
            can_control: true,
        };
        MprisSnapshot {
            players: BTreeMap::from([(key.clone(), player)]),
            active: Some(key),
            scan_revision: 7,
            scan_complete: true,
        }
    }

    #[test]
    fn fixture_projects_metadata_capabilities_and_computed_position() {
        let props = MprisProps::new(&fixture(), 9, 3, 4, 3_000_000);
        let snapshot: Value = (&props.snapshot()).into();
        let key = player_key("org.mpris.MediaPlayer2.alpha");
        assert_eq!(
            snapshot["players"]["list"][0],
            "org.mpris.MediaPlayer2.alpha"
        );
        assert_eq!(snapshot["active"]["name"], "org.mpris.MediaPlayer2.alpha");
        assert_eq!(snapshot["players"]["by_id"][&key]["title"], "Song");
        assert_eq!(
            snapshot["players"]["by_id"][&key]["artists"],
            serde_json::json!(["One", "Two"])
        );
        assert_eq!(
            snapshot["players"]["by_id"][&key]["computed_position_us"],
            2_000_000
        );
        assert_eq!(snapshot["lifecycle"]["publisher_loss"], 3);
        assert_eq!(snapshot["controls"]["dropped"], 4);
        assert_eq!(snapshot["players"]["scan_revision"], 7);
        assert_eq!(snapshot["players"]["scan_complete"], true);
    }

    #[test]
    fn no_player_is_an_empty_list_and_null_active() {
        let props = MprisProps::new(&MprisSnapshot::default(), 0, 0, 0, 0);
        let snapshot: Value = (&props.snapshot()).into();
        assert_eq!(snapshot["players"]["list"], serde_json::json!([]));
        assert!(snapshot["active"]["name"].is_null());
        assert!(snapshot["active"]["key"].is_null());
    }

    #[test]
    fn computed_position_is_transient_but_metadata_is_event_bearing() {
        let props = MprisProps::new(&fixture(), 9, 2, 1, 3_000_000);
        let key = player_key("org.mpris.MediaPlayer2.alpha");
        let position = PropPath::new(format!("players.by_id.{key}.computed_position_us")).unwrap();
        let title = PropPath::new(format!("players.by_id.{key}.title")).unwrap();
        let dropped = PropPath::new("controls.dropped").unwrap();
        assert!(props.describe(&position).unwrap().transient);
        assert!(props.describe(&dropped).unwrap().transient);
        assert!(!props.describe(&title).unwrap().transient);
    }
}
