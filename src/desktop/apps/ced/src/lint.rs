//! Frontend lint (ced E1 plan §4.10, D16): `mix lint --json -` on CAPTURED
//! bytes (stdin, cwd = the file's directory so relative `require()`
//! resolves), off the UI thread, for `mix` / `scene` / `mix-data` buffers on
//! open and after a save (and 1 s after the last edit under 1 MiB). Results
//! are tagged (`ResultTag`) and handed to `cosmix_edit_client::diag`. Stage
//! E1f implements it.

use cosmix_edit_client::highlight::ResultTag;

/// The lint binary (never a fallback: a missing binary is an error naming it).
pub const MIX: &str = "/opt/cosmix/bin/mix";

/// Run `mix lint --json -` over `text` with `cwd`; the raw JSON on success.
pub fn run(tag: &ResultTag, text: &str, cwd: Option<&std::path::Path>) -> Result<String, String> {
    let _ = (tag, text, cwd);
    todo!("ced E1f")
}
