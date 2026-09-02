//! Event-driven SCTK layer-shell host for Quoin.

#![deny(unsafe_code)]

mod corner_bus;
#[cfg(test)]
mod feature_graph;
pub mod input;
mod input_keysym;
pub mod output;
pub mod planner;
pub mod raw_handle;
pub mod runner;
pub mod surface;

pub use runner::{
    LayerHostConfig, LayerHostError, LayerHostWake, LayerPanelMounts, configure_layer_host,
};
