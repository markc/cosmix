//! Keyboard access to the shell (shell doc §5): output targeting, the focus
//! cycle through persistent panels, and the directive a host turns into
//! layer-shell keyboard interactivity.

use super::{Edge, OutputKey};

/// The output a keyboard action targets: the one holding the focused window,
/// else the one under the pointer. `None` when neither is known.
///
/// Quoin only receives keys while one of its own panel surfaces holds the
/// keyboard, so for a key it received the focused window is that panel and
/// its output always wins. The pointer fallback serves a future global route
/// (a compositor-grabbed chord fired while an application or nothing is
/// focused), whose inputs are comp's focus and pointer observations. Named
/// activation (panel doc §6) aims through this same seam, fed by those
/// observations (`apps/quoin` `activation.rs`).
pub fn keyboard_target_output<'a>(
    focused_window: Option<&'a OutputKey>,
    pointer: Option<&'a OutputKey>,
) -> Option<&'a OutputKey> {
    focused_window.or(pointer)
}

/// One stop of the "cycle focus through shell panels" binding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FocusStop {
    Panel(Edge),
    /// Focus returns to the application (the compositor's choice of surface).
    Application,
}

/// The next stop after `current`: each visible pinned or docked panel on the
/// output in [`Edge::ALL`] order, then the application, then round again.
/// `visible` lists those panels; a focus that is not one of them (a transient
/// panel, or nothing) starts the walk from the first.
pub fn next_focus_stop(visible: &[Edge], current: Option<Edge>) -> FocusStop {
    let next = match current.and_then(|edge| visible.iter().position(|&v| v == edge)) {
        Some(index) => visible.get(index + 1),
        None => visible.first(),
    };
    next.map_or(FocusStop::Application, |&edge| FocusStop::Panel(edge))
}

/// What the shell currently asks of keyboard focus. Hosts map it to
/// layer-shell interactivity; the model clears it from observed focus.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FocusDirective {
    /// Ordinary policy: mapped panels take focus on demand (a click).
    #[default]
    Follow,
    /// Move keyboard focus into this panel and keep it there until the
    /// directive is cleared (Escape, the cycle moving on, or focus leaving).
    Panel(Edge),
    /// Give keyboard focus back: every panel refuses it until the host
    /// reports that no panel holds it any more.
    Release,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_targets_focused_window_output_else_pointer() {
        let focused = OutputKey::new("DP-1").unwrap();
        let pointer = OutputKey::new("HDMI-A-1").unwrap();
        assert_eq!(
            keyboard_target_output(Some(&focused), Some(&pointer)),
            Some(&focused)
        );
        assert_eq!(keyboard_target_output(None, Some(&pointer)), Some(&pointer));
        assert_eq!(keyboard_target_output(Some(&focused), None), Some(&focused));
        assert_eq!(keyboard_target_output(None, None), None);
    }

    #[test]
    fn next_focus_stop_walks_visible_panels_then_the_application() {
        let visible = [Edge::Left, Edge::Right];
        assert_eq!(next_focus_stop(&visible, None), FocusStop::Panel(Edge::Left));
        assert_eq!(
            next_focus_stop(&visible, Some(Edge::Left)),
            FocusStop::Panel(Edge::Right)
        );
        assert_eq!(
            next_focus_stop(&visible, Some(Edge::Right)),
            FocusStop::Application
        );
        // A transient panel is not a stop: the walk starts from the first.
        assert_eq!(
            next_focus_stop(&visible, Some(Edge::Bottom)),
            FocusStop::Panel(Edge::Left)
        );
        assert_eq!(next_focus_stop(&[], Some(Edge::Top)), FocusStop::Application);
    }
}
