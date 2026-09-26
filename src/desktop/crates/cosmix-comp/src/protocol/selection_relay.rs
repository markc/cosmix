//! One logical clipboard/primary selection, advertised on both Wayland seats.
//! Payloads remain fd-driven; generations reject reads of replaced mirror offers.
use super::*;
use smithay::wayland::selection::{
    data_device::{clear_data_device_selection, request_data_device_client_selection, set_data_device_selection},
    primary_selection::{clear_primary_selection, request_primary_client_selection, set_primary_selection},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionOrigin {
    Seat(SeatKind),
    #[cfg(feature = "xwayland")]
    X11,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SelectionProvenance {
    origin: SelectionOrigin,
    generation: u64,
}

#[derive(Default)]
pub(super) struct SelectionRelay {
    entries: [RelayEntry; 2],
}

#[derive(Default)]
struct RelayEntry {
    generation: u64,
    origin: Option<SelectionOrigin>,
    source: Option<SelectionSource>,
}

fn index(target: SelectionTarget) -> usize {
    match target { SelectionTarget::Clipboard => 0, SelectionTarget::Primary => 1 }
}

impl WaylandState {
    fn selection_seat(&self, kind: SeatKind) -> Seat<Self> {
        match kind { SeatKind::Human => self.human.seat.clone(), SeatKind::Agent => self.agent.seat.clone() }
    }

    fn offer_relay(&self, target: SelectionTarget, seat: SeatKind, mime_types: Vec<String>, provenance: Option<SelectionProvenance>) {
        let seat = self.selection_seat(seat);
        match (target, provenance) {
            (SelectionTarget::Clipboard, Some(data)) => set_data_device_selection(&self.display_handle, &seat, mime_types, data),
            (SelectionTarget::Primary, Some(data)) => set_primary_selection(&self.display_handle, &seat, mime_types, data),
            (SelectionTarget::Clipboard, None) => clear_data_device_selection(&self.display_handle, &seat),
            (SelectionTarget::Primary, None) => clear_primary_selection(&self.display_handle, &seat),
        }
    }

    pub(super) fn relay_client_selection(&mut self, target: SelectionTarget, source: Option<SelectionSource>, seat: Seat<Self>) {
        let (kind, other) = if seat == self.human.seat { (SeatKind::Human, SeatKind::Agent) }
            else if seat == self.agent.seat { (SeatKind::Agent, SeatKind::Human) }
            else { return };
        let mime_types = source.as_ref().map(SelectionSource::mime_types);
        let entry = &mut self.selection_relay.entries[index(target)];
        entry.generation = entry.generation.wrapping_add(1);
        entry.origin = source.as_ref().map(|_| SelectionOrigin::Seat(kind));
        entry.source = source;
        let provenance = entry.origin.map(|origin| SelectionProvenance { origin, generation: entry.generation });
        // Smithay's compositor setters do not call new_selection. Only the
        // original seat retains a client source; the mirror cannot echo it.
        self.offer_relay(target, other, mime_types.clone().unwrap_or_default(), provenance);
        #[cfg(feature = "xwayland")]
        self.bridge_selection_to_x11(target, mime_types.as_deref());
    }

    pub(super) fn relay_source_destroyed(&mut self, source: SelectionSource) {
        for target in [SelectionTarget::Clipboard, SelectionTarget::Primary] {
            if self.selection_relay.entries[index(target)].source.as_ref() == Some(&source) {
                self.clear_relay(target);
                #[cfg(feature = "xwayland")]
                self.bridge_selection_to_x11(target, None);
            }
        }
    }

    fn clear_relay(&mut self, target: SelectionTarget) {
        let entry = &mut self.selection_relay.entries[index(target)];
        entry.generation = entry.generation.wrapping_add(1);
        entry.origin = None;
        entry.source = None;
        for seat in [SeatKind::Human, SeatKind::Agent] { self.offer_relay(target, seat, Vec::new(), None); }
    }

    pub(super) fn send_relay_selection(&mut self, target: SelectionTarget, mime_type: String, fd: std::os::fd::OwnedFd, provenance: &SelectionProvenance) {
        let entry = &self.selection_relay.entries[index(target)];
        if entry.generation != provenance.generation || entry.origin != Some(provenance.origin) { return; }
        match provenance.origin {
            SelectionOrigin::Seat(kind) => {
                let seat = self.selection_seat(kind);
                match target {
                    SelectionTarget::Clipboard => { let _ = request_data_device_client_selection(&seat, mime_type, fd); }
                    SelectionTarget::Primary => { let _ = request_primary_client_selection(&seat, mime_type, fd); }
                }
            }
            #[cfg(feature = "xwayland")]
            SelectionOrigin::X11 => self.serve_x11_selection(target, mime_type, fd),
        }
    }

    #[cfg(feature = "xwayland")]
    pub(super) fn relay_to_x11(&mut self, target: SelectionTarget, mime_type: String, fd: std::os::fd::OwnedFd) {
        let entry = &self.selection_relay.entries[index(target)];
        // Explicit relay provenance is the only route from an agent source to
        // X11. Never ask the human compositor selection to serve itself.
        if let Some(origin @ SelectionOrigin::Seat(_)) = entry.origin {
            let provenance = SelectionProvenance { origin, generation: entry.generation };
            self.send_relay_selection(target, mime_type, fd, &provenance);
        }
    }

    #[cfg(feature = "xwayland")]
    pub(super) fn relay_from_x11(&mut self, target: SelectionTarget, mime_types: Vec<String>) {
        let entry = &mut self.selection_relay.entries[index(target)];
        entry.generation = entry.generation.wrapping_add(1);
        entry.origin = Some(SelectionOrigin::X11);
        entry.source = None;
        let provenance = SelectionProvenance { origin: SelectionOrigin::X11, generation: entry.generation };
        for seat in [SeatKind::Human, SeatKind::Agent] { self.offer_relay(target, seat, mime_types.clone(), Some(provenance)); }
    }

    #[cfg(feature = "xwayland")]
    pub(super) fn clear_x11_relay(&mut self, target: SelectionTarget) {
        if self.selection_relay.entries[index(target)].origin == Some(SelectionOrigin::X11) { self.clear_relay(target); }
    }
}
