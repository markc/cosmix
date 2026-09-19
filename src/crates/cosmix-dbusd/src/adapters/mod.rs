//! The built-in D-Bus adapters — one file per domain, each registering
//! itself in [`crate::citizen::builtin_adapters`]. Adapters speak zbus,
//! so the modules ride the `cosmix` feature like the rest of the Bus
//! wiring; the supervision core stays bus-free and builds without it.

#[cfg(feature = "cosmix")]
pub mod notify;
