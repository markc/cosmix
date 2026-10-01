# Changelog

## 0.72.4

- Composite client Wayland drag artwork, including its subsurfaces, above
  desktop windows and layers while it follows the human pointer. Honour
  committed surface offsets, buffer scales, transforms and viewports.
- Keep drag artwork out of input targeting and remove it on drop, cancellation,
  source destruction, icon destruction and seat teardown.
