//! The runtime's own xkb state, kept beside sctk's so key events can carry
//! the base keysym and the consumed modifiers, and so repeats can be
//! translated with the modifiers held at repeat time.

use crate::event::{Keysym, Modifiers};
use xkbcommon::xkb;

#[derive(Debug, Clone, Copy)]
struct ModIndices([xkb::ModIndex; 6]);

impl ModIndices {
    // Order matches `modifiers_from`.
    const NAMES: [&'static str; 6] = [
        xkb::MOD_NAME_CTRL,
        xkb::MOD_NAME_ALT,
        xkb::MOD_NAME_SHIFT,
        xkb::MOD_NAME_LOGO,
        xkb::MOD_NAME_CAPS,
        xkb::MOD_NAME_NUM,
    ];

    fn new(keymap: &xkb::Keymap) -> Self {
        Self(Self::NAMES.map(|name| keymap.mod_get_index(name)))
    }
}

/// Build `Modifiers` from a per-modifier test, in `ModIndices::NAMES` order.
pub(crate) fn modifiers_from(test: impl Fn(usize) -> bool) -> Modifiers {
    Modifiers {
        ctrl: test(0),
        alt: test(1),
        shift: test(2),
        logo: test(3),
        caps_lock: test(4),
        num_lock: test(5),
    }
}

pub(crate) struct Translated {
    pub keysym: Keysym,
    pub text: Option<String>,
    pub consumed: Modifiers,
}

pub(crate) struct XkbState {
    keymap: xkb::Keymap,
    state: xkb::State,
    mods: ModIndices,
}

impl XkbState {
    pub fn new(keymap_text: String, mask: [u32; 4]) -> Option<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(
            &context,
            keymap_text,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::COMPILE_NO_FLAGS,
        )?;
        let state = xkb::State::new(&keymap);
        let mods = ModIndices::new(&keymap);
        let mut this = Self {
            keymap,
            state,
            mods,
        };
        this.update_mask(mask);
        Some(this)
    }

    /// `[depressed, latched, locked, group]` from `wl_keyboard.modifiers`.
    pub fn update_mask(&mut self, [depressed, latched, locked, group]: [u32; 4]) {
        self.state
            .update_mask(depressed, latched, locked, 0, 0, group);
    }

    fn code(raw: u32) -> xkb::Keycode {
        xkb::Keycode::new(raw + 8)
    }

    pub fn repeats(&self, raw: u32) -> bool {
        self.keymap.key_repeats(Self::code(raw))
    }

    pub fn base_keysym(&self, raw: u32) -> Option<Keysym> {
        let code = Self::code(raw);
        let layout = self.state.key_get_layout(code);
        self.keymap
            .key_get_syms_by_level(code, layout, 0)
            .first()
            .copied()
    }

    pub fn consumed(&self, raw: u32) -> Modifiers {
        let code = Self::code(raw);
        let idx = self.mods.0;
        modifiers_from(|i| {
            idx[i] != xkb::MOD_INVALID && self.state.mod_index_is_consumed(code, idx[i])
        })
    }

    /// The key as it reads with the current modifiers (no compose).
    pub fn translate(&self, raw: u32) -> Translated {
        let code = Self::code(raw);
        let text = self.state.key_get_utf8(code);
        Translated {
            keysym: self.state.key_get_one_sym(code),
            text: (!text.is_empty()).then_some(text),
            consumed: self.consumed(raw),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_order_matches_names() {
        let m = modifiers_from(|i| ModIndices::NAMES[i] == xkb::MOD_NAME_SHIFT);
        assert_eq!(
            m,
            Modifiers {
                shift: true,
                ..Modifiers::default()
            }
        );
        let m = modifiers_from(|i| ModIndices::NAMES[i] == xkb::MOD_NAME_NUM);
        assert!(m.num_lock && !m.caps_lock);
    }
}
