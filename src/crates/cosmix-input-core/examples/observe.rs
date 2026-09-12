//! Observe-only evdev validation probe (cosmix-inputd P0/P2 groundwork).
//!
//! Reads a keyboard event node, tracks side-specific modifier state, resolves
//! every key event through [`cosmix_input_core::Resolver`], and prints what verb
//! WOULD fire and whether the stroke WOULD be swallowed. It does **not** grab
//! (EVIOCGRAB) and does **not** re-emit — the compositor still sees every key,
//! so it cannot brick the keyboard. It is the safe half of the P0 empirical
//! check: prove the physical layer's side-specificity on real hardware (Right
//! Ctrl vs Left Ctrl) before the daemon ever grabs.
//!
//! Run (keyboard nodes need root; this reads keystrokes, so run it deliberately):
//!   sudo cargo run -p cosmix-input-core --example observe -- /dev/input/event8
//! Ctrl-C to stop. Nothing is grabbed; your keys keep working normally.
//!
//! This is scratch validation, NOT the shipped daemon (no grab, no uinput, no
//! Bus). It exists to confirm the mechanism the daemon will rely on.

use std::fs::File;
use std::io::Read;

use cosmix_input_core::{default_keymap, Edge, Resolver};
use cosmix_input_schema::SideModifiers;

// input-event-codes.h
const EV_KEY: u16 = 1;
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;

/// One `struct input_event` on 64-bit Linux: timeval (2×i64) + type + code +
/// value = 24 bytes. We only need type/code/value.
const EVENT_SIZE: usize = 24;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: observe /dev/input/eventN   (a keyboard event node, needs root)");
        std::process::exit(2);
    });
    let mut file = File::open(&path).unwrap_or_else(|error| {
        eprintln!("open {path}: {error} (keyboard nodes need root)");
        std::process::exit(1);
    });
    let resolver = Resolver::new(default_keymap());
    let mut mods = SideModifiers::NONE;
    let mut buffer = [0u8; EVENT_SIZE];

    eprintln!("observing {path} — OBSERVE ONLY, no grab, no re-emit. Ctrl-C to stop.");
    loop {
        if let Err(error) = file.read_exact(&mut buffer) {
            eprintln!("read: {error}");
            return;
        }
        let event_type = u16::from_ne_bytes([buffer[16], buffer[17]]);
        let code = u16::from_ne_bytes([buffer[18], buffer[19]]);
        let value = i32::from_ne_bytes([buffer[20], buffer[21], buffer[22], buffer[23]]);
        if event_type != EV_KEY {
            continue;
        }
        // Maintain the side-specific modifier state ourselves — this is the one
        // fact the daemon derives that xkb throws away.
        if set_modifier(&mut mods, code, value != 0) {
            continue; // a modifier key: track it, don't resolve it as a chord key.
        }
        let edge = match value {
            0 => Edge::Release,
            1 => Edge::Press,
            2 => Edge::Repeat,
            _ => continue,
        };
        let out = resolver.resolve(code, mods, edge);
        if out.swallow || out.verb.is_some() {
            println!(
                "code={code:<4} {edge:?}  mods={mods:?}  -> verb={:?} swallow={}",
                out.verb.as_ref().map(|v| v.as_str()),
                out.swallow
            );
        }
    }
}

/// Update the side-specific modifier state; returns true if `code` was a
/// tracked modifier key (so the caller skips resolving it as a chord key).
fn set_modifier(mods: &mut SideModifiers, code: u16, down: bool) -> bool {
    match code {
        KEY_LEFTCTRL => mods.left_ctrl = down,
        KEY_RIGHTCTRL => mods.right_ctrl = down,
        KEY_LEFTSHIFT => mods.left_shift = down,
        KEY_RIGHTSHIFT => mods.right_shift = down,
        KEY_LEFTALT => mods.left_alt = down,
        KEY_RIGHTALT => mods.right_alt = down,
        KEY_LEFTMETA => mods.left_super = down,
        KEY_RIGHTMETA => mods.right_super = down,
        _ => return false,
    }
    true
}
