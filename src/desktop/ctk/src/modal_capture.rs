//! Shared modal-capture ownership for CTK services and application overlays.
//!
//! Capture entries are token-owned and layer-aware. Releasing an entry marks it
//! closing immediately but retains it through the rest of the frame, so the key
//! that closes the top modal cannot leak into a modal or application beneath it.

use bevy::prelude::*;

/// Opaque identity for one acquired modal-capture entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModalCaptureToken(u64);

/// Routing and presentation precedence for a capture owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModalCaptureLayer(pub i32);

/// Diagnostic and routing identity for a capture owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ModalCaptureOwner {
    pub kind: &'static str,
    pub entity: Option<Entity>,
}

#[derive(Clone, Copy, Debug)]
struct CaptureEntry {
    token: ModalCaptureToken,
    owner: ModalCaptureOwner,
    layer: ModalCaptureLayer,
    acquisition_serial: u64,
    closing: bool,
}

/// Process-local authority for modal capture and top-owner routing.
#[derive(Resource, Default)]
pub struct ModalCapture {
    next_token: u64,
    next_acquisition_serial: u64,
    entries: Vec<CaptureEntry>,
}

impl ModalCapture {
    /// Acquires capture for `owner` and returns the exact token required to
    /// release it. Within a layer, the most recently acquired entry is top.
    pub fn acquire(
        &mut self,
        owner: ModalCaptureOwner,
        layer: ModalCaptureLayer,
    ) -> ModalCaptureToken {
        let token = ModalCaptureToken(self.next_token);
        self.next_token = self
            .next_token
            .checked_add(1)
            .expect("modal capture token exhausted");
        let acquisition_serial = self.next_acquisition_serial;
        self.next_acquisition_serial = self
            .next_acquisition_serial
            .checked_add(1)
            .expect("modal capture acquisition serial exhausted");
        self.entries.push(CaptureEntry {
            token,
            owner,
            layer,
            acquisition_serial,
            closing: false,
        });
        token
    }

    /// Logically releases `token`, while retaining its routing precedence until
    /// [`Last`] so the input that closed it remains captured for the whole frame.
    /// Returns `false` for an unknown or already-released token.
    pub fn release_latched(&mut self, token: ModalCaptureToken) -> bool {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.token == token && !entry.closing)
        else {
            return false;
        };
        entry.closing = true;
        true
    }

    /// Whether any active or close-latched owner currently captures input.
    pub fn is_captured(&self) -> bool {
        !self.entries.is_empty()
    }

    /// Whether `token` is the owner to which modal input must currently route.
    pub fn is_top(&self, token: ModalCaptureToken) -> bool {
        self.top_entry().is_some_and(|entry| entry.token == token)
    }

    /// The owner to which modal input must currently route.
    pub fn top_owner(&self) -> Option<ModalCaptureOwner> {
        self.top_entry().map(|entry| entry.owner)
    }

    fn top_entry(&self) -> Option<&CaptureEntry> {
        self.entries
            .iter()
            .max_by_key(|entry| (entry.layer, entry.acquisition_serial))
    }

    fn clear_released_entries(&mut self) {
        self.entries.retain(|entry| !entry.closing);
    }
}

fn clear_released(mut capture: ResMut<ModalCapture>) {
    capture.clear_released_entries();
}

/// Modal-service work that must complete before application input gates.
///
/// This is not an umbrella for every system that can acquire
/// [`ModalCapture`]. A service may ingest new requests later in its own set
/// (for example, interaction ingestion after application action routing).
/// Applications must either publish such requests before their input
/// preflight or defer late producers so a modal never opens behind input that
/// already resolved.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModalCaptureSystems;

/// Installs the shared capture authority and close-frame latch cleanup.
pub struct ModalCapturePlugin;

impl Plugin for ModalCapturePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ModalCapture>()
            .add_systems(Last, clear_released);
    }
}

/// Installs the authority once for CTK services which can also be used alone.
pub(crate) fn ensure_modal_capture_plugin(app: &mut App) {
    if !app.is_plugin_added::<ModalCapturePlugin>() {
        app.add_plugins(ModalCapturePlugin);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOW: ModalCaptureLayer = ModalCaptureLayer(10);
    const HIGH: ModalCaptureLayer = ModalCaptureLayer(20);
    const REQUESTER: ModalCaptureLayer = ModalCaptureLayer(1_000);
    const INTERACTION: ModalCaptureLayer = ModalCaptureLayer(1_100);

    fn owner(kind: &'static str) -> ModalCaptureOwner {
        ModalCaptureOwner { kind, entity: None }
    }

    #[test]
    fn acquire_and_release_are_token_owned() {
        let mut capture = ModalCapture::default();
        let token = capture.acquire(owner("one"), LOW);

        assert!(capture.is_captured());
        assert!(capture.is_top(token));
        assert_eq!(capture.top_owner(), Some(owner("one")));
        assert!(capture.release_latched(token));
        assert!(!capture.release_latched(token));
    }

    #[test]
    fn released_top_stays_top_until_last_then_disappears() {
        let mut app = App::new();
        app.add_plugins(ModalCapturePlugin);
        let (under, token) = {
            let mut capture = app.world_mut().resource_mut::<ModalCapture>();
            let under = capture.acquire(owner("under"), LOW);
            let token = capture.acquire(owner("closing"), LOW);
            (under, token)
        };
        assert!(app
            .world_mut()
            .resource_mut::<ModalCapture>()
            .release_latched(token));
        assert!(app.world().resource::<ModalCapture>().is_top(token));
        assert!(!app.world().resource::<ModalCapture>().is_top(under));

        app.update();

        assert!(!app.world().resource::<ModalCapture>().is_top(token));
        assert!(app.world().resource::<ModalCapture>().is_top(under));
    }

    #[test]
    fn higher_layer_is_top_even_when_acquired_earlier() {
        let mut capture = ModalCapture::default();
        let high = capture.acquire(owner("high"), HIGH);
        let low = capture.acquire(owner("low"), LOW);

        assert!(capture.is_top(high));
        assert!(!capture.is_top(low));
        assert_eq!(capture.top_owner(), Some(owner("high")));
    }

    #[test]
    fn two_owners_report_only_the_top_token() {
        let mut capture = ModalCapture::default();
        let first = capture.acquire(owner("first"), LOW);
        let second = capture.acquire(owner("second"), LOW);

        assert!(!capture.is_top(first));
        assert!(capture.is_top(second));
        assert_eq!(capture.top_owner(), Some(owner("second")));
    }

    #[test]
    fn same_frame_release_and_reacquire_keeps_fresh_token_top() {
        let mut capture = ModalCapture::default();
        let closing = capture.acquire(owner("requester"), REQUESTER);
        assert!(capture.is_captured());
        assert!(capture.is_top(closing));

        assert!(capture.release_latched(closing));
        assert!(
            capture.is_captured(),
            "release must retain close-frame capture"
        );
        let fresh = capture.acquire(owner("requester"), REQUESTER);

        assert!(
            capture.is_captured(),
            "reacquire must not open a capture gap"
        );
        assert!(capture.is_top(fresh));
        assert!(!capture.is_top(closing));

        capture.clear_released_entries();

        assert!(capture.is_captured());
        assert!(capture.is_top(fresh));
        assert!(!capture.is_top(closing));
        assert_eq!(capture.entries.len(), 1, "only the fresh entry remains");
    }

    #[test]
    fn closing_entries_respect_layer_order_in_both_permutations() {
        let mut capture = ModalCapture::default();
        let interaction = capture.acquire(owner("interaction"), INTERACTION);
        let requester = capture.acquire(owner("requester"), REQUESTER);

        assert!(capture.release_latched(interaction));
        assert!(capture.is_top(interaction));
        assert!(!capture.is_top(requester));

        capture.clear_released_entries();

        assert!(!capture.is_top(interaction));
        assert!(capture.is_top(requester));

        let mut capture = ModalCapture::default();
        let interaction = capture.acquire(owner("interaction"), INTERACTION);
        let requester = capture.acquire(owner("requester"), REQUESTER);

        assert!(capture.release_latched(requester));
        assert!(capture.is_top(interaction));
        assert!(!capture.is_top(requester));

        capture.clear_released_entries();

        assert!(capture.is_top(interaction));
        assert!(!capture.is_top(requester));
    }
}
