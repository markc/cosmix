# Changelog

## 0.1.4

- Add `Tokens::tooltip_style` for opaque popover surfaces, matching foreground,
  muted borders and token radius. Accept the resolved border-width metric;
  test deterministic styling and opacity even with translucent input colours.

## 0.1.3

- Expose iced TextInput submission through TextField::on_submit, including
  after undo restores the input.
