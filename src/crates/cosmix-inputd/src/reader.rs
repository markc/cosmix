//! The evdev reader. Two modes:
//!
//! - **observe** (safe): read a keyboard node WITHOUT `EVIOCGRAB`, track
//!   side-specific modifier state, resolve every key through the shared
//!   [`Resolver`], and report what verb WOULD fire. The compositor still sees
//!   every key, so it cannot brick the keyboard. Needs root to read the node and
//!   reads keystrokes — a supervised run.
//! - **grab** (the interception path): `EVIOCGRAB` the device, resolve each
//!   stroke, SWALLOW bound strokes (and fire their verb on the Bus) while
//!   re-emitting everything else through a uinput virtual keyboard. This is the
//!   dead-keyboard-risk path — but recoverable: the kernel releases the grab and
//!   destroys the uinput device when this process's fds close, i.e. on ANY
//!   process death (SIGKILL included). So `pkill -x cosmix-inputd` from another
//!   session, or the `--grab-timeout` watchdog, always restores the keyboard.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::time::Duration;

use cosmix_input_core::Edge;
use cosmix_input_schema::{
    PointerButton, PointerButtonAction, PointerButtonName, PointerMove, PointerScroll,
    SideModifiers,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::service::Shared;

// input-event-codes.h
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_REL: u16 = 2;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;
/// Highest key code we enable on the virtual device (input-event-codes.h KEY_MAX).
const KEY_MAX: u16 = 0x2ff;

/// One `struct input_event` on 64-bit Linux: timeval (2×i64) + type + code +
/// value = 24 bytes.
const EVENT_SIZE: usize = 24;

// ioctl request numbers (Linux asm-generic encoding). EVIOCGRAB from
// <linux/input.h>; the UI_* from <linux/uinput.h> (UINPUT_IOCTL_BASE 'U').
// Verified: EVIOCGRAB = _IOW('E', 0x90, int); UI_DEV_SETUP = _IOW('U', 3,
// sizeof(uinput_setup)=92); UI_SET_EVBIT/UI_SET_KEYBIT = _IOW('U', 100/101, int).
const EVIOCGRAB: libc::c_ulong = 0x4004_4590;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_DESTROY: libc::c_ulong = 0x5502;

const BUS_USB: u16 = 0x03;

/// A verb the grab reader resolved and wants fired on the Bus. Sent from the
/// (blocking) reader thread to the tokio side, which owns the Bus client.
#[derive(Clone)]
pub struct FiredVerb {
    pub verb: String,
    /// The binding's arguments, sent as the verb's body (`None` = empty body).
    pub args: Option<serde_json::Value>,
}

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

/// The interception path. `EVIOCGRAB` the device, then for every event:
/// re-emit modifiers and unbound/passthrough strokes through a uinput virtual
/// keyboard so the compositor still sees them; SWALLOW bound strokes (do not
/// re-emit) and send their verb to `fire_tx`. `timeout`, if set, arms a watchdog
/// that exits the process (releasing the grab via kernel fd-close) — the safety
/// valve for the supervised bring-up.
pub fn run_grab(
    device: &str,
    resolver: Shared,
    fire_tx: UnboundedSender<FiredVerb>,
    timeout: Option<Duration>,
) -> std::io::Result<()> {
    // Open the device; keep the owned File (its fd close releases the grab) and
    // grab an extra copy of the raw fd for the ioctl.
    let mut dev = File::open(device)?;
    let dev_fd = dev.as_raw_fd();

    // Build the virtual output FIRST — if uinput setup fails we return before
    // ever grabbing, so a broken setup can never leave the keyboard captured.
    let mut uinput = UinputDevice::keyboard()?;

    // Let the compositor discover and open the new virtual device BEFORE we grab.
    // If we grabbed immediately, keystrokes in that discovery window would be
    // re-emitted to a device nothing is reading yet and be lost. We are NOT
    // grabbed during this sleep, so the keyboard keeps working meanwhile.
    std::thread::sleep(Duration::from_millis(300));

    // Grab exclusively. After this the compositor sees nothing on `device` until
    // release. If the ioctl fails we drop `uinput` (destroying it) and return.
    if unsafe { libc::ioctl(dev_fd, EVIOCGRAB, 1 as libc::c_int) } != 0 {
        return Err(std::io::Error::last_os_error());
    }

    // Arm the safety watchdog IMMEDIATELY after grabbing and BEFORE any logging or
    // other fallible work: process exit closes every fd, so the kernel releases
    // the grab and removes the uinput device even if a later log write or the read
    // loop blocks. This is the escape hatch for the supervised bring-up, so
    // nothing that can block may precede it.
    if let Some(t) = timeout {
        std::thread::spawn(move || {
            std::thread::sleep(t);
            std::process::exit(0);
        });
    }
    eprintln!(
        "cosmix-inputd: GRAB active on {device} — re-emitting via uinput, firing bound verbs. \
         Release: kill this process (SIGKILL ok){}.",
        match timeout {
            Some(t) => format!(" or wait {}s for --grab-timeout", t.as_secs()),
            None => String::new(),
        }
    );

    // Discard events queued on the fd before the grab took effect: those strokes
    // were already delivered to the compositor (we were not exclusive yet), so
    // replaying them here would double-type or double-fire (the 300ms settle
    // above widens that window). Drain non-blocking, then resume blocking reads.
    let mut buffer = [0u8; EVENT_SIZE];
    let saved_flags = unsafe { libc::fcntl(dev_fd, libc::F_GETFL) };
    if saved_flags >= 0 {
        unsafe { libc::fcntl(dev_fd, libc::F_SETFL, saved_flags | libc::O_NONBLOCK) };
        while let Ok(n) = dev.read(&mut buffer) {
            if n != EVENT_SIZE {
                break;
            }
        }
        unsafe { libc::fcntl(dev_fd, libc::F_SETFL, saved_flags) };
    }

    // Per-keycode latch: for each currently-held non-modifier key, what we decided
    // on its PRESS — the swallow decision AND the verb to (re)fire on an allowed
    // repeat. Latched, not recomputed, because `mods`/keymap/mode can change while
    // a key is held: recomputing at release could swallow the release of a key
    // emitted on press (stuck-key leak — the MAJOR both cold-review arms found),
    // and recomputing at repeat could fire a different action than the press.
    let mut held: HashMap<u16, HeldKey> = HashMap::new();
    let mut mods = SideModifiers::NONE;
    loop {
        dev.read_exact(&mut buffer)?;
        let event_type = u16::from_ne_bytes([buffer[16], buffer[17]]);
        let code = u16::from_ne_bytes([buffer[18], buffer[19]]);
        let value = i32::from_ne_bytes([buffer[20], buffer[21], buffer[22], buffer[23]]);

        // Non-key events: re-emit SYN frames (they keep the stream well-formed),
        // drop the rest (e.g. EV_MSC scancodes — redundant with the key code, and
        // our device only advertises EV_SYN + EV_KEY).
        if event_type != EV_KEY {
            if event_type == EV_SYN {
                uinput.emit(&buffer)?;
            }
            continue;
        }

        // Modifier keys are tracked AND passed through, so the compositor's
        // modifier state stays correct; they are never resolved as chord keys and
        // never enter the latch.
        if set_modifier(&mut mods, code, value != 0) {
            uinput.emit(&buffer)?;
            continue;
        }

        match value {
            // Press: resolve press AND repeat under the current modifiers in one
            // lock, fire the press verb (if any), and latch both the swallow
            // decision and the repeat fire for this key's later edges.
            1 => {
                let (swallow, press_fire, repeat_fire) = {
                    let resolver = resolver.lock().expect("resolver poisoned");
                    let press = resolver.resolve(code, mods, Edge::Press);
                    let repeat = resolver.resolve(code, mods, Edge::Repeat);
                    (
                        press.swallow,
                        press.verb.map(|v| FiredVerb {
                            verb: v.as_str().to_string(),
                            args: press.args.clone(),
                        }),
                        repeat.verb.map(|v| FiredVerb {
                            verb: v.as_str().to_string(),
                            args: repeat.args.clone(),
                        }),
                    )
                };
                if let Some(fire) = press_fire {
                    let _ = fire_tx.send(fire);
                }
                held.insert(
                    code,
                    HeldKey {
                        swallowed: swallow,
                        repeat_fire,
                    },
                );
                if !swallow {
                    uinput.emit(&buffer)?;
                }
            }
            // Repeat: follow the latched decision — re-fire the latched repeat
            // verb (only present when the binding's repeat policy allows it), and
            // never re-emit a swallowed key. No re-resolve, so a mid-hold rebind
            // cannot fire a different action than the press.
            2 => match held.get(&code) {
                Some(h) if h.swallowed => {
                    if let Some(fire) = h.repeat_fire.as_ref() {
                        let _ = fire_tx.send(fire.clone());
                    }
                    // swallowed on press → swallow the repeat too (no re-emit)
                }
                // Emitted on press, or never-seen: pass the repeat through.
                _ => uinput.emit(&buffer)?,
            },
            // Release: apply the LATCHED decision, ignoring the current resolver
            // verdict (which may differ if modifiers/keymap/mode changed mid-hold).
            0 => match held.remove(&code) {
                // Swallowed on press → swallow the release (no stuck key).
                Some(h) if h.swallowed => {}
                // Emitted on press, or a release with no recorded press → emit, so
                // downstream never keeps a dangling key-down.
                _ => uinput.emit(&buffer)?,
            },
            // Unknown value: pass through rather than swallow.
            _ => uinput.emit(&buffer)?,
        }
    }
}

/// What the grab loop decided for a currently-held non-modifier key on its press,
/// latched so its repeat and release are handled consistently regardless of how
/// modifier state (or the keymap) changes while it is held.
struct HeldKey {
    /// Whether the press was swallowed — the release/repeat follow this.
    swallowed: bool,
    /// The fire (verb + args) for an allowed repeat (None if the binding does
    /// not repeat), captured at press time so a mid-hold rebind can't change it.
    repeat_fire: Option<FiredVerb>,
}

/// Owned uinput device. Keyboard and pointer use separate instances and
/// capabilities; both are destroyed on drop (or kernel fd close).
struct UinputDevice {
    file: File,
}

impl UinputDevice {
    fn keyboard() -> std::io::Result<Self> {
        Self::create(b"cosmix-inputd virtual keyboard", 1, 0..=KEY_MAX, &[])
    }

    fn pointer() -> std::io::Result<Self> {
        Self::create(
            b"cosmix-inputd virtual pointer",
            2,
            [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE],
            &[REL_X, REL_Y, REL_WHEEL, REL_HWHEEL],
        )
    }

    fn create(
        name: &[u8],
        product: u16,
        keys: impl IntoIterator<Item = u16>,
        axes: &[u16],
    ) -> std::io::Result<Self> {
        let file = OpenOptions::new().write(true).open("/dev/uinput")?;
        let fd = file.as_raw_fd();
        unsafe {
            check(libc::ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_int))?;
            check(libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int))?;
            for code in keys {
                check(libc::ioctl(fd, UI_SET_KEYBIT, code as libc::c_int))?;
            }
            if !axes.is_empty() {
                check(libc::ioctl(fd, UI_SET_EVBIT, EV_REL as libc::c_int))?;
                for &code in axes {
                    check(libc::ioctl(fd, UI_SET_RELBIT, code as libc::c_int))?;
                }
            }
            let mut setup = UinputSetup {
                id: InputId {
                    bustype: BUS_USB,
                    vendor: 0x1d5b,
                    product,
                    version: 1,
                },
                name: [0u8; 80],
                ff_effects_max: 0,
            };
            setup.name[..name.len()].copy_from_slice(name);
            check(libc::ioctl(
                fd,
                UI_DEV_SETUP,
                &setup as *const UinputSetup as *const libc::c_void,
            ))?;
            check(libc::ioctl(fd, UI_DEV_CREATE))?;
        }
        Ok(Self { file })
    }

    /// Write one 24-byte `input_event` verbatim to the virtual device. The
    /// kernel re-stamps the timestamp, so passing the read buffer through is
    /// correct.
    fn emit(&mut self, event: &[u8; EVENT_SIZE]) -> std::io::Result<()> {
        self.file.write_all(event)
    }
}

impl Drop for UinputDevice {
    fn drop(&mut self) {
        // Best-effort destroy; the fd close on drop also removes the device.
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), UI_DEV_DESTROY);
        }
    }
}

/// Service-owned injector, independent of physical readers and broker
/// connections. Invalid requests never create a device; failed creation can
/// be retried on the next valid request.
#[derive(Default)]
pub struct PointerInjector {
    device: Option<UinputDevice>,
}

impl PointerInjector {
    pub fn inject(&mut self, request: PointerInjection) -> std::io::Result<()> {
        if self.device.is_none() {
            self.device = Some(UinputDevice::pointer()?);
            // Give the seat's compositor time to discover the device before
            // the first frame, as for the keyboard grab path.
            std::thread::sleep(Duration::from_millis(300));
        }
        let device = self.device.as_mut().expect("pointer device created");
        for event in request.events() {
            device.emit(&event)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn with_test_file(file: File) -> Self {
        Self {
            device: Some(UinputDevice { file }),
        }
    }
}

pub enum PointerInjection {
    Move(PointerMove),
    Button(PointerButton),
    Scroll(PointerScroll),
}

impl PointerInjection {
    // TODO: input.pointer.warp needs an EV_ABS device or a comp-side verb that knows output geometry
    fn events(self) -> Vec<[u8; EVENT_SIZE]> {
        let mut events = Vec::with_capacity(4);
        let mut emit = |kind: u16, code: u16, value: i32| {
            let mut event = [0; EVENT_SIZE];
            event[16..18].copy_from_slice(&kind.to_ne_bytes());
            event[18..20].copy_from_slice(&code.to_ne_bytes());
            event[20..24].copy_from_slice(&value.to_ne_bytes());
            events.push(event);
        };
        match self {
            Self::Move(PointerMove { dx, dy }) => {
                emit(EV_REL, REL_X, dx);
                emit(EV_REL, REL_Y, dy);
            }
            Self::Scroll(PointerScroll { dy, dx }) => {
                emit(EV_REL, REL_WHEEL, dy);
                if let Some(dx) = dx {
                    emit(EV_REL, REL_HWHEEL, dx);
                }
            }
            Self::Button(PointerButton { button, action }) => {
                let code = match button {
                    PointerButtonName::Left => BTN_LEFT,
                    PointerButtonName::Right => BTN_RIGHT,
                    PointerButtonName::Middle => BTN_MIDDLE,
                };
                match action {
                    PointerButtonAction::Press => emit(EV_KEY, code, 1),
                    PointerButtonAction::Release => emit(EV_KEY, code, 0),
                    PointerButtonAction::Click => {
                        emit(EV_KEY, code, 1);
                        emit(EV_SYN, SYN_REPORT, 0);
                        emit(EV_KEY, code, 0);
                    }
                }
            }
        }
        emit(EV_SYN, SYN_REPORT, 0);
        events
    }
}

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

/// Turn a negative ioctl return into the current errno.
fn check(ret: libc::c_int) -> std::io::Result<()> {
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
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
