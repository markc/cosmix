//! Cross-output sub-panel name registry: global addresses and ownership.
//!
//! A [`Carousel`](super::Carousel) is one edge of one output's
//! [`ShellModel`](super::ShellModel), and a host runs one selected model at a
//! time — but a sub-panel name is an *address* (panel doc §5): globally
//! unique across all four edges and all outputs, and a citizen's sub-panels
//! are removed when their owner disconnects or crashes (§3). Neither rule can
//! live inside a model, so this registry sits above them: one instance per
//! process (the Quoin host and each embedded host own one) tracks every live
//! name with its `(output, edge, owner)` seat.
//!
//! [`SubPanelRegistry::register`] is the strict citizen-facing ingress and
//! refuses a name that is live anywhere. Hosts that mount content themselves
//! reconcile their seats with `reseat`/`forget`. Removal never re-implements
//! carousel policy: it delegates to the edge carousel's own removal landing
//! rule (previous neighbour, else next, else primary; the remembered
//! selection falls back to the primary).
//!
//! A seat's output records where the name was registered. Output migration
//! carries live carousels into the replacement model
//! ([`ShellModel::carry_live_state`]), so removal applies to whatever model
//! the caller holds rather than matching the seat's output against it.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};

use super::{Edge, OutputKey, ShellModel};

/// Where one registered sub-panel name lives, and which citizen owns it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubPanelSeat {
    /// Output the name was registered on. Registration-time identity, not a
    /// live-model address (see the module doc on output migration).
    pub output: OutputKey,
    /// Edge whose carousel carries the sub-panel.
    pub edge: Edge,
    /// The citizen that registered the sub-panel. A Bus-level disconnect of
    /// this citizen removes everything it owns.
    pub owner: String,
}

/// Live sub-panel names with their seats, one instance per process.
#[derive(Clone, Debug, Default)]
pub struct SubPanelRegistry {
    seats: BTreeMap<String, SubPanelSeat>,
}

/// Invalid sub-panel registration or removal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubPanelRegistryError {
    EmptyName,
    /// The name is already live — on any output, any edge, any owner.
    Duplicate(String),
    /// Removal targeted a name without a seat.
    Unknown(String),
}

impl Display for SubPanelRegistryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyName => formatter.write_str("sub-panel name must not be empty"),
            Self::Duplicate(name) => {
                write!(formatter, "sub-panel name '{name}' is already registered")
            }
            Self::Unknown(name) => write!(formatter, "sub-panel name '{name}' is not registered"),
        }
    }
}

impl Error for SubPanelRegistryError {}

impl SubPanelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The seat a name holds, if it is live.
    pub fn seat(&self, name: &str) -> Option<&SubPanelSeat> {
        self.seats.get(name)
    }

    /// Every live name owned by `owner`, in name order.
    pub fn names_owned_by(&self, owner: &str) -> Vec<&str> {
        self.seats
            .iter()
            .filter(|(_, seat)| seat.owner == owner)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Register a sub-panel name with its seat.
    ///
    /// Names are globally unique across all four edges and all outputs
    /// (panel doc §5): a name that is live anywhere is refused, whatever
    /// seat it holds — an identical seat is as much a duplicate as a
    /// conflicting one, because the second registration would attach
    /// different content to one address.
    pub fn register(
        &mut self,
        name: &str,
        output: OutputKey,
        edge: Edge,
        owner: impl Into<String>,
    ) -> Result<(), SubPanelRegistryError> {
        if name.trim().is_empty() {
            return Err(SubPanelRegistryError::EmptyName);
        }
        if self.seats.contains_key(name) {
            return Err(SubPanelRegistryError::Duplicate(name.to_owned()));
        }
        self.seats.insert(
            name.to_owned(),
            SubPanelSeat {
                output,
                edge,
                owner: owner.into(),
            },
        );
        Ok(())
    }

    /// Reconcile the seat for content a host has mounted under `name`,
    /// creating or replacing it.
    ///
    /// A host mount is not a citizen registration: the page id is already
    /// live in the edge carousel, and the citizen or edge can change across
    /// scene revisions, so this feed keeps the owner map fresh instead of
    /// refusing. Empty names are skipped — a host never mounts one, and the
    /// strict path is where that error belongs.
    pub fn reseat(
        &mut self,
        name: &str,
        output: OutputKey,
        edge: Edge,
        owner: impl Into<String>,
    ) {
        if let Some(seat) = self.seats.get_mut(name) {
            seat.output = output;
            seat.edge = edge;
            seat.owner = owner.into();
        } else if !name.trim().is_empty() {
            self.seats.insert(
                name.to_owned(),
                SubPanelSeat {
                    output,
                    edge,
                    owner: owner.into(),
                },
            );
        }
    }

    /// Drop a name's seat without touching any carousel — host teardown of
    /// content the landing rule will never see (the page never mounted, or
    /// the host unmounts it through its own path).
    pub fn forget(&mut self, name: &str) {
        self.seats.remove(name);
    }

    /// Remove one sub-panel: its seat plus its content in `model`'s edge
    /// carousel, landing per the carousel's removal rule.
    ///
    /// The carousel removal is best-effort: a seat can briefly outlive its
    /// carousel entry around host chrome teardown, and the landing rule has
    /// nothing to land when the name has no carousel content left.
    pub fn remove(
        &mut self,
        name: &str,
        model: &mut ShellModel,
    ) -> Result<SubPanelSeat, SubPanelRegistryError> {
        if name.trim().is_empty() {
            return Err(SubPanelRegistryError::EmptyName);
        }
        let Some(seat) = self.seats.remove(name) else {
            return Err(SubPanelRegistryError::Unknown(name.to_owned()));
        };
        let _ = model.carousel_mut(seat.edge).remove(name);
        Ok(seat)
    }

    /// Owner disconnect (or crash): remove every sub-panel the citizen owns,
    /// returning the removed seats in name order (panel doc §3).
    pub fn remove_all_owned(&mut self, owner: &str, model: &mut ShellModel) -> Vec<SubPanelSeat> {
        let names: Vec<String> = self
            .seats
            .iter()
            .filter(|(_, seat)| seat.owner == owner)
            .map(|(name, _)| name.clone())
            .collect();
        names
            .into_iter()
            .filter_map(|name| self.remove(&name, model).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::LogicalSize;

    fn key(output: &str) -> OutputKey {
        OutputKey::new(output).unwrap()
    }

    fn model(output: &str) -> ShellModel {
        ShellModel::new(
            key(output),
            LogicalSize::new(1_000.0, 800.0).unwrap(),
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(800),
            std::time::Duration::from_millis(200),
        )
        .unwrap()
    }

    #[test]
    fn duplicate_name_across_outputs_is_rejected() {
        let mut registry = SubPanelRegistry::new();
        registry
            .register("notify.n42", key("DP-1"), Edge::Right, "notifyd")
            .unwrap();
        // Same name on another output — the cross-output case the registry
        // exists for.
        assert_eq!(
            registry.register("notify.n42", key("HDMI-A-1"), Edge::Left, "notifyd"),
            Err(SubPanelRegistryError::Duplicate("notify.n42".into()))
        );
        // Same name on another edge of the same output, another owner, or
        // an identical seat: all duplicates, because the name is the address.
        for (output, edge, owner) in [
            (key("DP-1"), Edge::Bottom, "notifyd"),
            (key("HDMI-A-1"), Edge::Top, "calendard"),
            (key("DP-1"), Edge::Right, "notifyd"),
        ] {
            assert_eq!(
                registry.register("notify.n42", output, edge, owner),
                Err(SubPanelRegistryError::Duplicate("notify.n42".into()))
            );
        }
        // A different name is not a duplicate, wherever it sits; an empty
        // name never registers.
        registry
            .register("notify.n43", key("HDMI-A-1"), Edge::Left, "notifyd")
            .unwrap();
        assert!(registry.seat("notify.n43").is_some());
        assert_eq!(
            registry.register("", key("DP-1"), Edge::Left, "x"),
            Err(SubPanelRegistryError::EmptyName)
        );
    }

    #[test]
    fn owner_disconnect_removes_all_owned_subpanels() {
        let mut model = model("DP-1");
        model.declare_carousel(Edge::Right, ["notes", "calendar"]).unwrap();
        model.declare_carousel(Edge::Left, ["monitor"]).unwrap();
        let mut registry = SubPanelRegistry::new();
        for (name, edge) in [("notes", Edge::Right), ("calendar", Edge::Right)] {
            model.carousel_mut(edge).register(name).unwrap();
            registry
                .register(name, key("DP-1"), edge, "quoin-panel")
                .unwrap();
        }
        model.carousel_mut(Edge::Left).register("monitor").unwrap();
        registry
            .register("monitor", key("DP-1"), Edge::Left, "monitord")
            .unwrap();

        let removed = registry.remove_all_owned("quoin-panel", &mut model);
        assert_eq!(
            removed
                .iter()
                .map(|seat| (seat.edge, seat.owner.as_str()))
                .collect::<Vec<_>>(),
            [(Edge::Right, "quoin-panel"), (Edge::Right, "quoin-panel")],
            "both owned seats are removed in name order"
        );
        // Owned pages left the edge carousel; another citizen's page stays.
        assert!(model.carousel(Edge::Right).page_ids().is_empty());
        assert_eq!(model.carousel(Edge::Left).page_ids(), ["monitor"]);
        // The registry keeps only the surviving owner's entry.
        assert!(registry.names_owned_by("quoin-panel").is_empty());
        assert_eq!(registry.names_owned_by("monitord"), ["monitor"]);
        // An unknown owner removes nothing.
        assert!(registry.remove_all_owned("ghost", &mut model).is_empty());
    }

    #[test]
    fn remove_lands_per_carousel_rule_via_registry() {
        // Removing the shown page lands on the previous registered neighbour
        // while the remembered selection falls back to the primary — the
        // carousel's own two rules, reached through the registry.
        let mut model = model("DP-1");
        model
            .declare_carousel(Edge::Right, ["alpha", "beta", "gamma"])
            .unwrap();
        let mut registry = SubPanelRegistry::new();
        for name in ["alpha", "beta", "gamma"] {
            model.carousel_mut(Edge::Right).register(name).unwrap();
            registry
                .register(name, key("DP-1"), Edge::Right, "owner")
                .unwrap();
        }
        model.carousel_mut(Edge::Right).activate("gamma").unwrap();
        assert_eq!(model.carousel(Edge::Right).active_id(), Some("gamma"));

        let seat = registry.remove("gamma", &mut model).unwrap();
        assert_eq!((seat.edge, seat.owner.as_str()), (Edge::Right, "owner"));
        assert_eq!(
            model.carousel(Edge::Right).active_id(),
            Some("beta"),
            "removal via the registry lands on the previous neighbour"
        );
        assert_eq!(
            model.carousel(Edge::Right).last_selected(),
            Some("alpha"),
            "the remembered selection falls back to the primary"
        );

        // A removal on another edge does not disturb this edge's carousel.
        model.carousel_mut(Edge::Left).register("side").unwrap();
        registry
            .register("side", key("DP-1"), Edge::Left, "owner")
            .unwrap();
        registry.remove("side", &mut model).unwrap();
        assert_eq!(model.carousel(Edge::Right).page_ids(), ["alpha", "beta"]);

        // Unknown names and empty names are refused, seat untouched.
        registry
            .register("kept", key("DP-1"), Edge::Top, "owner")
            .unwrap();
        assert_eq!(
            registry.remove("missing", &mut model),
            Err(SubPanelRegistryError::Unknown("missing".into()))
        );
        assert_eq!(
            registry.remove("", &mut model),
            Err(SubPanelRegistryError::EmptyName)
        );
        assert!(registry.seat("kept").is_some());
    }

    #[test]
    fn re_register_after_remove_succeeds() {
        let mut model = model("DP-1");
        model.declare_carousel(Edge::Top, ["status"]).unwrap();
        let mut registry = SubPanelRegistry::new();
        registry
            .register("status", key("DP-1"), Edge::Top, "owner-a")
            .unwrap();
        registry.remove("status", &mut model).unwrap();
        // The name is free again — for the same or a different owner.
        registry
            .register("status", key("DP-1"), Edge::Top, "owner-b")
            .unwrap();
        assert_eq!(
            registry.seat("status").map(|seat| seat.owner.as_str()),
            Some("owner-b")
        );

        // The host feed path round-trips the same way: reseat recreates a
        // forgotten seat, and forget alone also frees the name for register.
        registry.forget("status");
        registry
            .register("status", key("DP-1"), Edge::Top, "owner-c")
            .unwrap();
        registry.forget("status");
        registry.reseat("status", key("HDMI-A-1"), Edge::Bottom, "owner-d");
        let seat = registry.seat("status").unwrap();
        assert_eq!(
            (seat.output.as_str(), seat.edge, seat.owner.as_str()),
            ("HDMI-A-1", Edge::Bottom, "owner-d")
        );
        // reseat refreshes an existing seat in place and skips empty names.
        registry.reseat("status", key("DP-2"), Edge::Left, "owner-e");
        assert_eq!(registry.seat("status").unwrap().owner, "owner-e");
        registry.reseat("", key("DP-2"), Edge::Left, "owner-e");
        assert!(registry.seat("").is_none());
    }
}
