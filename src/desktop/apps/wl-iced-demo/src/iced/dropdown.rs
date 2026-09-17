//! One menu panel as an iced program, drawn into its own popup surface.
//!
//! Local stand-in for a standalone panel widget from `cosmix-iced-widgets`
//! (its `Menu` only draws in-surface overlays). Rows are display-only: the
//! host hit-tests with [`crate::menus::Metrics`] and sets `selected`.

use crate::menus::{Entry, Metrics};
use cosmix_iced_host::core::{Background, Border, Color, Length, alignment};
use cosmix_iced_host::widget::{column, container, row, space, text};
use cosmix_iced_host::{Element, Program};
use cosmix_iced_widgets::MenuStyle;

#[derive(Debug, Clone, PartialEq)]
pub struct RowView {
    pub label: String,
    pub trailing: String,
    pub enabled: bool,
    pub separator: bool,
}

impl RowView {
    pub fn from_entries<A>(entries: &[Entry<A>]) -> Vec<Self> {
        entries
            .iter()
            .map(|e| match e {
                Entry::Action {
                    label,
                    accelerator,
                    enabled,
                    ..
                } => RowView {
                    label: label.clone(),
                    trailing: accelerator.clone(),
                    enabled: *enabled,
                    separator: false,
                },
                Entry::Submenu { label, children } => RowView {
                    label: label.clone(),
                    trailing: "\u{203a}".into(),
                    enabled: !children.is_empty(),
                    separator: false,
                },
                Entry::Separator => RowView {
                    label: String::new(),
                    trailing: String::new(),
                    enabled: false,
                    separator: true,
                },
            })
            .collect()
    }
}

pub struct Dropdown {
    pub rows: Vec<RowView>,
    pub selected: Option<usize>,
    pub metrics: Metrics,
    pub style: MenuStyle,
}

fn fill(colour: Color) -> Option<Background> {
    Some(Background::Color(colour))
}

impl Program for Dropdown {
    type Message = ();

    fn update(&mut self, _: ()) {}

    fn view(&self) -> Element<'_, ()> {
        let m = self.metrics;
        let style = self.style;
        let mut rows = column![].padding([m.panel_padding as u16, 0]);
        for (i, r) in self.rows.iter().enumerate() {
            if r.separator {
                rows = rows.push(
                    container(container(space()).width(Length::Fill).height(1.0).style(
                        move |_| container::Style {
                            background: fill(style.border),
                            ..container::Style::default()
                        },
                    ))
                    .height(m.separator_height as f32)
                    .padding([0, style.padding as u16])
                    .center_y(m.separator_height as f32),
                );
                continue;
            }
            let selected = self.selected == Some(i);
            let colour = match (r.enabled, selected) {
                (false, _) => style.disabled,
                (true, true) => style.selected_text,
                (true, false) => style.text,
            };
            let line = row![
                text(r.label.as_str()).size(style.text_size),
                space().width(Length::Fill),
                text(r.trailing.as_str()).size(style.text_size),
            ]
            .align_y(alignment::Vertical::Center);
            rows = rows.push(
                container(line)
                    .width(Length::Fill)
                    .height(m.row_height as f32)
                    .padding([0, style.padding as u16])
                    .align_y(alignment::Vertical::Center)
                    .style(move |_| container::Style {
                        text_color: Some(colour),
                        background: selected.then_some(Background::Color(style.selected)),
                        border: Border::default().rounded(style.radius),
                        ..container::Style::default()
                    }),
            );
        }
        container(rows)
            .width(Length::Fill)
            .height(Length::Fill)
            .padding([0, 4])
            .style(move |_| container::Style {
                background: fill(style.background),
                border: Border::default()
                    .rounded(style.radius)
                    .color(style.border)
                    .width(1.0),
                ..container::Style::default()
            })
            .into()
    }
}
