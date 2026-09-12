//! The evdev reader. Two modes:
//!
//! - **observe** (safe, implemented here): read a keyboard node WITHOUT
//!   `EVIOCGRAB`, track side-specific modifier state, resolve every key through
//!   the shared [`Resolver`], and report what verb WOULD fire. The compositor
//!   still sees every key, so it cannot brick the keyboard. Needs root to read
//!   the node and reads keystrokes — a supervised run.
//! - **grab** (the interception path): `EVIOCGRAB` the device, swallow matched
//!   strokes, re-emit the rest through uinput, and FIRE resolved verbs on the
//!   Bus. Dead-keyboard-if-wrong; gated behind a supervised run and NOT wired
//!   here — [`run_grab`] refuses with the reason.

use std::fs::File;
use std::io::Read;

use cosmix_input_core::Edge;
use cosmix_input_schema::SideModifiers;

use crate::service::Shared;

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
/// value = 24 bytes.
const EVENT_SIZE: usize = 24;

/// Observe-only: read, resolve, report. Never grabs, never fires. Blocks until
/// the device errors or EOF; run on a dedicated thread.
pub fn run_observe(device: &str, resolver: Shared) -> std::io::Result<()> {
    let mut file = File::open(device)?;
    let mut mods = SideModifiers::NONE;
    let mut buffer = [0u8; EVENT_SIZE];
    eprintln!("cosmix-inputd: OBSERVE-ONLY on {device} (no grab, no re-emit, no verbs fired)");
    loop {
        file.read_exact(&mut buffer)?;
        let event_type = u16::from_ne_bytes([buffer[16], buffer[17]]);
        let code = u16::from_ne_bytes([buffer[18], buffer[19]]);
        let value = i32::from_ne_bytes([buffer[20], buffer[21], buffer[22], buffer[23]]);
        if event_type != EV_KEY {
            continue;
        }
        if set_modifier(&mut mods, code, value != 0) {
            continue;
        }
        let edge = match value {
            0 => Edge::Release,
            1 => Edge::Press,
            2 => Edge::Repeat,
            _ => continue,
        };
        let out = {
            let resolver = resolver.lock().expect("resolver poisoned");
            resolver.resolve(code, mods, edge)
        };
        if let Some(verb) = out.verb.as_ref() {
            eprintln!(
                "cosmix-inputd: [observe] code={code} {edge:?} mods={mods:?} WOULD fire {} (swallow={})",
                verb.as_str(),
                out.swallow
            );
        }
    }
}

/// The interception path. NOT wired: `EVIOCGRAB` + uinput re-emission is the
/// dead-keyboard-risk step that must be brought up under supervision (plan P0
/// grab probe → P2). Refuses with the reason rather than half-grabbing.
pub fn run_grab(_device: &str, _resolver: Shared) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "grab mode (EVIOCGRAB + uinput) is the supervised P2 step and is not wired yet; \
         use --observe to validate resolution safely first",
    ))
}

/// Update side-specific modifier state; returns true if `code` is a tracked
/// modifier key (so it is not resolved as a chord key itself).
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
