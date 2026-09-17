//! Shared colours for the pro-audio controls and canvases.
use iced_core::renderer;
use iced_core::{Border, Color, Rectangle};

/// Colours and corner radius for `Fader`, `Knob`, `LevelMeter`, `Toggle`,
/// `Waveform` and `PianoRoll`. Build it with `Tokens::audio_style`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioStyle {
    /// Canvas and strip fill.
    pub background: Color,
    /// Fader track, knob body and meter background.
    pub track: Color,
    /// Fader fill below the thumb and the knob indicator.
    pub fill: Color,
    /// Fader thumb.
    pub thumb: Color,
    /// Labels.
    pub text: Color,
    /// Labels of inactive toggles.
    pub muted_text: Color,
    /// Outlines and the fader's 0 dB tick.
    pub border: Color,
    /// Meter zone below -12 dB.
    pub meter_low: Color,
    /// Meter zone from -12 dB to -3 dB.
    pub meter_high: Color,
    /// Meter zone above -3 dB.
    pub meter_clip: Color,
    /// Meter peak-hold line.
    pub peak: Color,
    /// Fill of an active toggle (solo style).
    pub active: Color,
    /// Label of an active toggle.
    pub active_text: Color,
    /// Fill of an active alert toggle (mute style).
    pub alert: Color,
    /// Label of an active alert toggle.
    pub alert_text: Color,
    /// Piano-roll bar lines and the waveform centre line.
    pub grid: Color,
    /// Piano-roll black-key lanes.
    pub lane: Color,
    /// Piano-roll notes; velocity sets the alpha.
    pub note: Color,
    /// Waveform body.
    pub waveform: Color,
    /// Playhead line on the roll and waveform.
    pub playhead: Color,
    /// Corner radius of thumbs, toggles and meters.
    pub radius: f32,
}

impl Default for AudioStyle {
    fn default() -> Self {
        crate::Tokens::default().audio_style()
    }
}

pub(crate) fn quad<R: renderer::Renderer>(
    renderer: &mut R,
    bounds: Rectangle,
    color: Color,
    radius: f32,
    border: Option<Color>,
) {
    renderer.fill_quad(
        renderer::Quad {
            bounds,
            border: Border {
                color: border.unwrap_or(Color::TRANSPARENT),
                width: if border.is_some() { 1.0 } else { 0.0 },
                radius: radius.into(),
            },
            ..Default::default()
        },
        color,
    );
}
