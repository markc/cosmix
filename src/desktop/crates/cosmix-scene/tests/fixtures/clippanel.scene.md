---
scene: 1
name: clippanel
citizen: clippanel-citizen
window: {"kind":"edge","edge":"right","title":"CosMix Clipboard","w":720,"h":520}
subscribe: ["desktop.clipboard.changed"]
targets: {"conf":"/etc/cosmix/clipboard.conf.mix"}
---
The canonical Mix Scenes v0 fixture (P1 parses/lints/diffs it, P2 renders it,
P3's citizen regenerates it). Prose outside the fence is ignored in v0.

```mix
root: { widget: "column", gap: 8, padding: 14, children: ["header", "columns", "table", "remote_title", "remote", "footer"] }
header:  { widget: "row", gap: 10, align: "center", children: ["title", "status", "spacer1", "search", "pause", "clear"] }
title:   { widget: "text", text: "CosMix Clipboard", size: 18, bold: true, color: "#e6e6e6" }
status:  { widget: "text", text: "rev 0 · 0 entries · ?", size: 12, color: "#8a8f98" }
spacer1: { widget: "spacer" }
search:  { widget: "field", value: "", placeholder: "search…", width: 170 }
pause:   { widget: "button", label: "Pause", tone: "normal", width: 64 }
clear:   { widget: "button", label: "Clear", tone: "normal", width: 58 }
columns: { widget: "row", gap: 8, children: ["c_id", "c_bytes", "c_age", "c_prev"] }
c_id:    { widget: "text", text: "ID", width: 36, size: 11, bold: true, color: "#6b7280" }
c_bytes: { widget: "text", text: "BYTES", width: 52, size: 11, bold: true, color: "#6b7280" }
c_age:   { widget: "text", text: "AGE", width: 38, size: 11, bold: true, color: "#6b7280" }
c_prev:  { widget: "text", text: "PREVIEW  (click = make the live selection)", size: 11, bold: true, color: "#6b7280", fill: true }
table: { widget: "list", row: "entry_row", row_height: 38, gap: 3, fill: true, rows: [{ id: "e1", cells: ["1", "36b", "2s", "example preview one"] }, { id: "e2", cells: ["2", "12b", "1m", "example two"] }] }
entry_row: { widget: "row", height: 38, radius: 6, padding: 10, gap: 8, background: "#20242d", hover: "#262b35", children: ["r_id", "r_bytes", "r_age", "r_prev"] }
r_id: { widget: "text", text: "{cells[0]}", width: 32, size: 13, bold: true, color: "#8fb8e8" }
r_bytes: { widget: "text", text: "{cells[1]}", width: 50, size: 12, color: "#6b7280" }
r_age: { widget: "text", text: "{cells[2]}", width: 34, size: 12, color: "#6b7280" }
r_prev: { widget: "text", text: "{cells[3]}", size: 13, mono: true, elide: true, fill: true, color: "#cfd3da" }
remote_title: { widget: "text", text: "REMOTE · (unavailable)", size: 11, bold: true, color: "#6b7280", hidden: true }
remote: { widget: "list", row: "entry_row", row_height: 30, gap: 3, max_rows: 4, hidden_if_empty: true, rows: [] }
footer: { widget: "text", text: "Click an entry to make it the live selection, then paste (Ctrl+V). Updates arrive live over the desktop.clipboard.changed Bus topic.", size: 11, color: "#6b7280" }
```
