//! Plain Properties sidebar: all data comes from a non-blocking core snapshot.
use super::{Look, elide::Label};
use crate::app::Msg;
use cosmix_dopus_core::{format_modified_at, format_size, properties::Properties};
use iced::widget::{column, container, scrollable, text};
use iced::{Element, Length};

pub fn sidebar<'a>(look: Look, properties: Properties) -> Element<'a, Msg> {
    let mut content = column![
        text("Properties")
            .font(look.ui_font)
            .size(look.sidebar_px())
            .color(look.chrome.secondary_text)
    ]
    .spacing(look.chrome.gap)
    .padding(look.chrome.pad)
    .width(Length::Fill);
    let mut fields = Vec::new();
    let title = match properties {
        Properties::Folder { path, summary } => {
            fields.push(("Contents", summary));
            path
        }
        Properties::Entry {
            entry,
            count_pending,
            metadata,
        } => {
            let size = if entry.is_dir {
                entry
                    .child_count
                    .map(|n| format!("{n} {}", if n == 1 { "item" } else { "items" }))
                    .unwrap_or_else(|| if count_pending { "…" } else { "Unavailable" }.into())
            } else {
                entry
                    .size
                    .map(|n| format!("{n} bytes ({})", format_size(n)))
                    .unwrap_or_else(|| "…".into())
            };
            fields.push(("Size", size));
            match metadata {
                None => fields.push(("Details", "…".into())),
                Some(Err(error)) => fields.push(("Details", format!("Unavailable: {error}"))),
                Some(Ok(meta)) => {
                    if !entry.is_dir {
                        fields[0].1 = format!("{} bytes ({})", meta.size, format_size(meta.size));
                    }
                    fields.insert(0, ("Kind", meta.kind));
                    for (label, time) in [
                        ("Modified", meta.modified),
                        ("Created", meta.created),
                        ("Accessed", meta.accessed),
                    ] {
                        fields.push((
                            label,
                            time.map(format_modified_at).unwrap_or_else(|| "—".into()),
                        ));
                    }
                    fields.push(("Permissions", meta.permissions));
                    fields.push(("Owner:group", meta.owner_group));
                    if let Some(target) = meta.symlink_target {
                        fields.push(("Link target", target));
                    }
                }
            }
            fields.push((
                "Path",
                cosmix_dopus_core::sanitise_display_path(&entry.path),
            ));
            entry.name
        }
    };
    content = content.push(Label {
        text: title,
        font: look.ui_font,
        px: look.sidebar_px(),
        color: look.tokens.text,
    });
    for (label, value) in fields {
        content = content.push(
            column![
                text(label)
                    .font(look.ui_font)
                    .size(look.sidebar_px())
                    .color(look.tokens.muted_text),
                text(value)
                    .wrapping(iced::advanced::text::Wrapping::WordOrGlyph)
                    .shaping(iced::advanced::text::Shaping::Advanced)
                    .font(look.ui_font)
                    .size(look.sidebar_px())
                    .color(look.tokens.text)
                    .width(Length::Fill),
            ]
            .spacing(look.chrome.small),
        );
    }
    container(scrollable(content))
        .width(Length::Fill)
        .height(Length::Fill)
        .style(look.strip(look.chrome.secondary, look.chrome.secondary_text))
        .into()
}
