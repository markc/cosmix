//! Per-domain D-Bus adapters. Each adapter module implements
//! [`crate::adapter::Adapter`] and registers itself in
//! [`crate::citizen::builtin_adapters`].

pub mod tray;
