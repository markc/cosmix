//! XWayland `DISPLAY` discovery for launched applications.
//!
//! An X11 application must be told which X server to talk to, and the tray
//! daemon is the process that launches applications — so this is where the
//! answer has to be supplied. The answer comes from cosmix-comp, which
//! publishes a per-socket descriptor once its XWayland generation is ready:
//!
//! * path — `$XDG_RUNTIME_DIR/cosmix-comp/<wayland-socket>.xwayland.env`
//!   (`crates/cosmix-comp/src/protocol/xwayland.rs:414`,
//!   `xwayland_descriptor_path`),
//! * contents — exactly `DISPLAY=:{display}\nGENERATION={generation}\n`,
//!   written to a mode-0600 temporary and renamed over the target
//!   (`crates/cosmix-comp/src/protocol/xwayland.rs:446`,
//!   `publish_xwayland_descriptor`),
//! * removed again when the generation dies (`remove_xwayland_descriptor`).
//!
//! The descriptor is keyed by the Wayland socket name comp serves —
//! `ProtocolServer` hands `XwaylandRuntime::new` the same name it passes to
//! `ListeningSocketSource::with_name`, i.e. the value a client sees as
//! `WAYLAND_DISPLAY`. Comp's own comment says the key exists "so nested
//! compositors never race one global file", so this reader keys by
//! `$WAYLAND_DISPLAY` — the socket *this* trayd is connected to — and never
//! scans the directory. A nested comp's apps then reach the nested comp's X
//! server rather than the host's.
//!
//! Absence is a normal state, never an error: XWayland may be disabled
//! (`xwayland.enabled`), may not be ready yet, or may have failed its
//! generation. Every failure path here returns "no display", which leaves
//! launching exactly as capable as it was before this module existed.

use std::env;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};

/// Refuse to read more than this from the descriptor. Comp writes ~30 bytes;
/// anything larger is not comp's file and is not worth pulling into memory.
const MAX_DESCRIPTOR_BYTES: u64 = 4096;

/// The most digits a display number may have. Real display numbers are one
/// or two digits; the cap only stops absurd input reaching a unit's
/// environment.
const MAX_DISPLAY_DIGITS: usize = 10;

/// The `DISPLAY` value to hand a launched application, or `None` when this
/// session has no XWayland of its own to point it at.
pub(crate) fn launch_display() -> Option<String> {
    let socket = env::var("WAYLAND_DISPLAY").ok()?;
    let runtime_dir = env::var_os("XDG_RUNTIME_DIR")?;
    let path = descriptor_path(Path::new(&runtime_dir), &socket)?;
    match read_descriptor(&path) {
        Ok(display) => display,
        Err(reason) => {
            // A descriptor that exists but cannot be believed is an anomaly
            // worth one line; a descriptor that is simply absent is the
            // ordinary XWayland-disabled case and stays silent.
            eprintln!(
                "cosmix-trayd: ignoring XWayland descriptor {}: {reason}",
                path.display()
            );
            None
        }
    }
}

/// Locate the descriptor for one Wayland socket, refusing any socket name
/// that is not a plain file name.
///
/// A `WAYLAND_DISPLAY` holding an absolute path is legal per the Wayland
/// spec, but comp only ever creates sockets *inside* `XDG_RUNTIME_DIR` via
/// `ListeningSocketSource::with_name`, so such a socket is not ours and has
/// no descriptor. Rejecting separators also keeps `..` from steering the
/// read out of comp's directory.
fn descriptor_path(runtime_dir: &Path, socket: &str) -> Option<PathBuf> {
    if socket.is_empty()
        || socket == "."
        || socket == ".."
        || socket.contains('/')
        || socket.contains('\0')
    {
        return None;
    }
    Some(
        runtime_dir
            .join("cosmix-comp")
            .join(format!("{socket}.xwayland.env")),
    )
}

/// Read one descriptor.
///
/// `Ok(None)` means "no descriptor" — the ordinary case when XWayland is
/// disabled or not yet ready. `Err` means the file was there but could not
/// be believed, which the caller reports and then treats as absence anyway.
fn read_descriptor(path: &Path) -> Result<Option<String>, String> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot read it: {error}")),
    };
    let mut text = String::new();
    file.take(MAX_DESCRIPTOR_BYTES)
        .read_to_string(&mut text)
        .map_err(|error| format!("cannot read it: {error}"))?;
    parse_display(&text)
        .map(Some)
        .ok_or_else(|| "no usable DISPLAY= line".to_owned())
}

/// Take `DISPLAY` out of a `KEY=value` descriptor.
///
/// Tolerant about everything that does not change meaning — surrounding
/// whitespace, line ordering, blank lines, unknown keys such as
/// `GENERATION` — and strict about the value itself: the first `DISPLAY`
/// key decides, and a value that is not `:<digits>` is a rejection rather
/// than a later key's chance. Scanning on past a bad `DISPLAY` would let a
/// malformed file promote a stale second entry.
fn parse_display(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "DISPLAY" {
            continue;
        }
        let value = value.trim();
        return is_display(value).then(|| value.to_owned());
    }
    None
}

/// `:<digits>` and nothing else — the exact shape comp writes.
///
/// Host-qualified (`host:0`) and screen-qualified (`:0.1`) forms are
/// refused deliberately: comp never writes them, so their presence means
/// the file is not comp's, and passing an unrecognised string into a unit's
/// environment is how an app ends up talking to an X server nobody chose.
fn is_display(value: &str) -> bool {
    let Some(number) = value.strip_prefix(':') else {
        return false;
    };
    !number.is_empty()
        && number.len() <= MAX_DISPLAY_DIGITS
        && number.chars().all(|character| character.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_the_descriptor_comp_actually_writes() {
        assert_eq!(
            parse_display("DISPLAY=:3\nGENERATION=7\n"),
            Some(":3".to_owned())
        );
    }

    #[test]
    fn tolerates_whitespace_ordering_and_unknown_keys() {
        let text = "\n  GENERATION = 12 \nCOSMIX_FUTURE_KEY=whatever\n  DISPLAY = :11  \n";
        assert_eq!(parse_display(text), Some(":11".to_owned()));
    }

    #[test]
    fn rejects_values_that_are_not_a_display_number() {
        for value in [
            "DISPLAY=\n",
            "DISPLAY=:\n",
            "DISPLAY=:1x\n",
            "DISPLAY=:1.0\n",
            "DISPLAY=host:0\n",
            "DISPLAY=:-1\n",
            "DISPLAY=:1 ; rm -rf /\n",
            "DISPLAY=:99999999999\n",
            "GENERATION=4\n",
            "",
        ] {
            assert_eq!(parse_display(value), None, "accepted {value:?}");
        }
    }

    #[test]
    fn the_first_display_key_decides() {
        assert_eq!(
            parse_display("DISPLAY=:1\nDISPLAY=:2\n"),
            Some(":1".to_owned())
        );
        // A malformed first entry is a rejection, not a search for a
        // better-looking second one.
        assert_eq!(parse_display("DISPLAY=bogus\nDISPLAY=:2\n"), None);
    }

    #[test]
    fn descriptor_is_keyed_by_this_compositors_socket() {
        let runtime = Path::new("/run/user/1000");
        assert_eq!(
            descriptor_path(runtime, "wayland-0"),
            Some(PathBuf::from(
                "/run/user/1000/cosmix-comp/wayland-0.xwayland.env"
            ))
        );
        // A nested compositor resolves to its own file, never the host's.
        assert_eq!(
            descriptor_path(runtime, "wayland-9"),
            Some(PathBuf::from(
                "/run/user/1000/cosmix-comp/wayland-9.xwayland.env"
            ))
        );
    }

    #[test]
    fn socket_names_that_are_not_plain_file_names_have_no_descriptor() {
        let runtime = Path::new("/run/user/1000");
        for socket in [
            "",
            ".",
            "..",
            "../../etc/passwd",
            "/run/user/1000/wayland-0",
            "nested/wayland-0",
        ] {
            assert_eq!(
                descriptor_path(runtime, socket),
                None,
                "accepted {socket:?}"
            );
        }
    }

    #[test]
    fn a_missing_descriptor_is_absence_not_failure() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("wayland-0.xwayland.env");
        assert_eq!(read_descriptor(&path), Ok(None));
    }

    #[test]
    fn a_published_descriptor_yields_its_display() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("wayland-0.xwayland.env");
        fs::write(&path, "DISPLAY=:4\nGENERATION=1\n").expect("write descriptor");
        assert_eq!(read_descriptor(&path), Ok(Some(":4".to_owned())));
    }

    #[test]
    fn a_malformed_descriptor_is_reported_and_not_used() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("wayland-0.xwayland.env");
        fs::write(&path, "GENERATION=1\nDISPLAY=not-a-display\n").expect("write descriptor");
        assert!(read_descriptor(&path).is_err());
    }

    #[test]
    fn an_oversized_descriptor_cannot_hide_a_display_past_the_cap() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("wayland-0.xwayland.env");
        let mut text = "PADDING=".to_owned();
        text.push_str(&"x".repeat(MAX_DESCRIPTOR_BYTES as usize));
        text.push_str("\nDISPLAY=:5\n");
        fs::write(&path, text).expect("write descriptor");
        assert!(read_descriptor(&path).is_err());
    }
}
