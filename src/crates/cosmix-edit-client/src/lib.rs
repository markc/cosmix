//! Frontend-side model for the `edit` service (ced E1 plan
//! `_plan/2026-09-26-ced-e1-implementation.md` §1.4): the mirror (local echo
//! + single-authority OT rebase), the editor model, highlighting and
//! diagnostics state. Shared by the ced app, `ced --headless` and the E2 scene
//! widget. No UI toolkit; nothing here needs a running async runtime; the Bus
//! transport stays in the consumer (the mirror only produces [`types::Outgoing`]
//! and consumes replies/events).
//!
//! Where each contract lives: pipeline, fold, replies, deadlines, recovery,
//! conflicts → [`mirror`]; editing commands and delta mapping → [`model`];
//! async result identity → [`highlight`] / [`diag`]; frozen shared types and
//! the Bus-lane label rule → [`types`]; the test server → `fake` (feature
//! `fake`).

pub mod diag;
pub mod highlight;
pub mod mirror;
pub mod model;
pub mod types;

#[cfg(any(test, feature = "fake"))]
pub mod fake;

#[cfg(test)]
mod tests {
    use crate::types::{OpIdGen, bus_lane_label};

    fn valid_label(l: &str) -> bool {
        !l.is_empty() && l.len() <= 64 && l.chars().all(|c| c.is_ascii_alphanumeric() || "._@/+-".contains(c))
    }

    #[test]
    fn bus_lane_labels_fit_the_origin_grammar() {
        assert_eq!(bus_lane_label("local:ctl-90"), "ced.local_ctl-90");
        assert_eq!(bus_lane_label("anon"), "ced.anon");
        let long = format!("mesh:{}@{}", "s".repeat(60), "p".repeat(54)); // 120 chars
        assert_eq!(long.len(), 120);
        let l = bus_lane_label(&long);
        assert_eq!(l.len(), 64, "{l}");
        assert!(valid_label(&l), "{l}");
        assert!(l.parse::<cosmix_edit_core::origin::Origin>().is_err(), "a bare label is not kind:label");
        assert!(format!("agent:{l}").parse::<cosmix_edit_core::origin::Origin>().is_ok());
        let other = format!("mesh:{}@{}", "s".repeat(60), "q".repeat(54));
        assert_ne!(bus_lane_label(&other), l, "distinct keys stay distinct after truncation");
        for key in ["local:x", "mesh:svc@peer.example", "anon", &long] {
            assert!(valid_label(&bus_lane_label(key)));
        }
    }

    #[test]
    fn op_ids_match_the_editd_grammar() {
        let mut g = OpIdGen::new(0xdead_beef);
        assert_eq!(g.next_id(), "cdeadbeef-000001");
        assert_eq!(g.next_tagged("keep1"), "cdeadbeef-keep1-000002");
        let ok = |s: &str| s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c));
        assert!(ok(&g.next_id()));
    }
}
