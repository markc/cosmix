//! The root key router (ced E1 plan §2, §4.6): an iced widget that sees every
//! key event before its children (the `apps/term/src/keys.rs` pattern — never
//! `event::listen`, which drops events under load), resolves
//! [`crate::keymap`] chords and Alt+mnemonics to actions, and passes
//! everything else down to the focused child. Stage E1f implements it.
