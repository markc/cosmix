//! Native Wayland input injection via `zwp_virtual_keyboard_v1`.
//!
//! Generates a minimal per-invocation XKB keymap (wtype-style): each character
//! gets its own ONE_LEVEL keycode→keysym mapping. No shift handling needed —
//! 'A' maps directly to keysym `A`, '!' maps to `exclam`, etc.

use std::collections::HashMap;
use std::os::fd::{AsFd, FromRawFd};

use anyhow::{Context, Result};
use wayland_client::{Connection, Dispatch, QueueHandle, protocol::{wl_registry, wl_seat}};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

fn char_to_keysym(c: char) -> Option<&'static str> {
    Some(match c {
        'a' => "a", 'b' => "b", 'c' => "c", 'd' => "d", 'e' => "e",
        'f' => "f", 'g' => "g", 'h' => "h", 'i' => "i", 'j' => "j",
        'k' => "k", 'l' => "l", 'm' => "m", 'n' => "n", 'o' => "o",
        'p' => "p", 'q' => "q", 'r' => "r", 's' => "s", 't' => "t",
        'u' => "u", 'v' => "v", 'w' => "w", 'x' => "x", 'y' => "y", 'z' => "z",
        'A' => "A", 'B' => "B", 'C' => "C", 'D' => "D", 'E' => "E",
        'F' => "F", 'G' => "G", 'H' => "H", 'I' => "I", 'J' => "J",
        'K' => "K", 'L' => "L", 'M' => "M", 'N' => "N", 'O' => "O",
        'P' => "P", 'Q' => "Q", 'R' => "R", 'S' => "S", 'T' => "T",
        'U' => "U", 'V' => "V", 'W' => "W", 'X' => "X", 'Y' => "Y", 'Z' => "Z",
        '0' => "0", '1' => "1", '2' => "2", '3' => "3", '4' => "4",
        '5' => "5", '6' => "6", '7' => "7", '8' => "8", '9' => "9",
        ' ' => "space", '\n' => "Return", '\t' => "Tab",
        '!' => "exclam", '@' => "at", '#' => "numbersign",
        '$' => "dollar", '%' => "percent", '^' => "asciicircum",
        '&' => "ampersand", '*' => "asterisk",
        '(' => "parenleft", ')' => "parenright",
        '-' => "minus", '_' => "underscore",
        '=' => "equal", '+' => "plus",
        '[' => "bracketleft", ']' => "bracketright",
        '{' => "braceleft", '}' => "braceright",
        '\\' => "backslash", '|' => "bar",
        ';' => "semicolon", ':' => "colon",
        '\'' => "apostrophe", '"' => "quotedbl",
        '`' => "grave", '~' => "asciitilde",
        ',' => "comma", '.' => "period",
        '<' => "less", '>' => "greater",
        '/' => "slash", '?' => "question",
        _ => return None,
    })
}

fn named_key_keysym(name: &str) -> Option<&'static str> {
    Some(match name {
        "enter" | "return" => "Return", "tab" => "Tab", "space" => "space",
        "esc" | "escape" => "Escape", "backspace" => "BackSpace", "delete" => "Delete",
        "up" => "Up", "down" => "Down", "left" => "Left", "right" => "Right",
        "home" => "Home", "end" => "End",
        "pageup" | "page_up" => "Prior", "pagedown" | "page_down" => "Next",
        "insert" => "Insert",
        "f1" => "F1", "f2" => "F2", "f3" => "F3", "f4" => "F4",
        "f5" => "F5", "f6" => "F6", "f7" => "F7", "f8" => "F8",
        "f9" => "F9", "f10" => "F10", "f11" => "F11", "f12" => "F12",
        _ => return None,
    })
}

/// Build a minimal XKB keymap. Returns (keymap_string, keysym→protocol_keycode).
fn build_keymap(keysyms: &[&str]) -> (String, HashMap<String, u32>) {
    let mut unique: Vec<&str> = Vec::new();
    let mut code_map = HashMap::new();

    for &ks in keysyms {
        if !unique.contains(&ks) {
            unique.push(ks);
        }
    }

    use std::fmt::Write;
    let mut km = String::with_capacity(256 + unique.len() * 32);

    write!(km, "xkb_keymap {{\n\
        xkb_keycodes \"(unnamed)\" {{\nminimum = 8;\nmaximum = {};\n",
        unique.len() + 9).unwrap();

    for (i, ks) in unique.iter().enumerate() {
        let code = (i + 1) as u32;
        write!(km, "<K{}> = {};\n", code, code + 8).unwrap();
        code_map.insert(ks.to_string(), code);
    }

    km.push_str("};\n\
        xkb_types \"(unnamed)\" { include \"complete\" };\n\
        xkb_compatibility \"(unnamed)\" { include \"complete\" };\n\
        xkb_symbols \"(unnamed)\" {\n");

    for (i, ks) in unique.iter().enumerate() {
        write!(km, "key <K{}> {{[{}]}};\n", i + 1, ks).unwrap();
    }

    km.push_str("};\n};\n");
    (km, code_map)
}

fn upload_keymap(vk: &ZwpVirtualKeyboardV1, keymap_str: &str) -> Result<()> {
    let bytes = keymap_str.as_bytes();
    let name = std::ffi::CString::new("cosmix-keymap").unwrap();
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    anyhow::ensure!(fd >= 0, "memfd_create failed: {}", std::io::Error::last_os_error());

    unsafe {
        let written = libc::write(fd, bytes.as_ptr() as *const _, bytes.len());
        libc::write(fd, [0u8].as_ptr() as *const _, 1); // null terminator
        if written < 0 || written as usize != bytes.len() {
            libc::close(fd);
            anyhow::bail!("Failed to write keymap to memfd");
        }
    }

    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    vk.keymap(1, owned.as_fd(), (bytes.len() + 1) as u32);
    Ok(())
}

/// Connect to Wayland compositor and create a virtual keyboard with the given keymap.
fn connect(keymap_str: &str) -> Result<(Connection, ZwpVirtualKeyboardV1)> {
    let conn = Connection::connect_to_env().context("Failed to connect to Wayland")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let mut state = VkState::default();

    conn.display().get_registry(&qh, ());
    queue.roundtrip(&mut state)?;

    let seat = state.seat.as_ref().context("No wl_seat found")?;
    let mgr = state.vk_manager.as_ref().context("No zwp_virtual_keyboard_manager_v1")?;
    let vk = mgr.create_virtual_keyboard(seat, &qh, ());

    upload_keymap(&vk, keymap_str)?;
    queue.roundtrip(&mut state)?;

    Ok((conn, vk))
}

/// Type text into the focused window using native Wayland virtual keyboard.
pub fn type_text(text: &str, _delay_us: u64) -> Result<()> {
    let keysyms: Vec<&str> = text.chars().filter_map(char_to_keysym).collect();
    let (km, codes) = build_keymap(&keysyms);
    let (conn, vk) = connect(&km)?;

    for c in text.chars() {
        if let Some(ks) = char_to_keysym(c) {
            if let Some(&code) = codes.get(ks) {
                vk.key(0, code, 1); // pressed
                vk.key(0, code, 0); // released
            }
        }
    }

    conn.flush()?;
    std::thread::sleep(std::time::Duration::from_millis(50));
    Ok(())
}

/// Send a key combo (e.g. "ctrl+v", "enter") using native Wayland virtual keyboard.
pub fn send_key(combo: &str, _delay_us: u64) -> Result<()> {
    let parts: Vec<&str> = combo.split('+').collect();

    let key_str = parts.last().unwrap().to_lowercase();
    let main_ks = if key_str.len() == 1 {
        char_to_keysym(key_str.chars().next().unwrap())
    } else {
        named_key_keysym(&key_str)
    }.ok_or_else(|| anyhow::anyhow!("Unknown key: {key_str}"))?;

    let mut keysyms = vec![main_ks];
    let mut depressed: u32 = 0;
    for &p in &parts[..parts.len() - 1] {
        match p.to_lowercase().as_str() {
            "ctrl" | "control" => { keysyms.push("Control_L"); depressed |= 4; }
            "shift"            => { keysyms.push("Shift_L");   depressed |= 1; }
            "alt"              => { keysyms.push("Alt_L");     depressed |= 8; }
            "super" | "meta"   => { keysyms.push("Super_L");   depressed |= 64; }
            _ => {}
        }
    }

    let (km, codes) = build_keymap(&keysyms);
    let (conn, vk) = connect(&km)?;

    if depressed != 0 { vk.modifiers(depressed, 0, 0, 0); }
    if let Some(&code) = codes.get(main_ks) {
        vk.key(0, code, 1);
        vk.key(0, code, 0);
    }
    if depressed != 0 { vk.modifiers(0, 0, 0, 0); }

    conn.flush()?;
    std::thread::sleep(std::time::Duration::from_millis(50));
    Ok(())
}

// -- Wayland dispatch --

#[derive(Default)]
struct VkState {
    seat: Option<wl_seat::WlSeat>,
    vk_manager: Option<ZwpVirtualKeyboardManagerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for VkState {
    fn event(state: &mut Self, reg: &wl_registry::WlRegistry, event: wl_registry::Event,
             _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_seat" => { state.seat = Some(reg.bind(name, version.min(7), qh, ())); }
                "zwp_virtual_keyboard_manager_v1" => { state.vk_manager = Some(reg.bind(name, 1, qh, ())); }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for VkState {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for VkState {
    fn event(_: &mut Self, _: &ZwpVirtualKeyboardManagerV1, _: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwpVirtualKeyboardV1, ()> for VkState {
    fn event(_: &mut Self, _: &ZwpVirtualKeyboardV1, _: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
