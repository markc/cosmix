//! Read the live kernel WG peer set via `wg show <iface> dump`.
//!
//! This is the **only** kernel interaction in P2, and it is a READ. The
//! `cosmix-lib-wg` crate is pure message-construction (no socket), and its
//! `dump` module exists precisely so the citizen shells out to `wg show …
//! dump` and parses the text — so P2's live read is that shell-out, isolated
//! here behind [`read_live`] so P3 can swap it for a direct WireGuard-generic
//! netlink `GET_DEVICE` without touching the reconciler.
//!
//! No shell is spawned — the interface name is passed as a single argv element
//! to `wg`, and is validated by the caller against the kernel ifname grammar
//! (`cosmix_wg::iface_name_for_mesh`) before it reaches here.

use std::process::Command;

use cosmix_wg::{DumpError, WgShowDump, parse_wg_show_dump};

/// Why a live read failed. A read failure is NOT fatal to the daemon — the
/// reconciler reports "live unavailable" and keeps the last intended set
/// serving; a missing lab interface (P2 runs on a dedicated lab iface that may
/// not be up yet) is the common benign case.
#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error("could not run `wg` (is wireguard-tools installed and on PATH?): {0}")]
    Spawn(std::io::Error),
    #[error("`wg show {iface} dump` exited {code}: {stderr}")]
    NonZero {
        iface: String,
        code: String,
        stderr: String,
    },
    #[error("`wg show {iface} dump` produced non-UTF-8 output")]
    NonUtf8 { iface: String },
    #[error("parsing `wg show {iface} dump`: {source}")]
    Parse { iface: String, source: DumpError },
}

/// Run `wg show <iface> dump` and parse it into the live peer snapshot.
pub fn read_live(iface: &str) -> Result<WgShowDump, LiveError> {
    let out = Command::new("wg")
        .arg("show")
        .arg(iface)
        .arg("dump")
        .output()
        .map_err(LiveError::Spawn)?;
    if !out.status.success() {
        return Err(LiveError::NonZero {
            iface: iface.to_string(),
            code: out
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string()),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    let text = String::from_utf8(out.stdout).map_err(|_| LiveError::NonUtf8 {
        iface: iface.to_string(),
    })?;
    parse_wg_show_dump(&text).map_err(|source| LiveError::Parse {
        iface: iface.to_string(),
        source,
    })
}
