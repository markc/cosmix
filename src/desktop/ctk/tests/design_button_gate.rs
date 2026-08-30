#[test]
fn compiled_design_is_the_only_button_style_authority() {
    // This is only a spelling/source-authority guard. The behavioural reload
    // gate lives in button::tests::in_memory_source_replacement_restyles_an_existing_button.
    let source = include_str!("../src/button.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("button production source precedes its tests");

    for forbidden in [
        "fn button_colours",
        "fn apply_button_metrics",
        "fn focused_border",
        "CtkThemeMetrics",
        "tokens::",
        "legacy_lift",
    ] {
        assert!(
            !production.contains(forbidden),
            "button production source still contains legacy authority: {forbidden}"
        );
    }
    for required in [
        "fn button_cell_key",
        ".button_cell(",
        "cell.ring.or(cell.border)",
        "cell.height",
        "cell.min_width",
        "cell.padding_x",
        "cell.border_width",
        "cell.radius",
    ] {
        assert!(
            production.contains(required),
            "button production source is missing compiled-cell authority: {required}"
        );
    }
}
