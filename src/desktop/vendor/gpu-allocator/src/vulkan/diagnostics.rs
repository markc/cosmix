//! Cosmix downstream block observations. Preserve when refreshing this vendor.
//! No observer means no clock, allocation, hashing or callback.

/// Numeric trace callback: stage, subject, detail, aux.
pub type Observer = fn(&'static str, u64, u64, u64);

// DELIBERATE DEVIATION: this workspace resolves gpu-allocator with
// default-features = false (no `std` feature), yet the observer needs
// OnceLock. The unconditional `extern crate std;` links std regardless of
// the feature set — fine on this hosted target, but it does change the
// crate's std-ness against its declaration. Acknowledged in
// COSMIX-PATCH.md; a no_std build of this vendored copy would need a cfg.
extern crate std;
use std::sync::OnceLock;

static OBSERVER: OnceLock<Observer> = OnceLock::new();

/// Install the process-lifetime observer. Never replaces an existing owner.
pub fn install(observer: Observer) -> bool {
    OBSERVER.set(observer).is_ok()
}

pub(super) fn record(stage: &'static str, fields: impl FnOnce() -> (u64, u64, u64)) {
    let Some(observer) = OBSERVER.get() else {
        return;
    };
    let (subject, detail, aux) = fields();
    observer(stage, subject, detail, aux);
}

#[cfg(test)]
mod tests {
    #[test]
    fn disabled_blocks_do_not_resolve_fields() {
        super::record("test", || panic!("disabled fields"));
    }
}
