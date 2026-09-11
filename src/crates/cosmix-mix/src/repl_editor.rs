//! Opt-in adapter. The legacy editor receives the same calls when unselected.
use crate::completion::MixHelper;
use crate::editor::runtime::{Control, Line, OwnedEditor};
use crate::editor::{Generation, PromptProfile};
use rustyline::error::ReadlineError;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct HistoryRefusals(HashSet<PathBuf>);
impl HistoryRefusals {
    fn key(path: &Path) -> io::Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let mut key = PathBuf::new();
        for part in absolute.components() {
            match part {
                std::path::Component::ParentDir => {
                    key.pop();
                }
                std::path::Component::CurDir => {}
                part => key.push(part.as_os_str()),
            }
        }
        Ok(key)
    }
    fn refuse(&mut self, path: &Path) -> io::Result<()> {
        self.0.insert(Self::key(path)?);
        // Keep both the destination spelling and its current symlink target.
        if let Ok(target) = path.canonicalize() {
            self.0.insert(target);
        }
        Ok(())
    }
    fn allows(&self, path: &Path) -> io::Result<bool> {
        Ok(!self.0.contains(&Self::key(path)?)
            && !path
                .canonicalize()
                .is_ok_and(|target| self.0.contains(&target)))
    }
}

const MAX_HISTORY_FILE_BYTES: u64 = 16 * 1024 * 1024;
fn read_complete_history(path: &Path) -> io::Result<String> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_HISTORY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_HISTORY_FILE_BYTES {
        return Err(io::Error::other("history exceeds 16 MiB ingestion limit"));
    }
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub enum ReplEditor {
    Legacy(Box<rustyline::Editor<MixHelper, rustyline::history::DefaultHistory>>),
    Owned {
        editor: OwnedEditor,
        helper: Option<MixHelper>,
        generation: u64,
        history_refusals: HistoryRefusals,
    },
}
impl ReplEditor {
    pub fn new() -> rustyline::Result<Self> {
        if std::env::var("MIX_EDITOR").as_deref() != Ok("owned")
            || !io::stdin().is_terminal()
            || !io::stdout().is_terminal()
        {
            return rustyline::Editor::new().map(Box::new).map(Self::Legacy);
        }
        fn duplicate(fd: i32) -> io::Result<File> {
            let fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(unsafe { File::from_raw_fd(fd) })
            }
        }
        let editor = match duplicate(0)
            .and_then(|input| duplicate(1).and_then(|output| OwnedEditor::start(input, output)))
        {
            Ok(editor) => editor,
            Err(error) => {
                eprintln!("mix: owned editor unavailable; using rustyline: {error}");
                return rustyline::Editor::new().map(Box::new).map(Self::Legacy);
            }
        };
        Ok(Self::Owned {
            editor,
            helper: None,
            generation: 0,
            history_refusals: HistoryRefusals::default(),
        })
    }
    pub fn control(&self) -> Option<Control> {
        match self {
            Self::Owned { editor, .. } => Some(editor.control.clone()),
            Self::Legacy(_) => None,
        }
    }
    pub fn set_helper(&mut self, helper: Option<MixHelper>) {
        match self {
            Self::Legacy(editor) => editor.set_helper(helper),
            Self::Owned { helper: target, .. } => *target = helper,
        }
    }
    pub fn readline(&mut self, prompt: &str, continuation: bool) -> rustyline::Result<String> {
        match self {
            Self::Legacy(editor) => {
                // Legacy has no activation acknowledgement; this is the last
                // observed readline boundary, never an admission permit.
                crate::session_state::commit(crate::session_state::Transition::PromptReady {
                    continuation,
                });
                editor.readline(prompt)
            }
            Self::Owned {
                editor,
                helper,
                generation,
                ..
            } => {
                *generation = generation
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("prompt generation exhausted"))?;
                let profile = if continuation {
                    PromptProfile::Continuation
                } else {
                    PromptProfile::Primary(prompt.to_owned())
                };
                editor.begin(
                    Generation {
                        // Real attachment/session identity is deferred to stage D.
                        session: 1,
                        prompt: *generation,
                    },
                    profile,
                    helper.as_ref().map(MixHelper::snapshot).unwrap_or_default(),
                    Vec::new(),
                )?;
                crate::session_state::commit(crate::session_state::Transition::PromptReady {
                    continuation,
                });
                match editor.readline()? {
                    Line::Submitted(line) => Ok(line),
                    Line::Interrupted => Err(ReadlineError::Interrupted),
                    Line::Eof => Err(ReadlineError::Eof),
                }
            }
        }
    }
    pub fn load_history(&mut self, path: &Path) -> rustyline::Result<()> {
        match self {
            Self::Legacy(editor) => editor.load_history(path),
            Self::Owned {
                editor,
                history_refusals,
                ..
            } => {
                match read_complete_history(path).and_then(|text| editor.control.load_history(text))
                {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => Err(e.into()),
                    Err(e) => {
                        history_refusals.refuse(path)?;
                        eprintln!("mix: incomplete history load; history saving disabled: {e}");
                        Err(e.into())
                    }
                }
            }
        }
    }
    pub fn save_history(&mut self, path: impl AsRef<Path>) -> rustyline::Result<()> {
        let path = path.as_ref();
        match self {
            Self::Legacy(editor) => editor.save_history(path),
            Self::Owned {
                editor,
                history_refusals,
                ..
            } => {
                if !history_refusals.allows(path)? {
                    return Err(
                        io::Error::other("history saving disabled after incomplete load").into(),
                    );
                }
                let text = editor.control.encode_history()?;
                let mut file = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(path)?;
                file.write_all(text.as_bytes())?;
                Ok(())
            }
        }
    }
    pub fn add_history_entry(&mut self, line: &str) -> rustyline::Result<bool> {
        match self {
            Self::Legacy(editor) => editor.add_history_entry(line),
            Self::Owned { editor, .. } => Ok(editor.control.append_history(line)?),
        }
    }
    pub fn history(&self) -> Vec<String> {
        match self {
            Self::Legacy(editor) => editor.history().iter().cloned().collect(),
            Self::Owned { editor, .. } => editor.control.history().unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_refusal_sticks_to_destination_across_other_loads() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad");
        let good = dir.path().join("good");
        std::fs::write(&bad, [0xff]).unwrap();
        std::fs::write(&good, b"#V2\ncomplete\n").unwrap();
        let mut refusals = HistoryRefusals::default();
        assert_eq!(
            read_complete_history(&bad).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        refusals.refuse(&bad).unwrap();
        assert!(read_complete_history(&good).is_ok());
        assert!(refusals.allows(&good).unwrap());
        assert!(!refusals.allows(&bad).unwrap());
        std::fs::write(&bad, b"#V2\nnow complete\n").unwrap();
        assert!(read_complete_history(&bad).is_ok());
        assert!(!refusals.allows(&bad).unwrap());
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&bad, &alias).unwrap();
        assert!(!refusals.allows(&alias).unwrap());
    }
}
