# Application chrome typography

CTK applications use Noto Sans at a 15.333px body size by default. Authored
widget text scales from its existing 13px baseline. Terminal grid text keeps
its separate terminal font settings.

Set `COSMIX_UI_FONT` to a system font family name, for example `SF Pro Text`,
and `COSMIX_UI_FONT_PX` to a body size in logical pixels. These deployment
overrides apply to every CTK application and take precedence over shared and
application theme typography, including theme reloads. Restart apps after
changing their environment.

Whitespace-only family names are ignored. Sizes must parse as finite numbers
between 6 and 96 inclusive; invalid values leave the default/theme value in
effect. Missing families retain CTK's existing safe font fallback. Fonts are
resolved through the system font database; no font files are bundled.

CosMix Term uses the dark variant of its selected chrome palette. Its menu
bar uses `ctk.panel` with white titles; dropdowns use `ctk.master.panel` and
`ctk.text`. CTK's optional `CtkThemeMode` resource keeps an application's chosen
light/dark mode across reloads without replacing its typography. Split panes
have no separator gap. Only the active pane has a 1px accent border.
