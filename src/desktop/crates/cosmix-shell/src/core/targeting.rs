//! Which output an action aimed at "where the user is" lands on (shell doc
//! §5, panel doc §6).

use super::OutputKey;

/// The output containing the focused window, else the one under the pointer;
/// `None` when neither is known.
///
/// The one rule for every such action: named activation resolves its target
/// here, and keyboard actions follow the same rule (shell doc §5), so the
/// keyboard's targeting converges on this seam rather than restating it.
pub fn target_output<'a>(
    focused_window: Option<&'a OutputKey>,
    pointer: Option<&'a OutputKey>,
) -> Option<&'a OutputKey> {
    focused_window.or(pointer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focused_window_output_wins_over_the_pointer() {
        let focused = OutputKey::new("DP-1").unwrap();
        let pointer = OutputKey::new("HDMI-A-1").unwrap();
        assert_eq!(target_output(Some(&focused), Some(&pointer)), Some(&focused));
        assert_eq!(target_output(None, Some(&pointer)), Some(&pointer));
        assert_eq!(target_output(Some(&focused), None), Some(&focused));
        assert_eq!(target_output(None, None), None);
    }
}
