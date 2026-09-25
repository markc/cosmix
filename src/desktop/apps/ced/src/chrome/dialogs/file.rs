//! ced's own Open / Save As dialog (ced E1 plan D11): an iced modal over
//! `std::fs` — a path field with Tab completion, the directory listing
//! (directories first; click selects, double-click opens), recent files and
//! a hidden-files toggle. There is no portal backend and no iced picker, and
//! interactd's dialogs need the Bevy interactgui, so this is ced's own.
//!
//! The field accepts an absolute path, `~/…`, or a name relative to the
//! listed directory. Submitting a directory lists it; submitting a file opens
//! it (Open) or saves to it (Save As — an existing file asks first, and the
//! overwrite goes out as `force:true`).

use std::path::{Path, PathBuf};

use cosmix_edit_client::types::{Intent, TabId};
use iced::widget::{button, column, container, mouse_area, row, scrollable, text_input};
use iced::{Alignment, Element, Length, Padding};

use super::{DialogMsg, frame};
use crate::app::Msg;
use crate::chrome::Look;

pub const PATH_INPUT: &str = "ced-file-path";

/// Directory entries listed at most (a huge directory stays responsive).
pub const MAX_ENTRIES: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileMode {
    Open,
    /// Save `tab` under a new path, for `intent`.
    SaveAs { tab: TabId, intent: Intent },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub dir: bool,
}

#[derive(Debug, Clone)]
pub struct FileDialog {
    pub mode: FileMode,
    pub dir: PathBuf,
    pub input: String,
    pub entries: Vec<Entry>,
    pub truncated: bool,
    pub hidden: bool,
    pub selected: Option<usize>,
    pub recent: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileMsg {
    Input(String),
    Complete,
    Up,
    Down,
    Submit,
    Select(usize),
    Activate(usize),
    Parent,
    ToggleHidden,
    Recent(String),
}

/// What submitting produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOutcome {
    Open(Vec<String>),
    SaveAs { tab: TabId, intent: Intent, path: String, exists: bool },
}

pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_absolute())
}

/// Resolve what the user typed against the listed directory.
pub fn resolve(dir: &Path, input: &str) -> PathBuf {
    let input = input.trim();
    if input == "~" {
        return home().unwrap_or_else(|| dir.to_path_buf());
    }
    if let Some(rest) = input.strip_prefix("~/")
        && let Some(home) = home()
    {
        return home.join(rest);
    }
    let p = Path::new(input);
    if p.is_absolute() { p.to_path_buf() } else { dir.join(p) }
}

/// List `dir`: directories first, then files, each by case-insensitive name.
pub fn list(dir: &Path, hidden: bool) -> std::io::Result<(Vec<Entry>, bool)> {
    let mut entries: Vec<Entry> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !hidden && name.starts_with('.') {
                return None;
            }
            // Follow symlinks: a link to a directory lists as a directory.
            let dir = std::fs::metadata(e.path()).map(|m| m.is_dir()).unwrap_or(false);
            Some(Entry { name, dir })
        })
        .collect();
    entries.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    let truncated = entries.len() > MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    Ok((entries, truncated))
}

/// Tab completion: the longest common prefix of the names in the typed
/// path's directory that start with its last component; a unique directory
/// match gets a trailing `/`. `None` when nothing matches or nothing would
/// change.
pub fn complete(dir: &Path, input: &str, hidden: bool) -> Option<String> {
    let (head, stem) = match input.rfind('/') {
        Some(i) => (&input[..=i], &input[i + 1..]),
        None => ("", input),
    };
    let base = if head.is_empty() { dir.to_path_buf() } else { resolve(dir, head) };
    let (entries, _) = list(&base, hidden || stem.starts_with('.')).ok()?;
    let matches: Vec<&Entry> = entries.iter().filter(|e| e.name.starts_with(stem)).collect();
    let first = matches.first()?;
    let mut prefix = first.name.clone();
    for m in &matches[1..] {
        let common = prefix.chars().zip(m.name.chars()).take_while(|(a, b)| a == b).count();
        prefix = prefix.chars().take(common).collect();
    }
    let mut out = format!("{head}{prefix}");
    if matches.len() == 1 && first.dir {
        out.push('/');
    }
    (out != input).then_some(out)
}

impl FileDialog {
    pub fn new(mode: FileMode, dir: PathBuf, recent: Vec<String>) -> Self {
        let mut d = FileDialog {
            mode,
            dir,
            input: String::new(),
            entries: Vec::new(),
            truncated: false,
            hidden: false,
            selected: None,
            recent,
            error: None,
        };
        if let FileMode::SaveAs { .. } = d.mode {
            d.input = "untitled.txt".into();
        }
        d.relist();
        d
    }

    /// Prefill the name (Save As of a named buffer).
    pub fn with_name(mut self, name: &str) -> Self {
        self.input = name.to_owned();
        self
    }

    fn relist(&mut self) {
        match list(&self.dir, self.hidden) {
            Ok((entries, truncated)) => {
                self.entries = entries;
                self.truncated = truncated;
                self.error = None;
            }
            Err(e) => {
                self.entries.clear();
                self.error = Some(format!("{}: {e}", self.dir.display()));
            }
        }
        self.selected = None;
    }

    fn navigate(&mut self, dir: PathBuf) {
        self.dir = dir;
        if matches!(self.mode, FileMode::Open) {
            self.input.clear();
        }
        self.relist();
    }

    pub fn update(&mut self, msg: FileMsg) -> Option<FileOutcome> {
        match msg {
            FileMsg::Input(s) => {
                self.input = s;
                self.error = None;
            }
            FileMsg::Complete => {
                if let Some(c) = complete(&self.dir, &self.input, self.hidden) {
                    self.input = c;
                }
            }
            FileMsg::Up => {
                self.selected = match self.selected {
                    None | Some(0) => (!self.entries.is_empty()).then_some(0),
                    Some(i) => Some(i - 1),
                };
                self.take_selection();
            }
            FileMsg::Down => {
                let last = self.entries.len().checked_sub(1)?;
                self.selected = Some(self.selected.map_or(0, |i| (i + 1).min(last)));
                self.take_selection();
            }
            FileMsg::Select(i) => {
                self.selected = Some(i);
                self.take_selection();
            }
            FileMsg::Activate(i) => {
                self.selected = Some(i);
                self.take_selection();
                return self.submit();
            }
            FileMsg::Parent => {
                if let Some(parent) = self.dir.parent() {
                    let parent = parent.to_path_buf();
                    self.navigate(parent);
                }
            }
            FileMsg::ToggleHidden => {
                self.hidden = !self.hidden;
                self.relist();
            }
            FileMsg::Recent(path) => {
                self.input = path;
                return self.submit();
            }
            FileMsg::Submit => return self.submit(),
        }
        None
    }

    fn take_selection(&mut self) {
        if let Some(e) = self.selected.and_then(|i| self.entries.get(i)) {
            self.input = if e.dir { format!("{}/", e.name) } else { e.name.clone() };
        }
    }

    fn submit(&mut self) -> Option<FileOutcome> {
        if self.input.trim().is_empty() {
            return None;
        }
        let path = resolve(&self.dir, &self.input);
        if path.is_dir() {
            self.navigate(path);
            return None;
        }
        let text = path.to_string_lossy().into_owned();
        match &self.mode {
            FileMode::Open => Some(FileOutcome::Open(vec![text])),
            FileMode::SaveAs { tab, intent } => {
                if !path.parent().is_some_and(Path::is_dir) {
                    self.error = Some(format!("{} does not exist", path.parent().unwrap_or(Path::new("/")).display()));
                    return None;
                }
                Some(FileOutcome::SaveAs { tab: *tab, intent: intent.clone(), path: text, exists: path.exists() })
            }
        }
    }

    pub fn title(&self) -> &'static str {
        match self.mode {
            FileMode::Open => "Open File",
            FileMode::SaveAs { .. } => "Save As",
        }
    }

    pub fn view<'a>(&'a self, look: Look) -> Element<'a, Msg> {
        let t = look.tokens;
        let m = |f: FileMsg| Msg::Dialog(DialogMsg::File(f));
        let field = text_input("File name or path (Tab completes)", &self.input)
            .id(PATH_INPUT)
            .on_input(move |s| m(FileMsg::Input(s)))
            .on_submit(m(FileMsg::Submit))
            .font(look.mono)
            .size(look.ui_px)
            .padding(Padding::from([6, 10]))
            .style(look.input());
        let location = row![
            look.button("↑", Some(m(FileMsg::Parent))).style(look.secondary()),
            look.code(self.dir.to_string_lossy()).color(t.muted_text).width(Length::Fill),
            look.button(if self.hidden { "Hide Hidden" } else { "Show Hidden" }, Some(m(FileMsg::ToggleHidden))).style(look.flat()),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        let mut list = column![].spacing(0);
        for (i, e) in self.entries.iter().enumerate() {
            let selected = self.selected == Some(i);
            let label = if e.dir { format!("{}/", e.name) } else { e.name.clone() };
            let fg = if selected { t.selection_text } else if e.dir { look.chrome.accent } else { t.text };
            let item = button(look.code(label).color(fg))
                .width(Length::Fill)
                .padding(Padding::from([2, 8]))
                .on_press(m(FileMsg::Select(i)))
                .style(move |_, status| iced::widget::button::Style {
                    background: if selected {
                        Some(t.selection.into())
                    } else if matches!(status, iced::widget::button::Status::Hovered) {
                        Some(t.muted_surface.into())
                    } else {
                        None
                    },
                    text_color: fg,
                    border: iced::Border { radius: t.radius.into(), ..iced::Border::default() },
                    ..iced::widget::button::Style::default()
                });
            list = list.push(mouse_area(item).on_double_click(m(FileMsg::Activate(i))));
        }
        if self.truncated {
            list = list.push(look.small(format!("… more than {MAX_ENTRIES} entries; type to narrow")).color(t.muted_text));
        }
        let listing = container(scrollable(list).height(Length::Fixed(260.0)))
            .padding(4)
            .style(move |_| container::Style {
                background: Some(t.surface.into()),
                border: iced::Border { color: t.border, width: 1.0, radius: t.radius.into() },
                ..container::Style::default()
            });
        let mut body = column![location, listing, field].spacing(10);
        if !self.recent.is_empty() && matches!(self.mode, FileMode::Open) {
            let mut recent = row![look.small("Recent:").color(t.muted_text)].spacing(6).align_y(Alignment::Center);
            for path in self.recent.iter().take(6) {
                let name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
                recent = recent.push(
                    iced::widget::tooltip(
                        look.button(name, Some(m(FileMsg::Recent(path.clone())))).style(look.flat()),
                        container(look.code(path.as_str())).padding(6).style(look.strip(t.popover, t.popover_text)),
                        iced::widget::tooltip::Position::Top,
                    ),
                );
            }
            body = body.push(scrollable(recent).direction(scrollable::Direction::Horizontal(scrollable::Scrollbar::new().width(3).scroller_width(3))));
        }
        if let Some(error) = &self.error {
            body = body.push(look.small(error.as_str()).color(t.destructive));
        }
        let action = match self.mode {
            FileMode::Open => "Open",
            FileMode::SaveAs { .. } => "Save",
        };
        frame(
            look,
            self.title(),
            body.into(),
            vec![
                look.button("Cancel", Some(Msg::Dialog(DialogMsg::Close))).style(look.secondary()).into(),
                look.button(action, (!self.input.trim().is_empty()).then(|| m(FileMsg::Submit))).style(look.primary()).into(),
            ],
            640.0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("src")).unwrap();
        std::fs::create_dir(d.path().join("scenes")).unwrap();
        std::fs::write(d.path().join("scene.mix"), "").unwrap();
        std::fs::write(d.path().join("Readme.md"), "").unwrap();
        std::fs::write(d.path().join(".hidden"), "").unwrap();
        d
    }

    #[test]
    fn listing_puts_directories_first_and_hides_dotfiles() {
        let d = tree();
        let (entries, truncated) = list(d.path(), false).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["scenes", "src", "Readme.md", "scene.mix"]);
        assert!(!truncated);
        assert!(list(d.path(), true).unwrap().0.iter().any(|e| e.name == ".hidden"));
    }

    #[test]
    fn tab_completion() {
        let d = tree();
        assert_eq!(complete(d.path(), "sr", false).as_deref(), Some("src/"), "a unique directory gets its slash");
        assert_eq!(complete(d.path(), "sc", false).as_deref(), Some("scene"), "common prefix of scene.mix and scenes");
        assert_eq!(complete(d.path(), "scene", false), None, "nothing more to add");
        assert_eq!(complete(d.path(), "zz", false), None);
        assert_eq!(complete(d.path(), ".h", false).as_deref(), Some(".hidden"), "a dot prefix completes hidden names");
        let abs = format!("{}/Rea", d.path().display());
        assert_eq!(complete(Path::new("/"), &abs, false), Some(format!("{}/Readme.md", d.path().display())));
    }

    #[test]
    fn submitting_navigates_directories_and_opens_files() {
        let d = tree();
        let mut dlg = FileDialog::new(FileMode::Open, d.path().to_path_buf(), Vec::new());
        assert_eq!(dlg.update(FileMsg::Input("src".into())), None);
        assert_eq!(dlg.update(FileMsg::Submit), None);
        assert_eq!(dlg.dir, d.path().join("src"));
        dlg.update(FileMsg::Parent);
        dlg.update(FileMsg::Down);
        dlg.update(FileMsg::Down);
        dlg.update(FileMsg::Down);
        assert_eq!(dlg.input, "Readme.md");
        let out = dlg.update(FileMsg::Submit);
        assert_eq!(out, Some(FileOutcome::Open(vec![d.path().join("Readme.md").to_string_lossy().into_owned()])));
    }

    #[test]
    fn save_as_reports_existing_targets() {
        let d = tree();
        let intent = Intent::ui(4);
        let mut dlg = FileDialog::new(FileMode::SaveAs { tab: 4, intent: intent.clone() }, d.path().to_path_buf(), Vec::new())
            .with_name("scene.mix");
        match dlg.update(FileMsg::Submit) {
            Some(FileOutcome::SaveAs { tab: 4, exists: true, path, .. }) => assert!(path.ends_with("scene.mix")),
            other => panic!("{other:?}"),
        }
        dlg.update(FileMsg::Input("nope/x.txt".into()));
        assert_eq!(dlg.update(FileMsg::Submit), None);
        assert!(dlg.error.is_some(), "a missing parent is refused in the dialog");
    }
}
