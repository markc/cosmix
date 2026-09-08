//! Event-driven SCTK layer-shell host for Quoin.

#![deny(unsafe_code)]

pub mod background;
mod corner_bus;
#[cfg(test)]
mod feature_graph;
pub mod input;
mod input_keysym;
pub mod output;
pub mod planner;
mod presentation;
pub mod raw_handle;
mod render_target;
pub mod runner;
pub mod scene;
pub mod surface;

pub use runner::{
    LayerHostConfig, LayerHostDeadline, LayerHostError, LayerHostWake, LayerPanelMounts,
    configure_layer_host,
};
