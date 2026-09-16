---
scene: 1
name: conformance
citizen: conformance-citizen
window: {"kind":"edge","edge":"left","title":"Conformance","w":640,"h":480}
---
```mix
root: {widget: "column", children: ["win", "row", "field", "button", "toggle", "list", "image", "spacer"], gap: 1, padding: 2, fill: true}
win: {widget: "window", kind: "edge", edge: "left", title: "Conformance", w: 640, h: 480}
row: {widget: "row", children: ["text"], gap: 2, padding: 1, fill: true, align: "center", height: 20, radius: 2, background: "#000", hover: "#111", on_click: "pick"}
text: {widget: "text", text: "hello", size: 12, bold: true, mono: true, color: "#fff", elide: true, width: 40, fill: true, hidden: false}
field: {widget: "field", value: "", placeholder: "type", width: 100, password: false, on_change: "change", on_submit: "submit"}
button: {widget: "button", label: "Go", tone: "primary", width: 40, on_click: "go"}
toggle: {widget: "toggle", value: false, label: "On", on_change: "toggle"}
list: {widget: "list", rows: [{id: "1", cells: ["one"]}], row: "template", row_height: 24, gap: 1, max_rows: 2, fill: true, hidden_if_empty: false, on_click: "select"}
template: {widget: "row", children: ["cell"]}
cell: {widget: "text", text: "{cells[0]}"}
image: {widget: "image", src: "icon.png", w: 16, h: 16}
spacer: {widget: "spacer", size: 4}
```
