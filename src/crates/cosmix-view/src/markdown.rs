use std::path::PathBuf;
use crate::dot;

pub fn render_gfm(markdown: &str, base_dir: Option<&PathBuf>) -> String {
    cosmix_ui::markdown::render_gfm(markdown, base_dir, Some(dot::render_dot))
}
