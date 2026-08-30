//! Pure domain policy for `cosmix-nspawnd`.
//!
//! The binary owns Bus, D-Bus, files and locks. This library owns only value
//! types and deterministic decisions so generation fencing can be tested
//! without a broker, systemd, or host privileges.

pub mod core;
