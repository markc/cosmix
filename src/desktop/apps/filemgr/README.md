# CosMix FileMgr

CosMix FileMgr is the native CTK/Bevy twin-pane file manager for CosMix
Desktop. It is distinct from the `filesd` storage domain and exposes its
semantic app controls over Bus.

Its stable component slug is `filemgr`; the binary is `cosmix-filemgr`, the
native app id is `dev.cosmix.filemgr`, and runtime state lives below
`cosmix/apps/filemgr`. See [the desktop registry](../../APPS.md).

The current local read/write slice has two independently navigable panes, lazy
folder expansion, Places, selection information, Lucide file-type icons, and
the reusable CTK Dual Carousel Sidebars (DCS) shell. Each DCS sidebar can float
over the file panes or pin beside them. Selected files and folders can be
copied or moved to the opposite pane from the toolbar or right-click menu;
drag-and-drop offers Move/Copy/Cancel, and permanent deletion requires an
explicit confirmation.

File operations never silently overwrite an existing destination. Copy and
move run off the UI thread; recursive directory deletion is only started from
the confirmation dialog.

```sh
cd ~/.cos/desktop
cargo run -p cosmix-filemgr
```

Current scope:

- asynchronous local directory listings with stale-result rejection
- lazy, in-place folder expansion through the reusable CTK TreeView component
- independent left and right browser paths
- editable location bars plus Back, Forward, Home, parent and Places navigation
- sortable Name, Size and Modified columns
- per-pane hidden-file visibility
- keyboard selection and navigation
- draggable 10–100% width sidebars that independently float or pin
- draggable twin-pane divider; double-click returns to an exact 50/50 split
- DCS panel carousel controls
- curated Lucide SVG assets installed below the app's runtime root
- native session persistence in `cosmix/apps/filemgr/config/config.conf.mix`

Keyboard shortcuts:

- `Up`/`Down`, `Home`/`End` — move the row selection
- `Enter` — open the selected folder or submit an edited location
- `Backspace` — parent directory
- `Alt+Left`/`Alt+Right` — pane history
- `Ctrl+H` — show or hide dotfiles in the active pane
- `F6` — switch the active pane

Previews, bookmarks, large-directory virtualisation and Bus mesh access
remain follow-on work (copy/move/rename/delete and drag-and-drop shipped —
see above).
