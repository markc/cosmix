//! Optional evaluator-owner observation seam. Only owned data leaves this thread.
//! No transport, session identity, key or evaluator handle enters the callback.
use std::cell::Cell;
/// Owned events from synchronous builtins; no language values cross the seam.
pub enum Observation {
    DirectoryChanged { cwd: Option<String> },
    ForegroundChanged { active: bool },
}
type Observer = fn(Observation);
thread_local! {
    static OBSERVER: Cell<Option<Observer>> = const { Cell::new(None) };
}
pub fn set_observer(observer: Observer) {
    OBSERVER.set(Some(observer));
}
pub(crate) fn directory_changed() {
    if let Some(observer) = OBSERVER.get() {
        observer(Observation::DirectoryChanged {
            cwd: std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned()),
        });
    }
}

pub(crate) struct Foreground(Option<Observer>);
pub(crate) fn foreground() -> Foreground {
    let observer = OBSERVER.get();
    if let Some(observer) = observer {
        observer(Observation::ForegroundChanged { active: true });
    }
    Foreground(observer)
}
impl Drop for Foreground {
    fn drop(&mut self) {
        if let Some(observer) = self.0 {
            observer(Observation::ForegroundChanged { active: false });
        }
    }
}
