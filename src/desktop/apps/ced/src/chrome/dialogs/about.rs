//! About CosMix Editor: version and build, the edit service it is attached
//! to, the theme and fonts in use, and where the config lives.

use iced::widget::{column, row};
use iced::{Element, Length};

use super::{DialogCtx, DialogMsg, frame};
use crate::app::Msg;
use crate::chrome::Look;

pub fn view<'a>(look: Look, ctx: &DialogCtx<'a>) -> Element<'a, Msg> {
    let t = look.tokens;
    let info = cosmix_buildinfo::build_info!();
    let line = |k: &'static str, v: String| -> Element<'a, Msg> {
        row![look.small(k).color(t.muted_text).width(Length::Fixed(110.0)), look.code(v)].spacing(10).into()
    };
    let service = match (ctx.edit_version, ctx.edit_epoch) {
        (Some(v), Some(e)) => format!("edit {v} · epoch {e}"),
        (Some(v), None) => format!("edit {v}"),
        _ => "not attached".to_owned(),
    };
    let recovery = match ctx.volatile {
        Some(false) => "recovery on".to_owned(),
        Some(true) => "VOLATILE — unsaved text is not crash-safe".to_owned(),
        None => "unknown".to_owned(),
    };
    let body = column![
        look.text("An editor for the CosMix edit service: agents' edits appear live, each origin has its own undo lane, and unsaved text survives a restart.")
            .color(t.popover_text),
        line("Version", format!("{} ({}, built {})", info.version, info.git_sha, info.build_time)),
        line("Service", service),
        line("Recovery", recovery),
        line("Theme", ctx.theme.clone()),
        line("Fonts", format!("{} · {}", ctx.ui, ctx.mono)),
        line("Config", ctx.config_path.clone().unwrap_or_else(|| "—".into())),
    ]
    .spacing(6);
    frame(
        look,
        "CosMix Editor",
        body.into(),
        vec![look.button("Close", Some(Msg::Dialog(DialogMsg::Close))).style(look.primary()).into()],
        520.0,
    )
}
