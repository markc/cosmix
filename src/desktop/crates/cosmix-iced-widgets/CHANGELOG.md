# Changelog

## 0.1.4

- Add `Tokens::tooltip_style` using the compiled `muted` surface/foreground pair,
  with `border` and token radius.
  Accept the resolved border-width metric; test deterministic token mapping.

## 0.1.3

- Expose iced TextInput submission through TextField::on_submit, including
  after undo restores the input.
