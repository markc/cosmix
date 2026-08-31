//! SCTK output discovery and the deliberately single-output v1 runtime shape.

#![deny(unsafe_code)]

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};

use cosmix_shell::core::{LogicalSize, OutputKey};
use smithay_client_toolkit::output::{OutputInfo, OutputState};
use wayland_client::protocol::wl_output;

use crate::surface::PanelSurface;

#[derive(Debug)]
pub struct SelectedOutput {
    pub key: OutputKey,
    pub wl_output: wl_output::WlOutput,
    pub info: OutputInfo,
    pub logical_size: LogicalSize,
    pub scale: i32,
}

#[derive(Debug)]
pub struct OutputRuntime {
    pub wl_output: wl_output::WlOutput,
    pub info: OutputInfo,
    pub logical_size: LogicalSize,
    pub scale: i32,
    pub panels: [PanelSurface; 4],
}

pub type OutputRuntimeMap = BTreeMap<OutputKey, OutputRuntime>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputError {
    RequestedOutputUnavailable(String),
    NoCompleteOutput,
    MoreThanOneOutput,
}

impl Display for OutputError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestedOutputUnavailable(name) => {
                write!(
                    formatter,
                    "requested output {name:?} is not complete or available"
                )
            }
            Self::NoCompleteOutput => {
                formatter.write_str("no complete Wayland output is available")
            }
            Self::MoreThanOneOutput => {
                formatter.write_str("Quoin v1 permits exactly one output runtime")
            }
        }
    }
}

impl Error for OutputError {}

/// Select by exact advertised name, or take the first complete output in
/// SCTK advertisement order. Complete means positive logical geometry and
/// scale are both known.
pub fn select_output(
    state: &OutputState,
    requested_name: Option<&str>,
) -> Result<SelectedOutput, OutputError> {
    let mut complete = state.outputs().filter_map(|wl_output| {
        let info = state.info(&wl_output)?;
        let (width, height) = info.logical_size?;
        let logical_size = LogicalSize::new(width as f32, height as f32).ok()?;
        (info.scale_factor > 0).then(|| SelectedOutput {
            key: OutputKey::new(
                info.name
                    .clone()
                    .unwrap_or_else(|| format!("wl-output-{}", info.id)),
            )
            .expect("generated output keys are non-empty"),
            wl_output,
            scale: info.scale_factor,
            info,
            logical_size,
        })
    });

    if let Some(requested_name) = requested_name {
        complete
            .find(|output| output.info.name.as_deref() == Some(requested_name))
            .ok_or_else(|| OutputError::RequestedOutputUnavailable(requested_name.to_owned()))
    } else {
        complete.next().ok_or(OutputError::NoCompleteOutput)
    }
}

pub fn insert_single_output(
    outputs: &mut OutputRuntimeMap,
    key: OutputKey,
    runtime: OutputRuntime,
) -> Result<(), OutputError> {
    if !outputs.is_empty() || outputs.contains_key(&key) {
        return Err(OutputError::MoreThanOneOutput);
    }
    outputs.insert(key, runtime);
    debug_assert_eq!(outputs.len(), 1);
    Ok(())
}
