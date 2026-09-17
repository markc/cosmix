//! The iced chrome: menu bar titles, a search field and a tab bar. Menu
//! panels are not drawn here; they are separate popup surfaces.

use crate::menus::Metrics;
use cosmix_iced_host::core::widget::Id;
use cosmix_iced_host::core::{Background, Border, Color, Length, alignment};
use cosmix_iced_host::widget::{button, column, container, row, space, text};
use cosmix_iced_host::{Element, Program};
use cosmix_iced_widgets::{MenuStyle, TextField, Tokens};
use cosmix_wl_app::SurfaceInfo;

pub const SEARCH_ID: &str = "search";
pub const TAB_HEIGHT: u32 = 30;
const TAB_WIDTH: f32 = 120.0;

/// Logical height of the whole chrome band.
pub fn band_height(metrics: &Metrics) -> u32 {
    metrics.bar_height as u32 + TAB_HEIGHT
}

/// The chrome surface for a window: physical width, physical band height
/// and scale. The band never exceeds the window.
pub fn chrome_surface(info: &SurfaceInfo, band: u32) -> (u32, u32, f32) {
    let height = info
        .scale
        .to_physical(band)
        .clamp(1, info.physical.1.max(1));
    (info.physical.0.max(1), height, info.scale_factor() as f32)
}

#[derive(Debug, Clone)]
pub enum ChromeMsg {
    Tab(usize),
    Search(String),
}

pub struct Chrome {
    pub titles: Vec<String>,
    pub tabs: Vec<String>,
    pub active: usize,
    pub search: String,
    /// Bar title highlighted because its panel is open.
    pub open_root: Option<usize>,
    pub metrics: Metrics,
    pub tokens: Tokens,
    pub style: MenuStyle,
}

impl Chrome {
    pub fn new(titles: Vec<String>, metrics: Metrics) -> Self {
        let tokens = Tokens::default();
        Self {
            titles,
            tabs: (1..=3).map(|i| format!("Tab {i}")).collect(),
            active: 0,
            search: String::new(),
            open_root: None,
            metrics,
            tokens,
            style: tokens.menu_style(),
        }
    }

    pub fn search_id() -> Id {
        Id::new(SEARCH_ID)
    }
}

fn fill(colour: Color) -> Option<Background> {
    Some(Background::Color(colour))
}

impl Program for Chrome {
    type Message = ChromeMsg;

    fn update(&mut self, message: ChromeMsg) {
        match message {
            ChromeMsg::Tab(i) => self.active = i.min(self.tabs.len().saturating_sub(1)),
            ChromeMsg::Search(value) => self.search = value,
        }
    }

    fn view(&self) -> Element<'_, ChromeMsg> {
        let m = self.metrics;
        let style = self.style;
        let tokens = self.tokens;
        let mut bar = row![space().width(m.bar_x as f32)];
        for (i, title) in self.titles.iter().enumerate() {
            let open = self.open_root == Some(i);
            bar = bar.push(
                container(text(title.as_str()).size(style.text_size))
                    .width(m.bar_item_width as f32)
                    .height(Length::Fill)
                    .center_x(m.bar_item_width as f32)
                    .align_y(alignment::Vertical::Center)
                    .style(move |_| container::Style {
                        text_color: Some(if open {
                            style.selected_text
                        } else {
                            style.text
                        }),
                        background: open.then_some(Background::Color(style.selected)),
                        border: Border::default().rounded(style.radius),
                        ..container::Style::default()
                    }),
            );
        }
        let search = TextField::new("Search", &self.search)
            .id(Self::search_id())
            .on_input(ChromeMsg::Search)
            .width(220.0)
            .size(style.text_size)
            .padding([2, 8])
            .style(move |_, status| tokens.text_input(status));
        let bar = bar
            .push(space().width(Length::Fill))
            .push(search)
            .push(space().width(6.0))
            .height(m.bar_height as f32)
            .align_y(alignment::Vertical::Center);

        let mut tabs = row![].spacing(2.0).padding([0, m.bar_x as u16]);
        for (i, label) in self.tabs.iter().enumerate() {
            let active = i == self.active;
            tabs = tabs.push(
                button(text(label.as_str()).size(style.text_size))
                    .width(TAB_WIDTH)
                    .on_press(ChromeMsg::Tab(i))
                    .style(move |_, status| {
                        let hovered = matches!(status, button::Status::Hovered);
                        button::Style {
                            background: fill(if active {
                                tokens.surface
                            } else if hovered {
                                tokens.muted_surface
                            } else {
                                style.background
                            }),
                            text_color: if active { tokens.text } else { style.text },
                            border: Border::default().rounded(style.radius),
                            ..button::Style::default()
                        }
                    }),
            );
        }
        let tabs = container(tabs.height(Length::Fill))
            .height(TAB_HEIGHT as f32)
            .align_y(alignment::Vertical::Bottom);

        container(column![bar, tabs])
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_| container::Style {
                background: fill(style.background),
                ..container::Style::default()
            })
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmix_wl_app::Scale;

    #[test]
    fn chrome_surface_follows_size_and_scale() {
        let band = band_height(&Metrics::default());
        assert_eq!(band, 58);
        let info = SurfaceInfo::new((795, 447), Scale::Fractional(300));
        assert_eq!(chrome_surface(&info, band), (1988, 145, 2.5));
        // A configure that changes neither size nor scale asks for the same
        // surface, so the chrome is neither resized nor invalidated.
        let same = SurfaceInfo::new((795, 447), Scale::Fractional(300));
        assert_eq!(chrome_surface(&same, band), chrome_surface(&info, band));
        let resized = SurfaceInfo::new((640, 447), Scale::Fractional(300));
        assert_ne!(chrome_surface(&resized, band), chrome_surface(&info, band));
        let rescaled = SurfaceInfo::new((795, 447), Scale::Fractional(120));
        assert_eq!(chrome_surface(&rescaled, band), (795, 58, 1.0));
        // A window shorter than the band gets a band the size of the window.
        let short = SurfaceInfo::new((795, 20), Scale::Integer(1));
        assert_eq!(chrome_surface(&short, band), (795, 20, 1.0));
    }
}
