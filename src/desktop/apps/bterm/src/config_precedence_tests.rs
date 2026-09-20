use crate::{config, resolve_config};

#[test]
fn env_over_file_over_default() {
    let defaults = config::Config::default();
    assert_eq!(
        resolve_config(defaults, None, "xterm-256color")
            .config
            .font_px,
        13.0
    );
    let file: config::Config = cosmix_config::from_conf_mix_str("font_px: 18").unwrap();
    assert_eq!(
        resolve_config(file, None, "xterm-256color").config.font_px,
        18.0
    );
    assert_eq!(
        resolve_config(file, Some("21.5"), "xterm-rio")
            .config
            .font_px,
        21.5
    );
    for invalid in ["bad", "NaN", "inf", "5", "49", ""] {
        assert_eq!(
            resolve_config(file, Some(invalid), "xterm-rio")
                .config
                .font_px,
            18.0
        );
    }
    let settings = resolve_config(defaults, Some("16"), "xterm-rio");
    let printed = serde_json::to_value(settings).unwrap();
    assert_eq!(printed["TERM"], "xterm-rio");
    assert_eq!(printed["font_px"], 16.0);
    assert_eq!(printed["scrollback"], 1000);
    assert_eq!(printed["cursor"], "underline");
}
