//! Input serials for popup grabs. Pure; the runtime feeds it input and asks
//! it which serial a grab request should carry.
//!
//! A compositor grants an `xdg_popup.grab` for a serial that started a live
//! pointer grab (a button press) or for the latest key press. Releases,
//! enters and motion never qualify. While a popup grab is active, a press
//! inside the app's own popups does not start a new pointer grab, so every
//! popup opened in that chain (a submenu, or a sibling root that replaces
//! the open one) must reuse the serial the chain was opened with.

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GrabSerials {
    press: Option<u32>,
    chain: Option<u32>,
}

impl GrabSerials {
    /// A pointer button or key went down.
    pub fn pressed(&mut self, serial: u32) {
        self.press = Some(serial);
    }

    /// The latest press serial (clipboard requests use it too).
    pub fn latest_press(&self) -> Option<u32> {
        self.press
    }

    /// The serial for a new grabbing popup. The first grab of a chain takes
    /// the latest press; later ones reuse it until the chain ends.
    pub fn for_grab(&mut self) -> Option<u32> {
        if self.chain.is_none() {
            self.chain = self.press;
        }
        self.chain
    }

    /// The chain serial, if a grab chain is live.
    pub fn chain(&self) -> Option<u32> {
        self.chain
    }

    /// The compositor dismissed the chain (`popup_done`): its serial is no
    /// longer a live grab.
    pub fn dismissed(&mut self) {
        self.chain = None;
    }

    /// Called once per loop iteration, after the app has had its turn: a
    /// chain with no grabbing popup left has ended. Deferring this to the end
    /// of the iteration lets an app close one root menu and open the next in
    /// the same turn under the same grab.
    pub fn settle(&mut self, grabbing_popups_open: bool) {
        if !grabbing_popups_open {
            self.chain = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_reuses_the_opening_press() {
        let mut g = GrabSerials::default();
        assert_eq!(g.for_grab(), None, "no press yet");
        g.pressed(10); // right click opens the menu
        assert_eq!(g.for_grab(), Some(10));
        // Clicking "More >" inside the menu is a new press, but the popup
        // grab still started with 10.
        g.pressed(12);
        assert_eq!(g.for_grab(), Some(10));
        g.settle(true);
        assert_eq!(g.for_grab(), Some(10));
    }

    #[test]
    fn switching_roots_in_one_turn_keeps_the_chain() {
        let mut g = GrabSerials::default();
        g.pressed(3);
        assert_eq!(g.for_grab(), Some(3));
        g.settle(true);
        // Hover or click on another title: the app closes the open root and
        // opens a new one before the iteration ends.
        g.pressed(4);
        assert_eq!(g.for_grab(), Some(3));
        g.settle(true);
    }

    #[test]
    fn chain_ends_when_the_last_grabbing_popup_closes() {
        let mut g = GrabSerials::default();
        g.pressed(3);
        g.for_grab();
        g.settle(false);
        assert_eq!(g.chain(), None);
        g.pressed(9);
        assert_eq!(g.for_grab(), Some(9), "a fresh menu uses the new press");
    }

    #[test]
    fn dismissal_ends_the_chain_at_once() {
        let mut g = GrabSerials::default();
        g.pressed(5);
        g.for_grab();
        g.dismissed();
        g.pressed(6);
        assert_eq!(g.for_grab(), Some(6));
    }
}
