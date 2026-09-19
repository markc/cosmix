//! The built-in D-Bus adapters — one file per domain, each implementing
//! [`crate::adapter::Adapter`] and registering itself in
//! [`crate::citizen::builtin_adapters`]. Adapters speak zbus, so this
//! module rides the `cosmix` feature (gated in `lib.rs`) like the rest of
//! the Bus wiring; the supervision core stays bus-free and builds without it.

pub mod notify;
pub mod tray;
