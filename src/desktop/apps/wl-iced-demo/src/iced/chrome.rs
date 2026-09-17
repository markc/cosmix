//! The iced chrome: a `cosmix-iced-widgets` menu bar in external-popup mode,
//! a tab bar and a search field. Menu panels are not drawn here; the host
//! shows each one on its own `xdg_popup` (see `popups.rs`).

use super::app::Action;
use cosmix_iced_host::core::widget::Id;
use cosmix_iced_host::core::{Background, Border, Color, Length, alignment};
use cosmix_iced_host::widget::{button, column, container, row, space, text};
use cosmix_iced_host::{Element, Program};
use cosmix_iced_widgets::menu::{Item, Menu, MenuState};
use cosmix_iced_widgets::{MenuStyle, TextField, Tokens};
use cosmix_wl_app::SurfaceInfo;

pub const SEARCH_ID: &str = "search";
pub const TAB_HEIGHT: u32 = 30;
const TAB_WIDTH: f32 = 120.0;

/// Logical height of the whole chrome band: the menu bar is one row tall.
pub fn band_height(style: &MenuStyle) -> u32 {
    style.row_height.ceil() as u32 + TAB_HEIGHT
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
    /// The bar published new menu state (opened, closed, another title).
    Menu(MenuState),
    /// A menu item was activated from the bar itself.
    Action(Action),
    /// A panel surface reports the row under the pointer (`None` off the
    /// rows). The panel widget is generic over the item message type, so
    /// these ride the same enum and only `PanelProgram` acts on them.
    PanelHover(Option<usize>),
    /// A panel surface reports a press on a row.
    PanelPress(usize),
}

pub struct Chrome {
    /// The menu tree. The host borrows it for its `Navigator`.
    pub items: Vec<Item<ChromeMsg>>,
    /// Open state, owned here because the bar widget needs it every view.
    pub menu_state: MenuState,
    pub tabs: Vec<String>,
    pub active: usize,
    pub search: String,
    /// An action the bar activated, for the host to run.
    pub pending: Option<Action>,
    pub tokens: Tokens,
    pub style: MenuStyle,
}

impl Chrome {
    pub fn new(items: Vec<Item<ChromeMsg>>) -> Self {
        let tokens = Tokens::default();
        Self {
            items,
            menu_state: MenuState::default(),
            tabs: (1..=3).map(|i| format!("Tab {i}")).collect(),
            active: 0,
            search: String::new(),
            pending: None,
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
            ChromeMsg::Menu(state) => self.menu_state = state,
            ChromeMsg::Action(action) => self.pending = Some(action),
            ChromeMsg::PanelHover(_) | ChromeMsg::PanelPress(_) => {}
        }
    }

    fn view(&self) -> Element<'_, ChromeMsg> {
        let style = self.style;
        let tokens = self.tokens;
        let bar = Menu::bar(self.items.clone())
            .style(style)
            .external_popups(ChromeMsg::Menu)
            .state(&self.menu_state);

        let search = TextField::new("Search", &self.search)
            .id(Self::search_id())
            .on_input(ChromeMsg::Search)
            .width(220.0)
            .size(style.text_size)
            .padding([2, 8])
            .style(move |_, status| tokens.text_input(status));

        let mut tabs = row![].spacing(2.0).padding([0, 4]);
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
        let tabs = container(
            tabs.push(space().width(Length::Fill))
                .push(search)
                .push(space().width(6.0))
                .height(Length::Fill)
                .align_y(alignment::Vertical::Center),
        )
        .height(TAB_HEIGHT as f32);

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
        let band = band_height(&MenuStyle::default());
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
