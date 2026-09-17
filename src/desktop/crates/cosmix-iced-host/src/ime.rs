use crate::damage::DamageRect;
use crate::{Renderer, Theme};
use iced_core::input_method::{self, InputMethod, Preedit, Purpose};
use iced_core::text::{self, Paragraph as _, Span};
use iced_core::{Color, Padding, Point, Rectangle, Renderer as _, Size, Vector, alignment};
use iced_graphics::text::Paragraph;

/// What the focused widget wants from the input method.
///
/// Forward `Enabled` to text-input-v3 (`enable`, `set_cursor_rectangle` with
/// `logical_cursor`, `set_content_type` from `purpose`, `commit`) or to a
/// compositor IME path (`physical_cursor` is in buffer pixels).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ImeRequest {
    #[default]
    Disabled,
    Enabled {
        logical_cursor: Rectangle,
        physical_cursor: DamageRect,
        purpose: Purpose,
        /// The preedit the widget currently shows, echoed back by iced.
        preedit: Option<Preedit>,
    },
}

impl ImeRequest {
    pub(crate) fn from_iced(method: &InputMethod, scale: f32, size: Size<u32>) -> Self {
        match method {
            InputMethod::Disabled => Self::Disabled,
            InputMethod::Enabled {
                cursor,
                purpose,
                preedit,
            } => {
                // A caret is 1 logical px wide; keep at least one physical px
                // even at the surface edge.
                let physical_cursor =
                    DamageRect::from_logical(*cursor, scale, size.width, size.height).unwrap_or(
                        DamageRect {
                            x: ((cursor.x * scale).floor().max(0.0) as u32)
                                .min(size.width.saturating_sub(1)),
                            y: ((cursor.y * scale).floor().max(0.0) as u32)
                                .min(size.height.saturating_sub(1)),
                            width: 1,
                            height: 1,
                        },
                    );
                Self::Enabled {
                    logical_cursor: *cursor,
                    physical_cursor,
                    purpose: *purpose,
                    preedit: preedit.clone(),
                }
            }
        }
    }

    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled { .. })
    }
}

/// Preedit text drawn under the caret, as iced_winit does: text widgets do
/// not paint the composition themselves.
///
/// Ported from iced_winit 0.14.1 `src/window.rs` (`Preedit`, lines 337-460),
/// Copyright 2019 Héctor Ramón, iced contributors, MIT licence.
pub(crate) struct PreeditOverlay {
    position: Point,
    content: Paragraph,
    key: Option<PreeditKey>,
}

/// What the shaped paragraph depends on: text, selection, the selection's
/// background colour and the text size.
type PreeditKey = (String, Option<std::ops::Range<usize>>, [u8; 4], Option<u32>);

impl PreeditOverlay {
    pub fn new() -> Self {
        Self {
            position: Point::ORIGIN,
            content: Paragraph::default(),
            key: None,
        }
    }

    pub fn update(
        &mut self,
        cursor: Rectangle,
        preedit: &input_method::Preedit,
        background: Color,
        renderer: &Renderer,
    ) {
        use iced_core::text::Renderer as _;

        self.position = cursor.position() + Vector::new(0.0, cursor.height);
        let key = (
            preedit.content.clone(),
            preedit.selection.clone(),
            background.into_rgba8(),
            preedit.text_size.map(|size| size.0.to_bits()),
        );
        if self.key.as_ref() == Some(&key) {
            return;
        }
        let background = Color {
            a: 1.0,
            ..background
        };
        let content = preedit.content.as_str();
        let spans: Vec<Span<'_, (), iced_core::Font>> = match &preedit.selection {
            Some(selection)
                if content.is_char_boundary(selection.start)
                    && content.is_char_boundary(selection.end) =>
            {
                vec![
                    Span::new(&content[..selection.start]),
                    Span::new(if selection.start == selection.end {
                        "\u{200A}"
                    } else {
                        &content[selection.start..selection.end]
                    })
                    .color(background),
                    Span::new(&content[selection.end..]),
                ]
            }
            _ => vec![Span::new(content)],
        };
        self.content = Paragraph::with_spans(iced_core::Text {
            content: spans.as_slice(),
            bounds: Size::INFINITE,
            size: preedit.text_size.unwrap_or_else(|| renderer.default_size()),
            line_height: text::LineHeight::default(),
            font: renderer.default_font(),
            align_x: text::Alignment::Default,
            align_y: alignment::Vertical::Top,
            shaping: text::Shaping::Advanced,
            wrapping: text::Wrapping::None,
        });
        self.key = Some(key);
    }

    pub fn draw(&self, renderer: &mut Renderer, theme: &Theme, viewport: Rectangle) {
        use iced_core::text::Renderer as _;
        use iced_core::theme::Base as _;

        if self.content.min_width() < 1.0 {
            return;
        }
        let base = theme.base();
        let color = base.text_color;
        let background = Color {
            a: 1.0,
            ..base.background_color
        };
        let mut bounds = Rectangle::new(
            self.position - Vector::new(0.0, self.content.min_height()),
            self.content.min_bounds(),
        );
        bounds.x = bounds
            .x
            .max(viewport.x)
            .min(viewport.x + viewport.width - bounds.width);
        bounds.y = bounds
            .y
            .max(viewport.y)
            .min(viewport.y + viewport.height - bounds.height);

        renderer.with_layer(bounds, |renderer| {
            renderer.fill_quad(
                iced_core::renderer::Quad {
                    bounds,
                    ..Default::default()
                },
                background,
            );
            renderer.fill_paragraph(&self.content, bounds.position(), color, bounds);
            const UNDERLINE: f32 = 2.0;
            renderer.fill_quad(
                iced_core::renderer::Quad {
                    bounds: bounds.shrink(Padding {
                        top: bounds.height - UNDERLINE,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                color,
            );
            for span_bounds in self.content.span_bounds(1) {
                renderer.fill_quad(
                    iced_core::renderer::Quad {
                        bounds: span_bounds + (bounds.position() - Point::ORIGIN),
                        ..Default::default()
                    },
                    color,
                );
            }
        });
    }
}
