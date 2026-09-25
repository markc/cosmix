//! Mix macros (ced E1 plan §4.9, D15 — cuttable to E1.1): `*.mix` files in
//! `<AppDirs ced>/config/macros/`, headers `-- ced-macro: <label>` and
//! optional `-- ced-key: <chord>`; run with `/opt/cosmix/bin/mix` in argv form
//! with `CED_BUFFER`, `CED_EPOCH`, `CED_REV`, `CED_PATH`, `CED_LANGUAGE`,
//! `CED_SEL_START`, `CED_SEL_END`, `CED_ORIGIN=agent:macro.<stem>`. Stage E1f.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroDef {
    /// File stem; the action id is `macro.<stem>`.
    pub stem: String,
    pub label: String,
    pub chord: Option<String>,
    pub path: std::path::PathBuf,
}

pub fn discover(dir: &std::path::Path) -> Vec<MacroDef> {
    let _ = dir;
    todo!("ced E1f")
}
