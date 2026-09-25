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
    let name = path.and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("");
    if name == "scene.mix" {
        return "scene";
    }
    if name.ends_with(".conf.mix") {
        return "mix-data";
    }
    if name.ends_with(".mix") || is_mix_shebang(first_line) {
        return "mix";
    }
    if name == "COMMIT_EDITMSG" {
        return "git_commit";
    }
    let ext = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => return "text",
    };
    match ext {
        "rs" => "rust",
        "md" => "markdown",
        "toml" => "toml",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "sh" | "bash" | "zsh" => "shell",
        "py" => "python",
        "js" | "mjs" => "javascript",
        "c" | "h" => "c",
        "cpp" | "hpp" | "cc" => "cpp",
        "go" => "go",
        "lua" => "lua",
        "xml" | "svg" => "xml",
        "diff" | "patch" => "diff",
        _ => "text",
    }
}

/// `#!…/mix …` or `#!…/env [-S] mix …`.
fn is_mix_shebang(first_line: &str) -> bool {
    let Some(rest) = first_line.strip_prefix("#!") else { return false };
    let mut words = rest.split_whitespace();
    let Some(interp) = words.next() else { return false };
    let base = interp.rsplit('/').next().unwrap_or(interp);
    if base == "mix" {
        return true;
    }
    base == "env" && words.find(|w| !w.starts_with('-')).is_some_and(|w| w.rsplit('/').next() == Some("mix"))
}
