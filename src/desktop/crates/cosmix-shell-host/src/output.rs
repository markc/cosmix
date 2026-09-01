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
    RequestedOutputUnavailable {
        requested: String,
        available: Vec<String>,
    },
    NoCompleteOutput,
    MoreThanOneOutput,
}

impl Display for OutputError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestedOutputUnavailable {
                requested,
                available,
            } => {
                write!(
                    formatter,
                    "requested output {requested:?} is not complete or available; advertised outputs: {}",
                    display_output_names(available)
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
    let available = state
        .outputs()
        .filter_map(|output| state.info(&output).and_then(|info| info.name))
        .collect::<Vec<_>>();
    let mut complete = state.outputs().filter_map(|wl_output| {
        let info = state.info(&wl_output)?;
        let (width, height) = info.logical_size?;
        let logical_size = LogicalSize::new(width as f32, height as f32).ok()?;
        let key = output_key(info.name.as_deref(), info.id)?;
        (info.scale_factor > 0).then(|| SelectedOutput {
            key,
            wl_output,
            scale: info.scale_factor,
            info,
            logical_size,
        })
    });

    if let Some(requested_name) = requested_name {
        complete
            .find(|output| output.info.name.as_deref() == Some(requested_name))
            .ok_or_else(|| OutputError::RequestedOutputUnavailable {
                requested: requested_name.to_owned(),
                available,
            })
    } else {
        complete.next().ok_or(OutputError::NoCompleteOutput)
    }
}

fn display_output_names(names: &[String]) -> String {
    if names.is_empty() {
        "(none)".to_owned()
    } else {
        names
            .iter()
            .map(|name| format!("{name:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn output_key(name: Option<&str>, id: u32) -> Option<OutputKey> {
    let fallback = || format!("wl-output-{id}");
    let value = name
        .filter(|name| !name.trim().is_empty())
        .map_or_else(fallback, ToOwned::to_owned);
    OutputKey::new(value)
        .ok()
        .or_else(|| OutputKey::new(fallback()).ok())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_or_whitespace_output_names_use_the_protocol_id() {
        for name in [None, Some(""), Some(" \t\n")] {
            assert_eq!(
                output_key(name, 42).as_ref().map(OutputKey::as_str),
                Some("wl-output-42")
            );
        }
        assert_eq!(
            output_key(Some("DP-1"), 42).as_ref().map(OutputKey::as_str),
            Some("DP-1")
        );
    }

    #[test]
    fn missing_requested_output_error_lists_advertised_names_clearly() {
        let error = OutputError::RequestedOutputUnavailable {
            requested: "DP-9".to_owned(),
            available: vec!["DP-1".to_owned(), "HDMI-A-1".to_owned()],
        };
        assert_eq!(
            error.to_string(),
            "requested output \"DP-9\" is not complete or available; advertised outputs: \"DP-1\", \"HDMI-A-1\""
        );
        assert_eq!(display_output_names(&[]), "(none)");
    }
}
