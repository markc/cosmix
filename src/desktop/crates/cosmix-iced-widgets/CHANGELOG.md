# Changelog

## 0.1.5

- Add `elevated`/`elevated_text` to `Tokens` from the compiled `elevated`
  pair; `tooltip_style` paints on that pair (with `border` and the token
  radius) so tooltip text never sits on the surface it covers.
  `popover`/`popover_text` remain mapped and deliver the same elevated
  surface in alias-relying designs.

## 0.1.4

- Add `Tokens::tooltip_style` using the compiled `muted` surface/foreground pair,
  with `border` and token radius.
  Accept the resolved border-width metric; test deterministic token mapping.

## 0.1.3

- Expose iced TextInput submission through TextField::on_submit, including
  after undo restores the input.
