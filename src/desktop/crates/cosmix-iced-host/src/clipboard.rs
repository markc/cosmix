pub use iced_core::clipboard::Kind as ClipboardKind;

/// Clipboard access provided by the host (wl_data_device, a compositor
/// selection store, or nothing). `Primary` is the middle-click selection.
pub trait Clipboard {
    fn read(&self, kind: ClipboardKind) -> Option<String>;

    fn write(&mut self, kind: ClipboardKind, contents: String);
}

/// A clipboard that holds nothing and drops writes.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullClipboard;

impl Clipboard for NullClipboard {
    fn read(&self, _kind: ClipboardKind) -> Option<String> {
        None
    }

    fn write(&mut self, _kind: ClipboardKind, _contents: String) {}
}

/// An in-process clipboard with separate standard and primary slots.
#[derive(Debug, Default, Clone)]
pub struct MemoryClipboard {
    pub standard: Option<String>,
    pub primary: Option<String>,
}

impl Clipboard for MemoryClipboard {
    fn read(&self, kind: ClipboardKind) -> Option<String> {
        match kind {
            ClipboardKind::Standard => self.standard.clone(),
            ClipboardKind::Primary => self.primary.clone(),
        }
    }

    fn write(&mut self, kind: ClipboardKind, contents: String) {
        match kind {
            ClipboardKind::Standard => self.standard = Some(contents),
            ClipboardKind::Primary => self.primary = Some(contents),
        }
    }
}

pub(crate) struct Adapter<'a>(pub &'a mut dyn Clipboard);

impl iced_core::Clipboard for Adapter<'_> {
    fn read(&self, kind: ClipboardKind) -> Option<String> {
        self.0.read(kind)
    }

    fn write(&mut self, kind: ClipboardKind, contents: String) {
        self.0.write(kind, contents);
    }
}
