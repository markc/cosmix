//! Pure MPRIS player state, active-player selection, and snapshot differencing.
//!
//! This module has no async runtime, D-Bus, Bus, mesh, or filesystem dependency.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
    #[default]
    Unknown,
}

impl PlaybackStatus {
    pub fn from_mpris(value: &str) -> Self {
        match value {
            "Playing" => Self::Playing,
            "Paused" => Self::Paused,
            "Stopped" => Self::Stopped,
            _ => Self::Unknown,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlayerMetadata {
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub length_us: Option<i64>,
    pub art_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerSnapshot {
    /// Stable bytewise encoding of the complete MPRIS well-known name.
    pub key: String,
    pub name: String,
    /// Captured unique D-Bus owner used for scans and control dispatch.
    pub owner: String,
    /// Fresh, never-reused epoch minted when this name changes owner.
    pub owner_epoch: u64,
    /// The player exceeded a bounded D-Bus request deadline during its scan.
    pub unresponsive: bool,
    /// The previous complete snapshot is being retained after repeated races.
    pub stale: bool,
    pub identity: String,
    pub desktop_entry: Option<String>,
    pub playback_status: PlaybackStatus,
    pub metadata: PlayerMetadata,
    pub position_us: i64,
    pub position_observed_at_us: u64,
    pub rate: f64,
    pub volume: f64,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_seek: bool,
    pub can_control: bool,
}

impl PlayerSnapshot {
    /// MPRIS does not signal ordinary Position changes. This is an approximate
    /// read-time projection from the last scan or Seeked observation.
    pub fn computed_position_us(&self, now_us: u64) -> i64 {
        let mut value = self.position_us.max(0);
        if self.playback_status == PlaybackStatus::Playing && self.rate.is_finite() {
            let elapsed = now_us.saturating_sub(self.position_observed_at_us) as f64;
            let advanced = elapsed * self.rate;
            let delta = if advanced >= i64::MAX as f64 {
                i64::MAX
            } else if advanced <= i64::MIN as f64 {
                i64::MIN
            } else {
                advanced as i64
            };
            value = value.saturating_add(delta).max(0);
        }
        if let Some(length) = self.metadata.length_us.filter(|value| *value >= 0) {
            value = value.min(length);
        }
        value
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MprisSnapshot {
    pub players: BTreeMap<String, PlayerSnapshot>,
    pub active: Option<String>,
    /// Daemon-session monotonic identifier for the scan represented here.
    pub scan_revision: u64,
    /// False while per-player results from `scan_revision` are still merging.
    pub scan_complete: bool,
}

impl Default for MprisSnapshot {
    fn default() -> Self {
        Self {
            players: BTreeMap::new(),
            active: None,
            scan_revision: 0,
            scan_complete: true,
        }
    }
}

impl MprisSnapshot {
    pub fn diff(&self, next: &Self) -> Vec<MprisEvent> {
        let mut events = Vec::new();
        let keys: BTreeSet<&String> = self.players.keys().chain(next.players.keys()).collect();
        for key in keys {
            match (self.players.get(key), next.players.get(key)) {
                (None, Some(player)) => events.push(MprisEvent::PlayerAppeared {
                    player: player.clone(),
                }),
                (Some(player), None) => events.push(MprisEvent::PlayerVanished {
                    player: player.clone(),
                }),
                (Some(old), Some(new))
                    if old.owner != new.owner || old.owner_epoch != new.owner_epoch =>
                {
                    events.push(MprisEvent::PlayerVanished {
                        player: old.clone(),
                    });
                    events.push(MprisEvent::PlayerAppeared {
                        player: new.clone(),
                    });
                }
                _ => {}
            }
        }
        if self.active != next.active {
            events.push(MprisEvent::ActiveChanged {
                old: self.active.clone(),
                new: next.active.clone(),
            });
        }
        events
    }
}

/// A signal-observed transition into Playing, ordered by monotonic receive time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayingObservation {
    pub key: String,
    pub observed_at_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PlayerInstance {
    key: String,
    owner_epoch: u64,
}

impl PlayerInstance {
    fn new(key: &str, owner_epoch: u64) -> Self {
        Self {
            key: key.to_string(),
            owner_epoch,
        }
    }

    fn from_player(player: &PlayerSnapshot) -> Self {
        Self::new(&player.key, player.owner_epoch)
    }
}

/// Stateful, transport-neutral reducer for the most-recently-playing rule.
#[derive(Debug, Clone, Default)]
pub struct MediaModel {
    /// Latest published view. Its player map may contain mixed scan generations.
    snapshot: MprisSnapshot,
    /// Lifecycle, active selection and history are based only on this view.
    complete_snapshot: MprisSnapshot,
    complete_statuses: BTreeMap<PlayerInstance, PlaybackStatus>,
    has_observed_player: bool,
    playing_clock: u64,
    last_playing: BTreeMap<PlayerInstance, u64>,
    active_transitions: Vec<MprisEvent>,
}

impl MediaModel {
    pub fn snapshot(&self) -> &MprisSnapshot {
        &self.snapshot
    }

    pub fn complete_snapshot(&self) -> &MprisSnapshot {
        &self.complete_snapshot
    }

    pub fn lifecycle_events(&self, previous: &MprisSnapshot) -> Vec<MprisEvent> {
        let turnover_key = previous.active.as_ref().filter(|key| {
            previous.players.get(*key).is_some_and(|old| {
                self.complete_snapshot
                    .players
                    .get(*key)
                    .is_some_and(|new| old.owner_epoch != new.owner_epoch)
            })
        });
        let mut transitions = self.active_transitions.iter();
        let mut events = Vec::new();
        for event in previous
            .diff(&self.complete_snapshot)
            .into_iter()
            .filter(|event| !matches!(event, MprisEvent::ActiveChanged { .. }))
        {
            let is_turnover_vanish = matches!(
                &event,
                MprisEvent::PlayerVanished { player }
                    if turnover_key.is_some_and(|key| key == &player.key)
            );
            let is_turnover_appear = matches!(
                &event,
                MprisEvent::PlayerAppeared { player }
                    if turnover_key.is_some_and(|key| key == &player.key)
            );
            events.push(event);
            if is_turnover_vanish && let Some(transition) = transitions.next() {
                events.push(transition.clone());
            }
            if is_turnover_appear {
                events.extend(transitions.by_ref().cloned());
            }
        }
        events.extend(transitions.cloned());
        events
    }

    pub fn active_transitions(&self) -> &[MprisEvent] {
        &self.active_transitions
    }

    pub fn replace_players(&mut self, players: BTreeMap<String, PlayerSnapshot>) -> MprisSnapshot {
        let revision = self.snapshot.scan_revision.saturating_add(1);
        self.complete_scan(players, revision, &[])
    }

    pub fn replace_players_with_observations(
        &mut self,
        players: BTreeMap<String, PlayerSnapshot>,
        observations: &[PlayingObservation],
    ) -> MprisSnapshot {
        let revision = self.snapshot.scan_revision.saturating_add(1);
        self.complete_scan(players, revision, observations)
    }

    /// Commit one complete scan. This is the only operation that may update
    /// active selection, Playing history, or the lifecycle baseline.
    pub fn complete_scan(
        &mut self,
        players: BTreeMap<String, PlayerSnapshot>,
        scan_revision: u64,
        observations: &[PlayingObservation],
    ) -> MprisSnapshot {
        self.last_playing.retain(|instance, _| {
            players
                .get(&instance.key)
                .is_some_and(|player| player.owner_epoch == instance.owner_epoch)
        });
        let first_observation = !self.has_observed_player;
        let mut playing_transitions = Vec::<PlayerInstance>::new();
        let mut ordered_observations = observations
            .iter()
            .enumerate()
            .filter(|(_, observation)| players.contains_key(&observation.key))
            .collect::<Vec<_>>();
        ordered_observations
            .sort_by_key(|(index, observation)| (observation.observed_at_us, *index));
        let observed_keys = ordered_observations
            .iter()
            .map(|(_, observation)| observation.key.as_str())
            .collect::<BTreeSet<_>>();
        for (_, observation) in ordered_observations {
            let player = players
                .get(&observation.key)
                .expect("Playing observation was filtered to a current player");
            let instance = PlayerInstance::from_player(player);
            self.playing_clock = self.playing_clock.saturating_add(1);
            self.last_playing
                .insert(instance.clone(), self.playing_clock);
            playing_transitions.push(instance);
        }
        for (key, player) in &players {
            let instance = PlayerInstance::from_player(player);
            let was_playing = self
                .complete_statuses
                .get(&instance)
                .is_some_and(|status| *status == PlaybackStatus::Playing);
            if player.playback_status == PlaybackStatus::Playing
                && !was_playing
                && !observed_keys.contains(key.as_str())
            {
                if first_observation {
                    self.last_playing.insert(instance.clone(), 0);
                } else {
                    self.playing_clock = self.playing_clock.saturating_add(1);
                    self.last_playing
                        .insert(instance.clone(), self.playing_clock);
                }
                playing_transitions.push(instance);
            }
        }

        let old_active = self.complete_snapshot.active.clone();
        let turnover_active = old_active.as_ref().filter(|key| {
            let Some(previous) = self.complete_snapshot.players.get(*key) else {
                return false;
            };
            players
                .get(*key)
                .is_some_and(|current| previous.owner_epoch != current.owner_epoch)
        });
        let turnover_fallback = turnover_active.and_then(|turned_over| {
            self.last_playing
                .iter()
                .filter(|(instance, _)| instance.key != *turned_over)
                .max_by(|(left, left_clock), (right, right_clock)| {
                    left_clock
                        .cmp(right_clock)
                        .then_with(|| right.key.cmp(&left.key))
                })
                .map(|(instance, _)| instance.key.clone())
                .or_else(|| players.keys().find(|key| *key != turned_over).cloned())
        });
        let previous_active = self.complete_snapshot.active.as_ref().and_then(|key| {
            let previous = self.complete_snapshot.players.get(key)?;
            let current = players.get(key)?;
            (previous.owner_epoch == current.owner_epoch).then_some(key)
        });
        let active = playing_transitions
            .iter()
            .max_by(|left, right| {
                self.last_playing
                    .get(*left)
                    .copied()
                    .unwrap_or(0)
                    .cmp(&self.last_playing.get(*right).copied().unwrap_or(0))
                    .then_with(|| right.key.cmp(&left.key))
            })
            .map(|instance| instance.key.clone())
            .or_else(|| {
                previous_active
                    .and_then(|key| players.get(key).map(|player| (key, player)))
                    .filter(|(_, player)| {
                        matches!(
                            player.playback_status,
                            PlaybackStatus::Playing
                                | PlaybackStatus::Paused
                                | PlaybackStatus::Stopped
                        )
                    })
                    .map(|(key, _)| key.clone())
            })
            .or_else(|| {
                self.last_playing
                    .iter()
                    .max_by(|(left, left_clock), (right, right_clock)| {
                        left_clock
                            .cmp(right_clock)
                            .then_with(|| right.key.cmp(&left.key))
                    })
                    .map(|(instance, _)| instance.key.clone())
            })
            .or_else(|| players.keys().next().cloned());
        self.active_transitions.clear();
        if turnover_active.is_some() {
            self.active_transitions.push(MprisEvent::ActiveChanged {
                old: old_active,
                new: turnover_fallback.clone(),
            });
            if turnover_fallback != active {
                self.active_transitions.push(MprisEvent::ActiveChanged {
                    old: turnover_fallback,
                    new: active.clone(),
                });
            }
        } else if old_active != active {
            self.active_transitions.push(MprisEvent::ActiveChanged {
                old: old_active,
                new: active.clone(),
            });
        }
        self.snapshot = MprisSnapshot {
            players,
            active,
            scan_revision,
            scan_complete: true,
        };
        self.complete_statuses = self
            .snapshot
            .players
            .values()
            .map(|player| (PlayerInstance::from_player(player), player.playback_status))
            .collect();
        self.has_observed_player |= !self.snapshot.players.is_empty();
        self.complete_snapshot = self.snapshot.clone();
        self.snapshot.clone()
    }

    /// Mark a scan in flight without changing any canonical selection state.
    pub fn begin_scan(&mut self, scan_revision: u64) -> MprisSnapshot {
        self.snapshot.scan_revision = scan_revision;
        self.snapshot.scan_complete = false;
        self.snapshot.clone()
    }

    /// Publish player data from an in-progress scan without committing
    /// playing-transition history or changing active-player selection.
    pub fn replace_partial_players(
        &mut self,
        players: BTreeMap<String, PlayerSnapshot>,
    ) -> MprisSnapshot {
        self.replace_partial_scan(players, self.snapshot.scan_revision)
    }

    pub fn replace_partial_scan(
        &mut self,
        mut players: BTreeMap<String, PlayerSnapshot>,
        scan_revision: u64,
    ) -> MprisSnapshot {
        let active = self.complete_snapshot.active.clone();
        if let Some(key) = &active
            && !players.contains_key(key)
            && let Some(player) = self.complete_snapshot.players.get(key)
        {
            players.insert(key.clone(), player.clone());
        }
        self.snapshot = MprisSnapshot {
            players,
            active,
            scan_revision,
            scan_complete: false,
        };
        self.snapshot.clone()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MprisEvent {
    PlayerAppeared {
        player: PlayerSnapshot,
    },
    PlayerVanished {
        player: PlayerSnapshot,
    },
    ActiveChanged {
        old: Option<String>,
        new: Option<String>,
    },
}

/// Collision-free SPEC-07 segment for a complete well-known bus name.
pub fn player_key(name: &str) -> String {
    let mut key = String::with_capacity(2 + name.len() * 2);
    key.push_str("p_");
    for byte in name.bytes() {
        write!(&mut key, "{byte:02x}").expect("writing to String cannot fail");
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(name: &str, status: PlaybackStatus, title: &str) -> PlayerSnapshot {
        PlayerSnapshot {
            key: player_key(name),
            name: name.into(),
            owner: format!(":1.{}", name.len()),
            owner_epoch: 1,
            unresponsive: false,
            stale: false,
            identity: name.rsplit('.').next().unwrap_or(name).into(),
            desktop_entry: None,
            playback_status: status,
            metadata: PlayerMetadata {
                title: Some(title.into()),
                artists: vec!["Artist".into()],
                album: Some("Album".into()),
                length_us: Some(10_000_000),
                art_url: None,
            },
            position_us: 1_000_000,
            position_observed_at_us: 2_000_000,
            rate: 1.0,
            volume: 0.5,
            can_play: true,
            can_pause: true,
            can_go_next: true,
            can_go_previous: true,
            can_seek: true,
            can_control: true,
        }
    }

    #[test]
    fn two_players_and_one_vanishes_are_deterministic() {
        let first = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Paused, "A");
        let second = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Stopped, "B");
        let mut model = MediaModel::default();
        let both = model.replace_players(BTreeMap::from([
            (first.key.clone(), first.clone()),
            (second.key.clone(), second.clone()),
        ]));
        assert_eq!(both.players.len(), 2);
        assert_eq!(both.active, Some(first.key.clone()));

        let one = model.replace_players(BTreeMap::from([(second.key.clone(), second.clone())]));
        assert!(matches!(
            both.diff(&one).as_slice(),
            [MprisEvent::PlayerVanished { player }, MprisEvent::ActiveChanged { .. }]
                if player.name == first.name
        ));
        assert_eq!(one.active, Some(second.key));
    }

    #[test]
    fn active_stays_selected_when_it_pauses_while_an_older_player_keeps_playing() {
        let mut alpha = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Paused, "A");
        let mut beta = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Stopped, "B");
        let mut model = MediaModel::default();
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        alpha.playback_status = PlaybackStatus::Playing;
        assert_eq!(
            model
                .replace_players(BTreeMap::from([
                    (alpha.key.clone(), alpha.clone()),
                    (beta.key.clone(), beta.clone()),
                ]))
                .active,
            Some(alpha.key.clone())
        );
        beta.playback_status = PlaybackStatus::Playing;
        assert_eq!(
            model
                .replace_players(BTreeMap::from([
                    (alpha.key.clone(), alpha.clone()),
                    (beta.key.clone(), beta.clone()),
                ]))
                .active,
            Some(beta.key.clone())
        );
        beta.playback_status = PlaybackStatus::Paused;
        assert_eq!(
            model
                .replace_players(BTreeMap::from([
                    (alpha.key.clone(), alpha.clone()),
                    (beta.key.clone(), beta.clone()),
                ]))
                .active,
            Some(beta.key.clone())
        );
        alpha.playback_status = PlaybackStatus::Paused;
        assert_eq!(
            model
                .replace_players(BTreeMap::from([
                    (alpha.key.clone(), alpha.clone()),
                    (beta.key.clone(), beta.clone()),
                ]))
                .active,
            Some(beta.key)
        );
    }

    #[test]
    fn vanished_active_falls_back_to_most_recent_surviving_playing_history() {
        let alpha = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Stopped, "A");
        let mut beta = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Paused, "B");
        let mut gamma = player("org.mpris.MediaPlayer2.gamma", PlaybackStatus::Paused, "C");
        let mut model = MediaModel::default();
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
            (gamma.key.clone(), gamma.clone()),
        ]));
        beta.playback_status = PlaybackStatus::Playing;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
            (gamma.key.clone(), gamma.clone()),
        ]));
        beta.playback_status = PlaybackStatus::Paused;
        gamma.playback_status = PlaybackStatus::Playing;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
            (gamma.key.clone(), gamma.clone()),
        ]));

        let after_vanish = model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha),
            (beta.key.clone(), beta.clone()),
        ]));
        assert_eq!(after_vanish.active, Some(beta.key));
    }

    #[test]
    fn encoded_order_is_only_used_without_any_playing_history() {
        let alpha = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Stopped, "A");
        let mut beta = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Paused, "B");
        let mut model = MediaModel::default();
        assert_eq!(
            model
                .replace_players(BTreeMap::from([
                    (alpha.key.clone(), alpha.clone()),
                    (beta.key.clone(), beta.clone()),
                ]))
                .active,
            Some(alpha.key.clone())
        );
        beta.playback_status = PlaybackStatus::Playing;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        let after_beta_vanishes =
            model.replace_players(BTreeMap::from([(alpha.key.clone(), alpha.clone())]));
        assert_eq!(after_beta_vanishes.active, Some(alpha.key));
    }

    #[test]
    fn stopped_active_beta_is_not_replaced_by_earlier_alpha() {
        let alpha = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Paused, "A");
        let mut beta = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Paused, "B");
        let mut model = MediaModel::default();
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        beta.playback_status = PlaybackStatus::Playing;
        assert_eq!(
            model
                .replace_players(BTreeMap::from([
                    (alpha.key.clone(), alpha.clone()),
                    (beta.key.clone(), beta.clone()),
                ]))
                .active,
            Some(beta.key.clone())
        );
        beta.playback_status = PlaybackStatus::Stopped;
        assert_eq!(
            model
                .replace_players(BTreeMap::from([
                    (alpha.key.clone(), alpha),
                    (beta.key.clone(), beta.clone()),
                ]))
                .active,
            Some(beta.key)
        );
    }

    #[test]
    fn first_player_wins_when_multiple_are_already_playing_at_startup() {
        let alpha = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Playing, "A");
        let beta = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Playing, "B");
        let mut model = MediaModel::default();
        let snapshot = model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta),
        ]));
        assert_eq!(snapshot.active, Some(alpha.key));
    }

    #[test]
    fn partial_startup_publication_does_not_reorder_initial_playing_tie() {
        let alpha = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Playing, "A");
        let beta = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Playing, "B");
        let mut model = MediaModel::default();
        let partial =
            model.replace_partial_players(BTreeMap::from([(beta.key.clone(), beta.clone())]));
        assert_eq!(partial.active, None);
        let complete = model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta),
        ]));
        assert_eq!(complete.active, Some(alpha.key));
    }

    #[test]
    fn partial_publication_never_changes_an_established_active_player() {
        let alpha = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Paused, "A");
        let beta = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Paused, "B");
        let mut model = MediaModel::default();
        let complete = model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        assert_eq!(complete.active, Some(alpha.key.clone()));

        let partial = model.replace_partial_scan(
            BTreeMap::from([(beta.key.clone(), beta)]),
            complete.scan_revision + 1,
        );
        assert_eq!(partial.active, Some(alpha.key.clone()));
        assert!(partial.players.contains_key(&alpha.key));
        assert_eq!(model.complete_snapshot().active, Some(alpha.key));
        assert!(!partial.scan_complete);
    }

    #[test]
    fn playing_observed_between_paused_samples_advances_history() {
        let alpha = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Paused, "A");
        let beta = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Paused, "B");
        let mut model = MediaModel::default();
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));

        let next = model.replace_players_with_observations(
            BTreeMap::from([(alpha.key.clone(), alpha), (beta.key.clone(), beta.clone())]),
            &[PlayingObservation {
                key: beta.key.clone(),
                observed_at_us: 10,
            }],
        );
        assert_eq!(next.active, Some(beta.key.clone()));
        assert_eq!(
            next.players
                .values()
                .map(|player| player.playback_status)
                .collect::<Vec<_>>(),
            [PlaybackStatus::Paused, PlaybackStatus::Paused]
        );
    }

    #[test]
    fn playing_observations_are_ordered_by_signal_observation_time() {
        let alpha = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Paused, "A");
        let beta = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Paused, "B");
        let mut model = MediaModel::default();
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));

        let next = model.replace_players_with_observations(
            BTreeMap::from([
                (alpha.key.clone(), alpha.clone()),
                (beta.key.clone(), beta.clone()),
            ]),
            &[
                PlayingObservation {
                    key: alpha.key.clone(),
                    observed_at_us: 20,
                },
                PlayingObservation {
                    key: beta.key.clone(),
                    observed_at_us: 10,
                },
            ],
        );
        assert_eq!(next.active, Some(alpha.key.clone()));

        let next = model.replace_players_with_observations(
            BTreeMap::from([
                (alpha.key.clone(), alpha.clone()),
                (beta.key.clone(), beta.clone()),
            ]),
            &[
                PlayingObservation {
                    key: alpha.key,
                    observed_at_us: 30,
                },
                PlayingObservation {
                    key: beta.key.clone(),
                    observed_at_us: 40,
                },
            ],
        );
        assert_eq!(next.active, Some(beta.key.clone()));
    }

    #[test]
    fn metadata_change_is_visible_without_a_lifecycle_event() {
        let before_player = player(
            "org.mpris.MediaPlayer2.alpha",
            PlaybackStatus::Playing,
            "Before",
        );
        let mut after_player = before_player.clone();
        after_player.metadata.title = Some("After".into());
        let before = MprisSnapshot {
            players: BTreeMap::from([(before_player.key.clone(), before_player)]),
            active: Some(after_player.key.clone()),
            ..MprisSnapshot::default()
        };
        let after = MprisSnapshot {
            players: BTreeMap::from([(after_player.key.clone(), after_player)]),
            active: before.active.clone(),
            ..MprisSnapshot::default()
        };
        assert!(before.diff(&after).is_empty());
        assert_ne!(before, after);
    }

    #[test]
    fn owner_turnover_is_a_vanish_then_appear_edge() {
        let old = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Paused, "A");
        let mut new = old.clone();
        new.owner = ":1.99".into();
        new.owner_epoch = old.owner_epoch + 1;
        let before = MprisSnapshot {
            players: BTreeMap::from([(old.key.clone(), old.clone())]),
            active: Some(old.key.clone()),
            ..MprisSnapshot::default()
        };
        let after = MprisSnapshot {
            players: BTreeMap::from([(new.key.clone(), new.clone())]),
            active: Some(new.key.clone()),
            ..MprisSnapshot::default()
        };
        assert!(matches!(
            before.diff(&after).as_slice(),
            [
                MprisEvent::PlayerVanished { player: vanished },
                MprisEvent::PlayerAppeared { player: appeared },
            ] if vanished.owner == old.owner && appeared.owner == new.owner
        ));
    }

    #[test]
    fn active_playing_owner_turnover_falls_back_instead_of_inheriting_selection() {
        let mut alpha = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Paused, "A");
        let mut beta = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Paused, "B");
        let mut model = MediaModel::default();
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        beta.playback_status = PlaybackStatus::Playing;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        beta.playback_status = PlaybackStatus::Paused;
        alpha.playback_status = PlaybackStatus::Playing;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));

        let before = model.complete_snapshot().clone();
        alpha.owner = ":1.99".into();
        alpha.owner_epoch += 1;
        alpha.playback_status = PlaybackStatus::Paused;
        let next = model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));

        assert_eq!(next.active, Some(beta.key.clone()));
        assert!(matches!(
            model.lifecycle_events(&before).as_slice(),
            [
                MprisEvent::PlayerVanished { player: vanished },
                MprisEvent::ActiveChanged { old, new },
                MprisEvent::PlayerAppeared { player: appeared },
            ] if vanished.owner != appeared.owner
                && old.as_deref() == Some(alpha.key.as_str())
                && new.as_deref() == Some(beta.key.as_str())
        ));
    }

    #[test]
    fn active_paused_owner_turnover_falls_back_instead_of_inheriting_selection() {
        let mut alpha = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Paused, "A");
        let mut beta = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Paused, "B");
        let mut model = MediaModel::default();
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        beta.playback_status = PlaybackStatus::Playing;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        beta.playback_status = PlaybackStatus::Paused;
        alpha.playback_status = PlaybackStatus::Playing;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        alpha.playback_status = PlaybackStatus::Paused;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));

        let before = model.complete_snapshot().clone();
        alpha.owner = ":1.99".into();
        alpha.owner_epoch += 1;
        let next = model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));

        assert_eq!(next.active, Some(beta.key.clone()));
        assert!(matches!(
            model.lifecycle_events(&before).as_slice(),
            [
                MprisEvent::PlayerVanished { .. },
                MprisEvent::ActiveChanged { old, new },
                MprisEvent::PlayerAppeared { .. },
            ] if old.as_deref() == Some(alpha.key.as_str())
                && new.as_deref() == Some(beta.key.as_str())
        ));
    }

    #[test]
    fn active_turnover_to_playing_replacement_emits_fallback_then_fresh_transition() {
        let mut alpha = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Paused, "A");
        let mut beta = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Paused, "B");
        let mut model = MediaModel::default();
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        beta.playback_status = PlaybackStatus::Playing;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        beta.playback_status = PlaybackStatus::Paused;
        alpha.playback_status = PlaybackStatus::Playing;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));

        let before = model.complete_snapshot().clone();
        alpha.owner = ":1.99".into();
        alpha.owner_epoch += 1;
        let next = model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));

        assert_eq!(next.active, Some(alpha.key.clone()));
        assert!(matches!(
            model.lifecycle_events(&before).as_slice(),
            [
                MprisEvent::PlayerVanished { .. },
                MprisEvent::ActiveChanged { old: first_old, new: first_new },
                MprisEvent::PlayerAppeared { .. },
                MprisEvent::ActiveChanged { old: second_old, new: second_new },
            ] if first_old.as_deref() == Some(alpha.key.as_str())
                && first_new.as_deref() == Some(beta.key.as_str())
                && second_old.as_deref() == Some(beta.key.as_str())
                && second_new.as_deref() == Some(alpha.key.as_str())
        ));
    }

    #[test]
    fn new_playing_owner_is_a_fresh_transition_even_with_surviving_history() {
        let mut alpha = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Paused, "A");
        let mut beta = player("org.mpris.MediaPlayer2.beta", PlaybackStatus::Paused, "B");
        let mut model = MediaModel::default();
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        beta.playback_status = PlaybackStatus::Playing;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));
        beta.playback_status = PlaybackStatus::Paused;
        model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta.clone()),
        ]));

        alpha.owner = ":1.99".into();
        alpha.owner_epoch += 1;
        alpha.playback_status = PlaybackStatus::Playing;
        let next = model.replace_players(BTreeMap::from([
            (alpha.key.clone(), alpha.clone()),
            (beta.key.clone(), beta),
        ]));

        assert_eq!(next.active, Some(alpha.key));
    }

    #[test]
    fn computed_position_advances_only_while_playing_and_clamps_to_length() {
        let mut snapshot = player("org.mpris.MediaPlayer2.alpha", PlaybackStatus::Playing, "A");
        assert_eq!(snapshot.computed_position_us(3_500_000), 2_500_000);
        assert_eq!(snapshot.computed_position_us(30_000_000), 10_000_000);
        snapshot.playback_status = PlaybackStatus::Paused;
        assert_eq!(snapshot.computed_position_us(30_000_000), 1_000_000);
    }

    #[test]
    fn complete_name_encoding_is_stable_and_collision_free() {
        let first = player_key("org.mpris.MediaPlayer2.alpha.instance1");
        assert_eq!(first, player_key("org.mpris.MediaPlayer2.alpha.instance1"));
        assert_ne!(first, player_key("org.mpris.MediaPlayer2.alpha_instance1"));
        assert!(
            first
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
        );
    }
}
