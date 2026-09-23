//! Name-addressed page registry for one edge panel carousel.
//!
//! A carousel holds one edge's ordered sub-panel names. Order comes from the
//! declared list (position 0 is the primary, the page shown by default);
//! names registered without a declaration append to the tail in registration
//! order. A declared name whose content is not registered yet is an empty
//! slot: paging and selection skip it, so the carousel never rests on one.
//!
//! Identity is the name, never the position — registering and removing shift
//! positions freely. The remembered "last selected" name is what a default
//! reveal shows; it falls back to the primary when the remembered page is
//! removed.

use std::collections::HashSet;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::sync::Arc;

/// Ordered carousel slots and the current selection.
///
/// Slots hold the declared list first (in declared order, possibly with
/// empty slots) followed by tail registrations. Selection and paging only
/// rest on registered slots; the cached registered names in slot order let
/// frames share the page schema without cloning its strings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Carousel {
    slots: Vec<Slot>,
    /// Leading `slots` count that came from the declared list.
    declared_len: usize,
    /// Currently shown slot; always registered when set.
    active: Option<usize>,
    /// Registered names in slot order, shared with rendered frames.
    pages: Arc<[String]>,
    /// Page a default reveal shows; `None` until one is selected.
    last_selected: Option<String>,
    /// Saved selection waiting for content; a successful explicit selection cancels it.
    pending_restore: Option<String>,
}

/// One ordered carousel position.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Slot {
    name: String,
    /// `false` for a declared name whose content is not registered.
    registered: bool,
}

impl Carousel {
    /// Build a live page set in the given order.
    ///
    /// Every name starts registered and selectable, with the first active:
    /// the pre-registry behaviour dynamic hosts still use when rebuilding a
    /// carousel from mounted pages. These pages carry no declaration, so
    /// removing one deletes it outright.
    pub fn new(
        page_ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, CarouselError> {
        let slots = validated_slots(page_ids, true)?;
        let mut carousel = Self {
            active: (!slots.is_empty()).then_some(0),
            slots,
            declared_len: 0,
            pages: Arc::from([]),
            last_selected: None,
            pending_restore: None,
        };
        carousel.rebuild_pages();
        Ok(carousel)
    }

    /// Build from the declared ordered list with every slot empty.
    ///
    /// Position 0 is the primary — the page a default reveal shows once its
    /// content registers. Nothing is selectable until `register` fills a
    /// declared slot or appends a tail name; an edge with no declarations
    /// and no registrations is empty.
    pub fn declared(
        page_ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, CarouselError> {
        let slots = validated_slots(page_ids, false)?;
        let declared_len = slots.len();
        Ok(Self {
            slots,
            declared_len,
            active: None,
            pages: Arc::from([]),
            last_selected: None,
            pending_restore: None,
        })
    }

    pub fn empty() -> Self {
        Self::default()
    }

    /// Reconcile config order without discarding live pages or selection.
    /// Live names omitted from config follow the declarations in their previous
    /// relative order; obsolete empty declarations disappear.
    pub fn redeclare(
        &mut self,
        page_ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<(), CarouselError> {
        let mut slots = validated_slots(page_ids, false)?;
        let declared_len = slots.len();
        let active = self.active_id().map(str::to_owned);
        let declared: HashSet<String> = slots.iter().map(|slot| slot.name.clone()).collect();
        for slot in &mut slots {
            slot.registered = self.registered_slot(&slot.name).is_some();
        }
        slots.extend(
            self.slots
                .iter()
                .filter(|slot| slot.registered && !declared.contains(&slot.name))
                .cloned(),
        );
        self.slots = slots;
        self.declared_len = declared_len;
        self.active = active
            .as_deref()
            .and_then(|name| self.registered_slot(name));
        self.rebuild_pages();
        Ok(())
    }

    /// Registered page IDs in slot order; empty slots are absent.
    pub fn page_ids(&self) -> &[String] {
        &self.pages
    }

    /// Clone the shared page schema without cloning its strings.
    pub fn shared_page_ids(&self) -> Arc<[String]> {
        Arc::clone(&self.pages)
    }

    pub fn active_index(&self) -> Option<usize> {
        let slot = self.resting_slot()?;
        Some(
            self.slots[..slot]
                .iter()
                .filter(|page| page.registered)
                .count(),
        )
    }

    pub fn active_id(&self) -> Option<&str> {
        self.resting_slot()
            .map(|slot| self.slots[slot].name.as_str())
    }

    /// The remembered "last selected" name; `None` means a default reveal
    /// shows the primary, skipping empty slots if it has no content.
    pub fn last_selected(&self) -> Option<&str> {
        self.last_selected.as_deref()
    }

    /// Restore a saved name now, or when its content first registers.
    /// Until then the carousel rests on live content. Explicit selection wins
    /// over this deferred restore, including selection of the current page.
    pub fn restore_saved_selection(&mut self, name: &str) {
        self.pending_restore = None;
        if !self.select_id(name) && !name.trim().is_empty() {
            self.pending_restore = Some(name.to_owned());
        }
    }

    /// Restore default-reveal selection without rewriting selection memory.
    pub(super) fn restore_selection(&mut self) {
        self.active = self
            .last_selected
            .as_deref()
            .and_then(|name| self.registered_slot(name))
            .or_else(|| self.slots.iter().position(|slot| slot.registered));
    }

    pub fn next_page(&mut self) -> Option<&str> {
        let current = self.resting_slot()?;
        let next = self.slots[current + 1..]
            .iter()
            .position(|page| page.registered)
            .map(|offset| offset + current + 1)
            .or_else(|| {
                self.slots[..current]
                    .iter()
                    .position(|page| page.registered)
            })
            .unwrap_or(current);
        self.select_slot(next);
        self.active_id()
    }

    pub fn previous_page(&mut self) -> Option<&str> {
        let current = self.resting_slot()?;
        let previous = self.slots[..current]
            .iter()
            .rposition(|page| page.registered)
            .or_else(|| {
                self.slots[current + 1..]
                    .iter()
                    .rposition(|page| page.registered)
                    .map(|offset| offset + current + 1)
            })
            .unwrap_or(current);
        self.select_slot(previous);
        self.active_id()
    }

    /// Select by position among the registered pages (see [`Self::page_ids`]).
    pub fn select_index(&mut self, index: usize) -> bool {
        let Some(slot) = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, page)| page.registered)
            .nth(index)
            .map(|(slot, _)| slot)
        else {
            return false;
        };
        self.select_slot(slot);
        true
    }

    pub fn select_id(&mut self, id: &str) -> bool {
        let Some(slot) = self.registered_slot(id) else {
            return false;
        };
        self.select_slot(slot);
        true
    }

    /// Register content for `name`.
    ///
    /// A declared name fills its slot in declared order; any other name
    /// appends to the tail in registration order. Selection stays put except
    /// when this name fulfils a pending saved-state restore. The name must be
    /// non-empty and not already live.
    pub fn register(&mut self, name: &str) -> Result<(), CarouselError> {
        if name.trim().is_empty() {
            return Err(CarouselError::EmptyId);
        }
        let showing = self.resting_slot();
        match self.slots.iter_mut().find(|page| page.name == name) {
            Some(page) if page.registered => {
                return Err(CarouselError::DuplicateId(name.to_owned()));
            }
            Some(page) => page.registered = true,
            None => self.slots.push(Slot {
                name: name.to_owned(),
                registered: true,
            }),
        }
        // Filling a preceding empty slot must not move the default-shown page.
        // Registration only appends or fills slots, so this index stays valid.
        self.active = showing.or_else(|| self.registered_slot(name));
        if self.pending_restore.as_deref() == Some(name) {
            self.select_id(name);
        }
        self.rebuild_pages();
        Ok(())
    }

    /// Activate a registered name: select it and remember it as the edge's
    /// last selection.
    ///
    /// Activating a name without registered content — unknown, or a declared
    /// empty slot — is an error and never a creation.
    pub fn activate(&mut self, name: &str) -> Result<(), CarouselError> {
        if name.trim().is_empty() {
            return Err(CarouselError::EmptyId);
        }
        let Some(slot) = self.registered_slot(name) else {
            return Err(CarouselError::Unregistered(name.to_owned()));
        };
        self.select_slot(slot);
        Ok(())
    }

    /// Remove a registered name's content.
    ///
    /// Removing the page being shown lands the selection on the previous
    /// registered neighbour, else the next, else the primary; removing the
    /// remembered last selection falls that memory back to the primary when
    /// the primary still has content, else clears it. A declared name keeps
    /// its now-empty slot for re-registration; a tail name is deleted.
    pub fn remove(&mut self, name: &str) -> Result<(), CarouselError> {
        if name.trim().is_empty() {
            return Err(CarouselError::EmptyId);
        }
        let Some(slot) = self.registered_slot(name) else {
            return Err(CarouselError::Unregistered(name.to_owned()));
        };
        let showing = self.resting_slot() == Some(slot);
        let remembered = self.last_selected.as_deref() == Some(name);
        // Decide the landing before the slot empties or later slots shift.
        let landing = self.landing_slot(slot);
        let tail = slot >= self.declared_len;
        if tail {
            self.slots.remove(slot);
        } else {
            self.slots[slot].registered = false;
        }
        let shift = |index: usize| index - usize::from(tail && index > slot);
        if showing {
            self.active = landing.map(shift);
        } else if let Some(active) = self.active {
            self.active = Some(shift(active));
        }
        if remembered {
            self.last_selected = match self.slots.first() {
                Some(primary) if primary.registered => Some(primary.name.clone()),
                _ => None,
            };
        }
        self.rebuild_pages();
        Ok(())
    }

    /// The slot the carousel rests on, or the first live slot if unset.
    fn resting_slot(&self) -> Option<usize> {
        self.active
            .filter(|&slot| self.slots.get(slot).is_some_and(|page| page.registered))
            .or_else(|| self.slots.iter().position(|page| page.registered))
    }

    /// Position of `name` when its content is registered.
    fn registered_slot(&self, name: &str) -> Option<usize> {
        self.slots
            .iter()
            .position(|page| page.registered && page.name == name)
    }

    /// Previous registered neighbour, else the next. Scanning back reaches
    /// the primary (position 0) last, which is the landing rule's final
    /// fallback.
    fn landing_slot(&self, removed: usize) -> Option<usize> {
        self.slots[..removed]
            .iter()
            .rposition(|page| page.registered)
            .or_else(|| {
                self.slots[removed + 1..]
                    .iter()
                    .position(|page| page.registered)
                    .map(|offset| offset + removed + 1)
            })
    }

    fn select_slot(&mut self, slot: usize) {
        self.pending_restore = None;
        self.active = Some(slot);
        self.last_selected = Some(self.slots[slot].name.clone());
    }

    fn rebuild_pages(&mut self) {
        self.pages = Arc::from(
            self.slots
                .iter()
                .filter(|page| page.registered)
                .map(|page| page.name.clone())
                .collect::<Vec<_>>(),
        );
    }
}

/// Validate an ordered name list and materialise its slots.
fn validated_slots(
    page_ids: impl IntoIterator<Item = impl Into<String>>,
    registered: bool,
) -> Result<Vec<Slot>, CarouselError> {
    let names: Vec<String> = page_ids.into_iter().map(Into::into).collect();
    let mut seen = HashSet::with_capacity(names.len());
    for name in &names {
        if name.trim().is_empty() {
            return Err(CarouselError::EmptyId);
        }
        if !seen.insert(name.clone()) {
            return Err(CarouselError::DuplicateId(name.clone()));
        }
    }
    Ok(names
        .into_iter()
        .map(|name| Slot { name, registered })
        .collect())
}

/// Invalid stable page IDs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CarouselError {
    EmptyId,
    DuplicateId(String),
    /// Activate or remove targeted a name without registered content.
    Unregistered(String),
}

impl Display for CarouselError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyId => formatter.write_str("carousel page ID must not be empty"),
            Self::DuplicateId(id) => write!(formatter, "duplicate carousel page ID '{id}'"),
            Self::Unregistered(id) => write!(formatter, "carousel page '{id}' is not registered"),
        }
    }
}

impl Error for CarouselError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registering_earlier_slots_preserves_shown_page() {
        let mut carousel = Carousel::declared(["alpha", "beta", "gamma"]).unwrap();
        carousel.register("gamma").unwrap();
        assert_eq!(carousel.active_id(), Some("gamma"));
        carousel.register("beta").unwrap();
        assert_eq!(carousel.active_id(), Some("gamma"));
        assert_eq!(carousel.active_index(), Some(1));
        carousel.register("alpha").unwrap();
        assert_eq!(carousel.active_id(), Some("gamma"));
        assert_eq!(carousel.active_index(), Some(2));
        assert_eq!(carousel.last_selected(), None);
        carousel.restore_selection();
        assert_eq!(carousel.active_id(), Some("alpha"));
    }

    #[test]
    fn all_empty_paging_has_no_selection() {
        for mut carousel in [
            Carousel::empty(),
            Carousel::declared(["alpha", "beta"]).unwrap(),
        ] {
            assert_eq!(carousel.next_page(), None);
            assert_eq!(carousel.previous_page(), None);
            assert!(!carousel.select_index(0));
            assert!(!carousel.select_id("alpha"));
            assert_eq!(carousel.active_index(), None);
            assert_eq!(carousel.last_selected(), None);
            carousel.restore_selection();
            assert_eq!(carousel.active_id(), None);
        }
    }

    #[test]
    fn single_live_page_wraps_across_empty_slots() {
        for name in ["alpha", "beta", "gamma"] {
            let mut carousel = Carousel::declared(["alpha", "beta", "gamma"]).unwrap();
            carousel.register(name).unwrap();
            assert_eq!(carousel.next_page(), Some(name));
            assert_eq!(carousel.previous_page(), Some(name));
            assert_eq!(carousel.active_index(), Some(0));
            assert_eq!(carousel.last_selected(), Some(name));
            assert!(carousel.select_index(0));
            assert!(!carousel.select_index(1));
        }
    }

    #[test]
    fn removing_sole_page_clears_selection_and_allows_reregistration() {
        for mut carousel in [Carousel::empty(), Carousel::declared(["only"]).unwrap()] {
            carousel.register("only").unwrap();
            carousel.activate("only").unwrap();
            carousel.remove("only").unwrap();
            assert!(carousel.page_ids().is_empty());
            assert_eq!(carousel.active_id(), None);
            assert_eq!(carousel.active_index(), None);
            assert_eq!(carousel.last_selected(), None);
            assert_eq!(carousel.next_page(), None);
            assert_eq!(carousel.previous_page(), None);
            assert_eq!(
                carousel.activate("only"),
                Err(CarouselError::Unregistered("only".into()))
            );
            carousel.register("only").unwrap();
            assert_eq!(carousel.active_id(), Some("only"));
            assert_eq!(carousel.active_index(), Some(0));
        }
    }

    #[test]
    fn removing_shown_page_preserves_distinct_remembered_page() {
        let mut carousel = Carousel::new(["alpha", "beta", "gamma"]).unwrap();
        carousel.activate("gamma").unwrap();
        carousel.remove("gamma").unwrap();
        assert_eq!(carousel.active_id(), Some("beta"));
        assert_eq!(carousel.last_selected(), Some("alpha"));
        carousel.remove("beta").unwrap();
        assert_eq!(carousel.active_id(), Some("alpha"));
        assert_eq!(carousel.last_selected(), Some("alpha"));
    }

    #[test]
    fn register_appends_to_tail_in_registration_order() {
        let mut carousel = Carousel::declared(["alpha", "beta"]).unwrap();
        carousel.register("tail-one").unwrap();
        carousel.register("tail-two").unwrap();
        // Filling a declared slot keeps its declared position.
        carousel.register("beta").unwrap();
        carousel.register("tail-three").unwrap();
        assert_eq!(
            carousel.page_ids(),
            ["beta", "tail-one", "tail-two", "tail-three"]
        );
    }

    #[test]
    fn undeclared_slots_are_skipped_by_paging() {
        let mut carousel = Carousel::declared(["alpha", "beta", "gamma"]).unwrap();
        carousel.register("alpha").unwrap();
        carousel.register("gamma").unwrap();
        assert_eq!(carousel.page_ids(), ["alpha", "gamma"]);
        assert_eq!(carousel.active_id(), Some("alpha"));
        assert_eq!(carousel.next_page(), Some("gamma"));
        assert_eq!(carousel.next_page(), Some("alpha"));
        assert_eq!(carousel.previous_page(), Some("gamma"));
        assert_eq!(carousel.active_index(), Some(1));
        assert!(!carousel.select_id("beta"));
        assert!(carousel.select_index(1));
        assert!(!carousel.select_index(2));
        assert_eq!(carousel.active_id(), Some("gamma"));
    }

    #[test]
    fn remove_lands_on_previous_else_next_else_primary() {
        // Previous neighbour.
        let mut carousel = Carousel::declared(["alpha", "beta", "gamma"]).unwrap();
        for name in ["alpha", "beta", "gamma"] {
            carousel.register(name).unwrap();
        }
        carousel.activate("gamma").unwrap();
        carousel.remove("gamma").unwrap();
        assert_eq!(carousel.active_id(), Some("beta"));

        // Next when nothing registered precedes it; the declared slot stays.
        let mut carousel = Carousel::declared(["alpha", "beta", "gamma"]).unwrap();
        for name in ["alpha", "beta", "gamma"] {
            carousel.register(name).unwrap();
        }
        carousel.activate("alpha").unwrap();
        carousel.remove("alpha").unwrap();
        assert_eq!(carousel.active_id(), Some("beta"));
        assert_eq!(carousel.page_ids(), ["beta", "gamma"]);

        // The primary when both neighbours are empty slots.
        let mut carousel = Carousel::declared(["alpha", "beta", "gamma"]).unwrap();
        carousel.register("alpha").unwrap();
        carousel.register("gamma").unwrap();
        carousel.activate("gamma").unwrap();
        carousel.remove("gamma").unwrap();
        assert_eq!(carousel.active_id(), Some("alpha"));

        // Next when the default-shown page (never explicitly selected) goes.
        let mut carousel = Carousel::declared(["alpha", "beta", "gamma"]).unwrap();
        carousel.register("beta").unwrap();
        carousel.register("gamma").unwrap();
        assert_eq!(carousel.active_id(), Some("beta"));
        carousel.remove("beta").unwrap();
        assert_eq!(carousel.active_id(), Some("gamma"));

        // Tail names are deleted outright and later positions shift.
        let mut carousel = Carousel::declared(["alpha"]).unwrap();
        carousel.register("alpha").unwrap();
        carousel.register("tail-one").unwrap();
        carousel.register("tail-two").unwrap();
        carousel.activate("tail-two").unwrap();
        carousel.remove("tail-one").unwrap();
        assert_eq!(carousel.page_ids(), ["alpha", "tail-two"]);
        assert_eq!(carousel.active_id(), Some("tail-two"));

        // Unknown and still-empty names are refused.
        let mut carousel = Carousel::declared(["alpha", "beta"]).unwrap();
        carousel.register("beta").unwrap();
        assert_eq!(
            carousel.remove("alpha"),
            Err(CarouselError::Unregistered("alpha".into()))
        );
        assert_eq!(
            carousel.remove("missing"),
            Err(CarouselError::Unregistered("missing".into()))
        );
    }

    #[test]
    fn remove_falls_back_last_selected_to_primary() {
        let mut carousel = Carousel::declared(["alpha", "beta", "gamma"]).unwrap();
        for name in ["alpha", "beta", "gamma"] {
            carousel.register(name).unwrap();
        }
        carousel.activate("gamma").unwrap();
        assert_eq!(carousel.last_selected(), Some("gamma"));
        carousel.remove("gamma").unwrap();
        assert_eq!(carousel.last_selected(), Some("alpha"));
        // Landing and memory are separate rules: the view moved to the
        // previous neighbour while the memory fell back to the primary.
        assert_eq!(carousel.active_id(), Some("beta"));

        // Removing some other page leaves the memory alone.
        let mut carousel = Carousel::declared(["alpha", "beta", "gamma"]).unwrap();
        for name in ["alpha", "beta", "gamma"] {
            carousel.register(name).unwrap();
        }
        carousel.activate("beta").unwrap();
        carousel.remove("gamma").unwrap();
        assert_eq!(carousel.last_selected(), Some("beta"));
        assert_eq!(carousel.active_id(), Some("beta"));

        // Removing the remembered primary itself clears the memory; a default
        // reveal skips its empty slot.
        let mut carousel = Carousel::declared(["alpha", "beta"]).unwrap();
        carousel.register("alpha").unwrap();
        carousel.register("beta").unwrap();
        carousel.activate("alpha").unwrap();
        carousel.remove("alpha").unwrap();
        assert_eq!(carousel.last_selected(), None);
        assert_eq!(carousel.active_id(), Some("beta"));
    }

    #[test]
    fn pending_restore_survives_redeclare_and_reveal_then_uses_removal_memory() {
        let mut carousel = Carousel::new(["primary", "other"]).unwrap();
        carousel.restore_saved_selection("scene-panel");
        carousel.redeclare(["primary", "scene-panel", "other"]).unwrap();
        carousel.restore_selection();
        assert_eq!(carousel.active_id(), Some("primary"));
        carousel.register("scene-panel").unwrap();
        assert_eq!(carousel.active_id(), Some("scene-panel"));
        assert_eq!(carousel.last_selected(), Some("scene-panel"));
        carousel.remove("scene-panel").unwrap();
        assert_eq!(carousel.last_selected(), Some("primary"));
        carousel.register("scene-panel").unwrap();
        carousel.restore_selection();
        assert_eq!(carousel.active_id(), Some("primary"));
    }

    #[test]
    fn activation_and_index_selection_cancel_pending_restore() {
        for by_index in [false, true] {
            let mut carousel = Carousel::new(["primary", "other"]).unwrap();
            carousel.restore_saved_selection("scene-panel");
            if by_index {
                assert!(carousel.select_index(0));
            } else {
                carousel.activate("other").unwrap();
            }
            let selected = carousel.active_id().unwrap().to_owned();
            carousel.register("scene-panel").unwrap();
            carousel.restore_selection();
            assert_eq!(carousel.active_id(), Some(selected.as_str()));
            assert_eq!(carousel.last_selected(), Some(selected.as_str()));
        }
    }

    #[test]
    fn activate_unregistered_is_an_error() {
        let mut carousel = Carousel::declared(["alpha", "beta"]).unwrap();
        carousel.register("alpha").unwrap();
        assert_eq!(
            carousel.activate("beta"),
            Err(CarouselError::Unregistered("beta".into()))
        );
        assert_eq!(
            carousel.activate("missing"),
            Err(CarouselError::Unregistered("missing".into()))
        );
        assert_eq!(carousel.activate(""), Err(CarouselError::EmptyId));
        assert_eq!(carousel.active_id(), Some("alpha"));
        carousel.activate("alpha").unwrap();
        assert_eq!(carousel.active_id(), Some("alpha"));
        assert_eq!(carousel.last_selected(), Some("alpha"));

        // Live page sets built by `Carousel::new` stay activatable.
        let mut live = Carousel::new(["nav"]).unwrap();
        live.activate("nav").unwrap();
        assert_eq!(live.active_id(), Some("nav"));
    }

    #[test]
    fn duplicate_name_within_model_is_rejected() {
        let mut carousel = Carousel::declared(["alpha"]).unwrap();
        carousel.register("alpha").unwrap();
        assert_eq!(
            carousel.register("alpha"),
            Err(CarouselError::DuplicateId("alpha".into()))
        );
        carousel.register("tail").unwrap();
        assert_eq!(
            carousel.register("tail"),
            Err(CarouselError::DuplicateId("tail".into()))
        );
        assert_eq!(carousel.register(""), Err(CarouselError::EmptyId));

        // Names already live through `Carousel::new` are duplicates too.
        let mut live = Carousel::new(["nav", "places"]).unwrap();
        assert_eq!(
            live.register("places"),
            Err(CarouselError::DuplicateId("places".into()))
        );
    }
}
