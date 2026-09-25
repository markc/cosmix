//! The iced application (ced E1 plan §2, §4.4): state = the Controller plus
//! chrome state; `view` composes menu bar · tab strip · infobar ·
//! [`EditorWidget`](crate::editor::widget::EditorWidget) · find bar ·
//! problems/output panel · status bar · modal dialogs. A normal xdg toplevel,
//! `application_id = "dev.cosmix.ced"`, SingleThread executor, tiny-skia.
//! Stage E1f implements it.

use crate::config::Config;

pub const APP_ID: &str = "dev.cosmix.ced";

/// Run the windowed app registered on the Bus as `service`, opening `paths`.
pub fn run(service: &str, config: Config, paths: Vec<String>) -> anyhow::Result<()> {
    let _ = (service, config, paths);
    todo!("ced E1f")
}
