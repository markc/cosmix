//! The one dialog seat of a Quoin host (scene-editor plan §4.3 Q2).
//!
//! A dialog is a centred overlay surface, not a carousel page: it has no
//! edge, no page id and no carousel slot, so it lives beside
//! [`SubPanelRegistry`](super::SubPanelRegistry) rather than in it. One host
//! has at most one dialog seat. Loading a dialog reserves the seat unmapped;
//! a different scene is refused ([`DialogSeatError::Busy`]) unless the load
//! pre-empts, which releases the incumbent and hands it back so the host can
//! tell its owner (the `reason:"preempted"` notice).
//!
//! Frozen in Stage S: the seat type and these three operations. The host
//! wiring (surface, chrome with ×, show/hide, mount path) is Stage Q2's.

use std::error::Error;
use std::fmt::{Display, Formatter};

use super::OutputKey;

/// The live dialog: which scene holds the seat, whose it is, and how big.
#[derive(Clone, Debug, PartialEq)]
pub struct DialogSeat {
    /// Scene document name; also its address in scene verbs.
    pub scene: String,
    /// Broker-attested owner, as for [`SubPanelSeat`](super::SubPanelSeat).
    pub owner: String,
    /// Quoin receipt sequence at acceptance.
    pub accepted_at: u64,
    /// Output the dialog maps on (the selected output when shown).
    pub output: OutputKey,
    /// Authored logical size, already validated to 240..=2048 by cosmix-scene.
    pub w: f32,
    pub h: f32,
    pub title: Option<String>,
    /// Frame chrome: title bar and ×. Defaults to true in the document.
    pub chrome: bool,
}

/// Refusals from the dialog seat.
#[derive(Clone, Debug, PartialEq)]
pub enum DialogSeatError {
    EmptyName,
    /// Another scene holds the seat; the load did not ask to pre-empt.
    Busy { scene: String, owner: String },
    /// Release named a scene that does not hold the seat.
    Unknown(String),
}

impl Display for DialogSeatError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => formatter.write_str("dialog scene name must not be empty"),
            Self::Busy { scene, owner } => {
                write!(formatter, "the dialog seat is held by {scene} ({owner})")
            }
            Self::Unknown(scene) => write!(formatter, "{scene} does not hold the dialog seat"),
        }
    }
}

impl Error for DialogSeatError {}

/// At most one dialog per host.
#[derive(Clone, Debug, Default)]
pub struct DialogSlot {
    seat: Option<DialogSeat>,
}

impl DialogSlot {
    pub fn seat(&self) -> Option<&DialogSeat> {
        self.seat.as_ref()
    }

    /// Reserve or update the seat. The same scene and owner may reload
    /// (size, title or chrome change); any other holder refuses `Busy`.
    pub fn register_dialog(&mut self, seat: DialogSeat) -> Result<(), DialogSeatError> {
        if seat.scene.trim().is_empty() {
            return Err(DialogSeatError::EmptyName);
        }
        if let Some(held) = &self.seat
            && (held.scene != seat.scene || held.owner != seat.owner)
        {
            return Err(DialogSeatError::Busy {
                scene: held.scene.clone(),
                owner: held.owner.clone(),
            });
        }
        self.seat = Some(seat);
        Ok(())
    }

    /// Free the seat held by `scene` (unload, or its owner disconnected).
    pub fn release_dialog(&mut self, scene: &str) -> Result<DialogSeat, DialogSeatError> {
        self.seat
            .take_if(|held| held.scene == scene)
            .ok_or_else(|| DialogSeatError::Unknown(scene.to_owned()))
    }

    /// Take the seat whoever holds it. Returns the displaced seat when a
    /// different scene or owner held it (the host unloads that scene and
    /// notifies its owner); `None` when the seat was free or already this
    /// scene's.
    pub fn preempt_dialog(
        &mut self,
        seat: DialogSeat,
    ) -> Result<Option<DialogSeat>, DialogSeatError> {
        if seat.scene.trim().is_empty() {
            return Err(DialogSeatError::EmptyName);
        }
        let displaced = self
            .seat
            .take()
            .filter(|held| held.scene != seat.scene || held.owner != seat.owner);
        self.seat = Some(seat);
        Ok(displaced)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(scene: &str, owner: &str) -> DialogSeat {
        DialogSeat {
            scene: scene.into(),
            owner: owner.into(),
            accepted_at: 1,
            output: OutputKey::new("DP-1").unwrap(),
            w: 880.0,
            h: 620.0,
            title: Some("Scene Editor".into()),
            chrome: true,
        }
    }

    #[test]
    fn one_seat_same_holder_updates_other_is_busy() {
        let mut slot = DialogSlot::default();
        slot.register_dialog(seat("editor", "scenes")).unwrap();
        let mut bigger = seat("editor", "scenes");
        bigger.w = 1000.0;
        slot.register_dialog(bigger).unwrap();
        assert_eq!(slot.seat().unwrap().w, 1000.0);
        assert_eq!(
            slot.register_dialog(seat("other", "someone")),
            Err(DialogSeatError::Busy { scene: "editor".into(), owner: "scenes".into() })
        );
        assert_eq!(
            slot.register_dialog(seat("editor", "someone")),
            Err(DialogSeatError::Busy { scene: "editor".into(), owner: "scenes".into() })
        );
        assert_eq!(slot.register_dialog(seat(" ", "scenes")), Err(DialogSeatError::EmptyName));
    }

    #[test]
    fn release_only_by_the_holding_scene() {
        let mut slot = DialogSlot::default();
        slot.register_dialog(seat("editor", "scenes")).unwrap();
        assert_eq!(slot.release_dialog("other"), Err(DialogSeatError::Unknown("other".into())));
        assert_eq!(slot.release_dialog("editor").unwrap().scene, "editor");
        assert!(slot.seat().is_none());
        assert_eq!(slot.release_dialog("editor"), Err(DialogSeatError::Unknown("editor".into())));
    }

    #[test]
    fn preempt_returns_only_a_displaced_holder() {
        let mut slot = DialogSlot::default();
        assert_eq!(slot.preempt_dialog(seat("editor", "scenes")), Ok(None));
        assert_eq!(slot.preempt_dialog(seat("editor", "scenes")), Ok(None));
        let displaced = slot.preempt_dialog(seat("other", "someone")).unwrap().unwrap();
        assert_eq!((displaced.scene.as_str(), displaced.owner.as_str()), ("editor", "scenes"));
        assert_eq!(slot.seat().unwrap().scene, "other");
        assert_eq!(slot.preempt_dialog(seat("", "x")), Err(DialogSeatError::EmptyName));
        assert_eq!(slot.seat().unwrap().scene, "other");
    }
}
