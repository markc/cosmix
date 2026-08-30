use crate::axis::closed_axis;

closed_axis! {
    deserialized
    /// Closed interaction state used as an argument to family tables.
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
    pub enum InteractionState {
        #[default]
        Resting => "resting",
        Hovered => "hovered",
        Pressed => "pressed",
        Disabled => "disabled",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A wire contract, for the same reason as the button axes: a theme file
    // writes these in `when:` selectors. Changing one is a format break.
    #[test]
    fn the_authored_interaction_vocabulary_is_pinned() {
        assert_eq!(
            InteractionState::NAMES,
            ["resting", "hovered", "pressed", "disabled"]
        );
    }

    #[test]
    fn indexes_are_each_variants_position_in_all() {
        for (position, state) in InteractionState::ALL.into_iter().enumerate() {
            assert_eq!(state.index(), position);
            assert_eq!(state.name(), InteractionState::NAMES[position]);
        }
    }
}

/// Renderer-neutral style state. CTK's component wrapper arrives in slice 2.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StyleStateKey {
    pub interaction: InteractionState,
    pub checked: bool,
    pub selected: bool,
    pub focus_visible: bool,
}
