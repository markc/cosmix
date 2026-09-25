//! Language detection (plan §3.9). E1 replaces the table with lsh's globs.
//!
//! Basename `scene.mix` → `scene`; `*.conf.mix` → `mix-data`; `*.mix` or a
//! `mix` shebang → `mix`; then `rs`→`rust`, `md`→`markdown`, `toml`, `json`,
//! `yaml|yml`→`yaml`, `sh|bash|zsh`→`shell`, `py`→`python`, `js|mjs`→`javascript`,
//! `c|h`→`c`, `cpp|hpp|cc`→`cpp`, `go`, `lua`, `xml|svg`→`xml`,
//! `diff|patch`→`diff`, `COMMIT_EDITMSG`→`git_commit`; else `text`
//! (`.ts` stays `text`: lsh has no TypeScript).

use std::path::Path;

/// Language id for a buffer, from its path and first line.
pub fn detect(path: Option<&Path>, first_line: &str) -> &'static str {
    let _ = (path, first_line);
    todo!("E0a: table above")
}
