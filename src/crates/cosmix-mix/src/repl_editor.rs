//! Opt-in adapter. The legacy editor receives the same calls when unselected.
use crate::completion::MixHelper;
use crate::editor::runtime::{Control, Line, OwnedEditor};
use crate::editor::{Generation, PromptProfile};
use rustyline::error::ReadlineError;
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

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
        history_writable: bool,
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
        Ok(Self::Owned {
            editor: OwnedEditor::start(duplicate(0)?, duplicate(1)?)?,
            helper: None,
            generation: 0,
            history_writable: true,
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
            Self::Legacy(editor) => editor.readline(prompt),
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
                history_writable,
                ..
            } => {
                *history_writable = false;
                match read_complete_history(path).and_then(|text| editor.control.load_history(text))
                {
                    Ok(()) => {
                        *history_writable = true;
                        Ok(())
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        *history_writable = true;
                        Err(e.into())
                    }
                    Err(e) => {
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
                history_writable,
                ..
            } => {
                if !*history_writable {
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
