//! `<AppDirs ced>/state/session.json` (ced E1 plan §4.7) — the format is
//! frozen in Stage S; load/save land in Stage E1d. Written atomically (temp,
//! fsync, rename) 1 s after the last change (armed by the change) and on exit.

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
/// At most this many recent files are kept.
pub const RECENT_MAX: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    /// Index into `tabs`.
    pub active: Option<usize>,
    pub tabs: Vec<SessionTab>,
    pub recent: Vec<String>,
}

/// A path tab reattaches by `path`; a scratch tab by `recovery_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionTab {
    pub path: Option<String>,
    pub recovery_id: Option<String>,
    /// View byte offset of the caret.
    pub caret: usize,
    /// 1-based first visible line.
    pub first_line: usize,
}

impl Default for Session {
    fn default() -> Self {
        Self { version: VERSION, active: None, tabs: Vec::new(), recent: Vec::new() }
    }
}

pub fn load(path: &std::path::Path) -> Session {
    let _ = path;
    todo!("ced E1d")
}

pub fn save(path: &std::path::Path, session: &Session) -> std::io::Result<()> {
    let _ = (path, session);
    todo!("ced E1d")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frozen_example_parses() {
        let s: Session = serde_json::from_str(
            r#"{"version":1,"active":1,"tabs":[{"path":"/home/u/x.mix","recovery_id":null,"caret":1204,"first_line":40},{"path":null,"recovery_id":"5f0c2a9e1b7d4c33","caret":0,"first_line":1}],"recent":["/home/u/x.mix"]}"#,
        )
        .unwrap();
        assert_eq!(s.tabs.len(), 2);
        assert_eq!(serde_json::from_str::<Session>(&serde_json::to_string(&s).unwrap()).unwrap(), s);
    }
}
