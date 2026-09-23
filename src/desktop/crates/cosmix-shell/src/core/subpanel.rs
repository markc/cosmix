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
//! reserve their seats with `mount`/`forget`. Removal never re-implements
//! carousel policy: it delegates to the edge carousel's own removal landing
//! rule (previous neighbour, else next, else primary; the remembered
//! selection falls back to the primary).
//!
//! The current host has one selected model. Output replacement carries its
//! live carousels and explicitly migrates their seats; citizen ingress cannot
//! move a name by updating it onto another output.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};

use super::{Edge, OutputKey, ShellModel};

/// Where one registered sub-panel name lives, and which citizen owns it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubPanelSeat {
    /// Output carrying the name, updated only by explicit host migration.
    pub output: OutputKey,
    /// Edge whose carousel carries the sub-panel.
    pub edge: Edge,
    /// Broker-attested sender (qualified for remote callers), independent of
    /// authored scene metadata. Untracked anonymous mounts use a host handle.
    pub owner: String,
    /// Quoin receipt sequence at acceptance, not a broker incarnation token.
    pub accepted_at: u64,
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
                accepted_at: 0,
            },
        );
        Ok(())
    }

    /// Reserve a mount before accepting its content. Only the same owner on
    /// the same output and edge may update a live name. Conflicts are atomic.
    pub fn mount(
        &mut self,
        name: &str,
        output: OutputKey,
        edge: Edge,
        owner: impl Into<String>,
        accepted_at: u64,
    ) -> Result<(), SubPanelRegistryError> {
        let owner = owner.into();
        if let Some(seat) = self.seats.get_mut(name) {
            if seat.output != output || seat.edge != edge || seat.owner != owner {
                return Err(SubPanelRegistryError::Duplicate(name.to_owned()));
            }
            seat.accepted_at = accepted_at;
            return Ok(());
        }
        self.register(name, output, edge, owner)?;
        self.seats.get_mut(name).unwrap().accepted_at = accepted_at;
        Ok(())
    }

    /// Distinct locally registered owners to reconcile against broker discovery.
    /// Empty/qualified identities have no entry in the local registration set.
    pub fn live_owners(&self) -> std::collections::BTreeSet<String> {
        self.seats
            .values()
            .filter(|seat| !seat.owner.is_empty() && !seat.owner.contains('@'))
            .map(|seat| seat.owner.clone())
            .collect()
    }

    /// The singleton host migrates its live content when an output disappears.
    /// This is a host lifecycle operation, never an ingress collision bypass.
    pub fn migrate_output(&mut self, old: &OutputKey, new: &OutputKey) {
        for seat in self.seats.values_mut().filter(|seat| &seat.output == old) {
            seat.output = new.clone();
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
        self.remove_owned_before(owner, u64::MAX, model)
    }

    /// Remove only seats accepted before the observed absence. A replacement
    /// accepted at that receipt sequence or later survives the deferred sweep.
    pub fn remove_owned_before(
        &mut self,
        owner: &str,
        before: u64,
        model: &mut ShellModel,
    ) -> Vec<SubPanelSeat> {
        let names: Vec<String> = self
            .seats
            .iter()
            .filter(|(_, seat)| seat.owner == owner && seat.accepted_at < before)
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
    use super::super::LogicalSize;
    use super::*;

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
        model
            .declare_carousel(Edge::Right, ["notes", "calendar"])
            .unwrap();
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

        // The host mount path round-trips the same way: mount recreates a
        // forgotten seat, and forget alone also frees the name for register.
        registry.forget("status");
        registry
            .register("status", key("DP-1"), Edge::Top, "owner-c")
            .unwrap();
        registry.forget("status");
        registry
            .mount("status", key("HDMI-A-1"), Edge::Bottom, "owner-d", 1)
            .unwrap();
        let seat = registry.seat("status").unwrap();
        assert_eq!(
            (seat.output.as_str(), seat.edge, seat.owner.as_str()),
            ("HDMI-A-1", Edge::Bottom, "owner-d")
        );
        // A mount cannot silently steal or move an existing seat.
        assert!(
            registry
                .mount("status", key("DP-2"), Edge::Left, "owner-e", 2)
                .is_err()
        );
        assert_eq!(registry.seat("status").unwrap().owner, "owner-d");
        assert!(
            registry
                .mount("", key("DP-2"), Edge::Left, "owner-e", 2)
                .is_err()
        );
        assert!(registry.seat("").is_none());
    }

    #[test]
    fn mount_rejects_cross_output_edge_and_owner_collisions() {
        let mut registry = SubPanelRegistry::new();
        registry
            .mount("page", key("DP-1"), Edge::Left, "owner", 1)
            .unwrap();
        for (output, edge, owner) in [
            ("DP-2", Edge::Left, "owner"),
            ("DP-1", Edge::Right, "owner"),
            ("DP-1", Edge::Left, "other"),
        ] {
            assert_eq!(
                registry.mount("page", key(output), edge, owner, 2),
                Err(SubPanelRegistryError::Duplicate("page".into()))
            );
            assert_eq!(registry.seat("page").unwrap().accepted_at, 1);
        }
        registry
            .mount("page", key("DP-1"), Edge::Left, "owner", 2)
            .unwrap();
        assert_eq!(registry.seat("page").unwrap().accepted_at, 2);
    }

    #[test]
    fn disconnect_sweep_preserves_replacements_on_every_edge() {
        let mut registry = SubPanelRegistry::new();
        let mut model = model("DP-1");
        for edge in Edge::ALL {
            let old = format!("old-{edge:?}");
            let new = format!("new-{edge:?}");
            for (name, receipt) in [(&old, 1), (&new, 2)] {
                model.carousel_mut(edge).register(name).unwrap();
                registry
                    .mount(name, key("DP-1"), edge, "owner", receipt)
                    .unwrap();
            }
        }
        assert_eq!(
            registry.remove_owned_before("owner", 2, &mut model).len(),
            4
        );
        for edge in Edge::ALL {
            assert_eq!(model.carousel(edge).page_ids(), [format!("new-{edge:?}")]);
        }
    }
}
