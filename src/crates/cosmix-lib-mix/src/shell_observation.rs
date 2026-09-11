//! Optional evaluator-owner observation seam. Only owned data leaves this thread.
//! No transport, session identity, key or evaluator handle enters the callback.
use std::cell::Cell;
type DirectoryObserver = fn(Option<String>);
thread_local! {
    static DIRECTORY: Cell<Option<DirectoryObserver>> = const { Cell::new(None) };
}
pub fn set_directory_observer(observer: DirectoryObserver) {
    DIRECTORY.set(Some(observer));
}
pub(crate) fn directory_changed() {
    if let Some(observer) = DIRECTORY.get() {
        observer(
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned()),
        );
    }
}
