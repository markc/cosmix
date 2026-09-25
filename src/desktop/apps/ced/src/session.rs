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

/// Read the session; a missing, unreadable, malformed or newer-version file
/// is an empty session (ced never refuses to start over its own state).
pub fn load(path: &std::path::Path) -> Session {
    let Ok(text) = std::fs::read_to_string(path) else { return Session::default() };
    match serde_json::from_str::<Session>(&text) {
        Ok(mut s) if s.version == VERSION => {
            s.recent.truncate(RECENT_MAX);
            if s.active.is_some_and(|a| a >= s.tabs.len()) {
                s.active = None;
            }
            s
        }
        _ => Session::default(),
    }
}

/// Write atomically: a sibling temp file, fsync, rename over, fsync the
/// directory. The parent directory is created if missing.
pub fn save(path: &std::path::Path, session: &Session) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().ok_or_else(|| std::io::Error::other("session path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".session.json.{}.tmp", std::process::id()));
    let body = serde_json::to_vec_pretty(session).map_err(std::io::Error::other)?;
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::File::open(dir)?.sync_all()
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

    #[test]
    fn save_then_load_round_trips_and_bad_files_are_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/session.json");
        assert_eq!(load(&path), Session::default());
        let s = Session {
            version: VERSION,
            active: Some(0),
            tabs: vec![SessionTab { path: Some("/tmp/x.mix".into()), recovery_id: None, caret: 3, first_line: 1 }],
            recent: vec!["/tmp/x.mix".into()],
        };
        save(&path, &s).unwrap();
        assert_eq!(load(&path), s);
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(load(&path), Session::default());
        std::fs::write(&path, r#"{"version":99,"active":null,"tabs":[],"recent":[]}"#).unwrap();
        assert_eq!(load(&path), Session::default());
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temp file left behind");
    }
}
