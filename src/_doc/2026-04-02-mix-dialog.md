# Mix Dialog: Deep Dive & Design

## A Dioxus-Desktop GUI Dialog System for the Mix Shell

---

## Part 1: The Existing Landscape

### 1.1 The Dialog Tool Family Tree

Shell dialog tools form a clear lineage spanning four decades. Understanding this lineage reveals the common patterns, the gaps, and the design space that `mix-dialog` can occupy.

**dialog (1990s, Thomas Dickey)** — The TUI ancestor. Uses ncurses to draw terminal-mode widgets: message boxes, input boxes, checklists, radio lists, file selectors, gauges, calendars, tree views. Communicates via exit codes (0=OK, 1=Cancel, 255=ESC) and stderr for captured input. The core interaction model: spawn a process, it blocks until the user responds, the result appears on stderr or in the exit code. Every successor inherits this model.

**whiptail (Debian/Red Hat)** — A simplified `dialog` clone using newt instead of ncurses. Fewer widgets but lighter dependencies. Same interaction model: exit codes + stderr. The tool Debian's installer uses for its TUI screens. Notable for `--gauge` (progress bar fed via stdin percentage lines) and `--checklist` / `--radiolist`.

**Xdialog (2000s)** — The first GUI leap. Reimplements dialog's CLI interface but renders GTK+ windows instead of ncurses. Same flags, same semantics, GUI output. Proved the concept that the dialog CLI contract could survive a rendering backend swap.

**zenity (GNOME, 2003)** — Rewrote the dialog-for-scripts concept natively for GTK+/GNOME. Simplified the flag vocabulary compared to dialog. Core dialog types: `--info`, `--warning`, `--error`, `--question`, `--entry`, `--password`, `--file-selection`, `--color-selection`, `--calendar`, `--list`, `--progress`, `--scale`, `--text-info`, `--notification`, `--forms`. Each dialog is a single invocation with stdout/exit-code return. Written in C, uses GTK+ directly. Cross-platform (Linux, BSD, Windows via ports, macOS via Homebrew).

**kdialog (KDE, ~2004)** — The Qt/KDE equivalent of zenity. Same interaction model but with KDE-native widgets. Adds two crucial innovations that zenity lacks: (a) `--progressbar` returns a D-Bus reference that subsequent `qdbus` calls can update, creating a persistent controllable widget, and (b) `--passivepopup` for transient notifications. Dialog types: `--msgbox`, `--yesno`, `--yesnocancel`, `--warningyesno`, `--warningyesnocancel`, `--warningcontinuecancel`, `--sorry`, `--error`, `--detailederror`, `--inputbox`, `--password`, `--combobox`, `--checklist`, `--radiolist`, `--slider`, `--getopenfilename`, `--getsavefilename`, `--getexistingdirectory`, `--getopenurl`, `--getsaveurl`, `--textbox`, `--progressbar`, `--passivepopup`. The `--dontagain` flag persists a "don't show again" preference to KDE's config system — a feature no other tool offers.

**YAD — Yet Another Dialog (Victor Ananjevsky, 2008–present)** — The most feature-rich tool in the family, a fork of zenity that massively extended the concept. YAD is the high-water mark for what shell dialogs can do. Beyond zenity's types it adds: `--form` (multi-field structured input with typed field widgets — text, numeric spin, checkbox, combo, color, file, date, button, label, scale, font), `--notebook` (tabbed container that swallows other YAD instances via XEmbed/plug-socket), `--paned` (split-pane container), `--html` (embedded WebKit browser), `--picture` (image viewer with zoom/rotate), `--print` (print dialog for text/image/PDF), `--dnd` (drag-and-drop target), `--icons` (icon browser shortcut launcher), and `--notification` (system tray icon with listen-on-stdin command protocol). The notification listen mode is particularly interesting — it creates a persistent process that accepts `icon:`, `tooltip:`, `visible:`, `action:`, `menu:`, and `quit` commands on stdin, making it a scriptable system tray icon.

**EasyBashGUI** — An abstraction layer in bash that auto-selects from yad, gtkdialog, kdialog, zenity, Xdialog, gum, qarma, dialog, whiptail, or bash builtins depending on the runtime environment. Demonstrates that the dialog API can be backend-independent.

**rofi / dmenu / wofi** — A related but distinct category: launcher/menu tools that accept stdin lines and return the selected line. Not general dialog tools, but they cover the "pick from a list" use case extremely well with keyboard-driven fuzzy-matching UIs. `rofi -dmenu` with custom themes can approximate simple dialogs.

**gum (Charm, Go)** — A modern TUI dialog tool from the Charm ecosystem. Beautifully styled terminal widgets: `input`, `write` (multiline), `filter` (fuzzy select), `choose`, `confirm`, `file`, `pager`, `spin`, `table`, `log`. Demonstrates that the dialog interaction model works well with modern aesthetics. Go-based, no C dependencies.

### 1.2 The Common Contract

Every tool in this family shares a fundamental interaction contract:

```
┌─────────────────────────────────────────────────┐
│ INVOCATION                                      │
│   dialog_tool --type "prompt" [options] [items]  │
├─────────────────────────────────────────────────┤
│ LIFECYCLE                                       │
│   1. Process spawns                             │
│   2. Window/widget renders                      │
│   3. User interacts                             │
│   4. User confirms (OK/Cancel/button)           │
│   5. Process exits                              │
├─────────────────────────────────────────────────┤
│ OUTPUT                                          │
│   exit code  → decision (0=OK, 1=Cancel, etc.)  │
│   stdout     → selected value(s)                │
│   stderr     → (dialog/whiptail only) values    │
├─────────────────────────────────────────────────┤
│ EXCEPTIONS                                      │
│   --progressbar → persistent process + D-Bus    │
│   --notification → persistent process + stdin   │
└─────────────────────────────────────────────────┘
```

This contract maps perfectly to shell command substitution: `$result = $(dialog_tool --entry "Name?")` captures the value, `$?` captures the decision. The two exceptions (progress bar and notification) require persistent handles, which kdialog solves with D-Bus references and YAD solves with stdin command streams.

### 1.3 Comprehensive Widget Taxonomy

Collating every dialog type across all tools produces 28 distinct widget categories:

**Message/Alert dialogs** — info, warning, error, sorry, question, yesno, yesnocancel, warningyesno, warningcontinuecancel, detailederror (with expandable detail text), passivepopup (transient notification toast)

**Input dialogs** — text entry (with optional default), password entry, multiline text input, combobox (dropdown select), editable combobox, checklist (multi-select with keys), radiolist (single-select with keys), slider/scale (numeric range), spin button (numeric with step)

**Chooser dialogs** — file open (single), file open (multiple), file save, directory select, color chooser, font chooser, calendar/date picker

**Display dialogs** — text info viewer (scrollable text, optional filename/URI source, optional checkbox "I have read"), image/picture viewer, HTML viewer (embedded WebKit)

**Progress dialogs** — single progress bar (fed via stdin percentage lines or D-Bus), multi-progress (multiple named bars), spinner/indeterminate

**Container dialogs** — notebook (tabbed, swallows child dialogs via XEmbed), paned (split view, two child dialogs), form (multi-field structured input)

**System integration** — notification/tray icon (persistent, stdin-controllable), drag-and-drop target, print dialog, icon/application launcher grid

### 1.4 What's Missing: The Gap Analysis

Despite the richness of YAD and kdialog, the existing tools share several limitations:

**No structured data return.** Everything comes back as flat strings with configurable separators. A form with 5 fields returns `"value1|value2|value3|value4|value5"` and the script must split and index manually. There's no JSON, no key-value pairs, no typed returns.

**No bidirectional live communication.** Except for kdialog's D-Bus progress bars and YAD's notification stdin protocol, there's no way for the script to update a dialog's content while it's open or receive events as they happen. The model is fire-and-wait.

**No composability.** YAD's notebook and paned dialogs attempt composition but rely on XEmbed socket/plug IPC between separate processes, which is fragile (segfaults on nesting, per GitHub issues) and X11-specific (doesn't work on Wayland without XWayland).

**No mesh/network awareness.** Every tool is purely local. There's no concept of showing a dialog on a remote machine, no addressability for the dialog within a larger system, no way to integrate dialog interactions into a distributed workflow.

**No WASM target.** All existing tools depend on system toolkits (GTK+, Qt, ncurses). None can run in a browser or in a WASM sandbox.

**No style/theme control beyond system defaults.** The dialog looks like whatever GTK/Qt theme is active. Scripts can't specify colors, fonts, or layouts beyond the fixed widget types.

---

## Part 2: mix-dialog Design

### 2.1 Architecture: The AMP-Native Dialog Server

`mix-dialog` is not just a CLI tool — it's an AMP service that exposes dialog widgets as addressable ports within the Cosmix mesh. The binary operates in three modes:

```
┌───────────────────────────────────────────────────────────┐
│                      mix-dialog                           │
│                                                           │
│  Mode 1: CLI (kdialog-compatible)                        │
│    $ mix-dialog --entry "Name?" --title "Input"          │
│    → stdout + exit code                                  │
│                                                           │
│  Mode 2: Mix builtin (embedded in mix interpreter)       │
│    $name = dialog entry "What is your name?"             │
│    → direct Value return, no subprocess                   │
│                                                           │
│  Mode 3: AMP service (persistent, addressable)           │
│    dialog.main.node1.amp                                 │
│    → accepts AMP messages, responds with results         │
│    → multiple concurrent dialogs, live updates           │
│    → mesh-addressable from any node                      │
└───────────────────────────────────────────────────────────┘
```

The rendering engine is **dioxus-desktop** (WebKitGTK on Linux). This gives us full HTML/CSS/JS rendering power inside a native window, with Rust logic running natively (not in WASM). For the WASM target, the same Dioxus components render in a browser via dioxus-web.

### 2.2 The Crate Structure

```
cosmix/crates/
  mix-dialog/
    Cargo.toml
    src/
      lib.rs              — Public API (DialogRequest, DialogResult)
      types.rs            — Dialog type definitions, field specs
      protocol.rs         — AMP message encoding/decoding
      render/
        mod.rs            — Dioxus component registry
        message.rs        — Info/warning/error/question dialogs
        input.rs          — Entry, password, multiline
        choice.rs         — Combobox, checklist, radiolist
        form.rs           — Multi-field form dialog
        file.rs           — File/directory/save choosers (rfd integration)
        color.rs          — Color picker
        calendar.rs       — Date/time picker
        progress.rs       — Progress bar (single/multi)
        scale.rs          — Slider/range
        table.rs          — Data table with sort/filter
        text_viewer.rs    — Scrollable text/code viewer
        notebook.rs       — Tabbed container
        wizard.rs         — Multi-step wizard flow
        notification.rs   — Toast/notification
        custom.rs         — User-provided HTML/Dioxus template
      window.rs           — Window creation, sizing, positioning
      theme.rs            — Theming engine (CSS custom properties)
      cli.rs              — CLI argument parsing (compat layer)
      amp.rs              — AMP service mode
    tests/
```

### 2.3 Core Data Model

```rust
/// What the caller sends
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DialogRequest {
    /// Dialog type and parameters
    pub kind: DialogKind,
    /// Window title
    pub title: Option<String>,
    /// Window dimensions (auto-sized if None)
    pub size: Option<(u32, u32)>,
    /// Window position (centered if None)
    pub position: Option<(i32, i32)>,
    /// Theme overrides
    pub theme: Option<ThemeOverrides>,
    /// Timeout in seconds (0 = no timeout)
    pub timeout: u32,
    /// AMP return address (for async/mesh mode)
    pub reply_to: Option<String>,
    /// Whether dialog is modal to parent window
    pub modal: bool,
    /// Custom icon (path or theme name)
    pub icon: Option<String>,
}

/// What comes back
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DialogResult {
    /// The user's decision
    pub action: DialogAction,
    /// Returned data (structured, not flat strings)
    pub data: DialogData,
    /// AMP return code (0=ok, 1=cancel, 5=timeout, 10=error)
    pub rc: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DialogAction {
    Ok,
    Cancel,
    Yes,
    No,
    Custom(String),    // For user-defined buttons
    Timeout,
    Error(String),
}

/// Structured return data — the key improvement over flat strings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DialogData {
    None,
    Text(String),
    Number(f64),
    Bool(bool),
    FilePath(PathBuf),
    FilePaths(Vec<PathBuf>),
    Color(String),         // #RRGGBB
    Date(String),          // ISO 8601
    Selection(Vec<String>),
    Form(IndexMap<String, String>),  // field_name → value
    Table(Vec<IndexMap<String, String>>), // rows of key-value
}
```

### 2.4 The Dialog Types

#### Tier 1: Essential (MVP)

These map 1:1 to the most-used kdialog/zenity/YAD types:

```rust
pub enum DialogKind {
    // === Messages ===
    Message {
        text: String,
        level: MessageLevel,     // Info, Warning, Error
        detail: Option<String>,  // Expandable detail text
    },
    Question {
        text: String,
        yes_label: Option<String>,
        no_label: Option<String>,
        cancel: bool,            // Show cancel button?
    },

    // === Input ===
    Entry {
        text: String,
        default: Option<String>,
        placeholder: Option<String>,
    },
    Password {
        text: String,
    },
    TextInput {
        text: String,
        default: Option<String>,
        syntax: Option<String>,  // Syntax highlighting lang
    },

    // === Selection ===
    ComboBox {
        text: String,
        items: Vec<String>,
        default: Option<usize>,
        editable: bool,
    },
    CheckList {
        text: String,
        items: Vec<ListItem>,    // (key, label, checked)
    },
    RadioList {
        text: String,
        items: Vec<ListItem>,
    },

    // === Choosers ===
    FileOpen {
        filters: Vec<FileFilter>,
        directory: Option<String>,
        multiple: bool,
    },
    FileSave {
        filters: Vec<FileFilter>,
        directory: Option<String>,
        default_name: Option<String>,
    },
    DirectorySelect {
        directory: Option<String>,
    },

    // === Progress ===
    Progress {
        text: String,
        pulsate: bool,           // Indeterminate mode
        auto_close: bool,
    },

    // === Composite ===
    Form {
        text: String,
        fields: Vec<FormField>,
    },
}
```

#### Tier 2: Extended

```rust
    // === Choosers (continued) ===
    ColorChooser {
        default: Option<String>,
    },
    Calendar {
        text: String,
        default_date: Option<String>,
        format: Option<String>,
    },
    Scale {
        text: String,
        min: f64,
        max: f64,
        step: f64,
        default: Option<f64>,
    },

    // === Display ===
    TextViewer {
        source: TextSource,      // File path, URL, or inline text
        checkbox: Option<String>, // "I have read and agree" text
        syntax: Option<String>,
    },
    Table {
        columns: Vec<ColumnDef>,
        rows: Vec<Vec<String>>,
        selectable: bool,
        multi_select: bool,
        editable: bool,
        sortable: bool,
        filterable: bool,
    },

    // === Containers ===
    Notebook {
        tabs: Vec<TabDef>,       // Each tab contains a DialogKind
    },
    Wizard {
        steps: Vec<WizardStep>,  // Sequential pages with validation
    },

    // === System ===
    Notification {
        text: String,
        timeout: u32,            // Auto-dismiss seconds
        icon: Option<String>,
        actions: Vec<String>,    // Clickable action buttons
    },
```

#### Tier 3: Cosmix-Specific

```rust
    // === AMP-Aware ===
    ServiceBrowser {
        filter: Option<String>,  // Service name filter
    },
    NodeSelector {
        mesh: String,            // Mesh network to browse
    },
    PortMonitor {
        ports: Vec<String>,      // AMP addresses to watch
        live: bool,              // Real-time updates
    },

    // === Custom ===
    Custom {
        html: String,            // Custom HTML template
        script: Option<String>,  // Custom JS
        style: Option<String>,   // Custom CSS
    },
    DioxusComponent {
        component: String,       // Registered component name
        props: IndexMap<String, String>,
    },
}
```

### 2.5 Form Fields

The form dialog is where `mix-dialog` most surpasses the existing tools. Each field is typed and returns structured data:

```rust
pub struct FormField {
    pub id: String,              // Return key name
    pub label: String,           // Display label
    pub kind: FieldKind,
    pub required: bool,
    pub help: Option<String>,    // Tooltip/help text
    pub width: Option<String>,   // CSS width
}

pub enum FieldKind {
    Text { default: Option<String>, placeholder: Option<String>, max_length: Option<usize> },
    Password,
    Number { default: Option<f64>, min: Option<f64>, max: Option<f64>, step: Option<f64> },
    Toggle { default: bool, on_label: Option<String>, off_label: Option<String> },
    Select { items: Vec<String>, default: Option<usize> },
    MultiSelect { items: Vec<String>, defaults: Vec<usize> },
    Date { default: Option<String>, format: Option<String> },
    Time { default: Option<String> },
    Color { default: Option<String> },
    File { filters: Vec<FileFilter> },
    Slider { min: f64, max: f64, step: f64, default: Option<f64> },
    TextArea { default: Option<String>, rows: usize },
    Label { text: String },      // Non-interactive, just display
    Separator,                   // Horizontal rule
    Hidden { value: String },    // Passes through without display
}
```

### 2.6 Mix Language Integration

This is where the ARexx heritage pays off. Dialog invocations in Mix are language primitives, not function calls:

```mix
-- Simple message (blocks, returns DialogAction in $rc)
dialog info "Backup complete" title="Success"

-- Question (returns 0 for yes, 1 for no)
dialog question "Delete ${filename}?" yes="Delete" no="Keep"
if $rc == 0 then
    rm $filename
end

-- Entry (returns text in $result)
$name = dialog entry "What is your name?" default="Mark"

-- Password
$pass = dialog password "Enter sudo password:"

-- File chooser
$file = dialog file_open filter="Mix Scripts:*.mx" dir=$HOME

-- Combo box
$color = dialog combo "Pick a color:" items=["Red","Green","Blue"]

-- Checklist (returns list)
$packages = dialog checklist "Install which packages:" \
    items=["nginx:Web server:on", "redis:Cache:off", "pg:Database:on"]

-- Scale/slider
$volume = dialog scale "Volume:" min=0 max=100 step=5 default=75

-- Calendar
$date = dialog calendar "Select date:" format="%Y-%m-%d"

-- Progress (returns handle for updates)
$prog = dialog progress "Installing..." pulsate=false
for each $i in range(1, 100)
    send $prog set value=$i text="Step ${i}/100"
    sleep 0.1
next
send $prog close
```

#### Forms — Structured Input

```mix
$result = dialog form "New VPS Configuration" fields=[
    { id: "hostname", label: "Hostname", kind: "text", required: true },
    { id: "os",       label: "OS",       kind: "select",
      items: ["CachyOS", "Alpine", "Debian", "Ubuntu"] },
    { id: "ram",      label: "RAM (GB)", kind: "number",
      min: 1, max: 128, step: 1, default: 4 },
    { id: "disk",     label: "Disk (GB)", kind: "slider",
      min: 10, max: 1000, step: 10, default: 50 },
    { id: "ssh_key",  label: "SSH Key",  kind: "file",
      filter: "SSH Keys:*.pub" },
    { id: "headless", label: "Headless", kind: "toggle", default: true },
]

if $rc == 0 then
    print "Creating VPS: ${result.hostname}"
    print "OS: ${result.os}, RAM: ${result.ram}GB"
    send "vps.create.node1" \
        hostname=$result.hostname \
        os=$result.os \
        ram=$result.ram \
        disk=$result.disk
end
```

#### Wizard — Multi-Step Flows

```mix
$config = dialog wizard "Setup Wizard" steps=[
    {
        title: "Welcome",
        text: "This wizard will configure your Cosmix node.",
        fields: [
            { id: "node_name", label: "Node Name", kind: "text", required: true }
        ]
    },
    {
        title: "Network",
        fields: [
            { id: "mesh",     label: "Mesh Network", kind: "select",
              items: ["home.amp", "office.amp", "cloud.amp"] },
            { id: "wg_port",  label: "WireGuard Port", kind: "number",
              default: 51820, min: 1024, max: 65535 },
        ]
    },
    {
        title: "Services",
        fields: [
            { id: "services", label: "Enable Services", kind: "multiselect",
              items: ["jmap", "web", "dns", "vps", "amp-relay"] }
        ]
    }
]
```

#### Notebook — Tabbed Settings

```mix
dialog notebook "Settings" tabs=[
    {
        label: "General",
        icon: "preferences-system",
        dialog: { kind: "form", fields: [
            { id: "theme", label: "Theme", kind: "select",
              items: ["dark", "light", "auto"] },
            { id: "font_size", label: "Font Size", kind: "number",
              default: 14, min: 8, max: 32 },
        ]}
    },
    {
        label: "Network",
        icon: "network-wired",
        dialog: { kind: "form", fields: [
            { id: "proxy", label: "Proxy", kind: "text" },
            { id: "timeout", label: "Timeout (s)", kind: "number", default: 30 },
        ]}
    }
]
```

### 2.7 AMP Integration: Dialogs as Addressable Ports

This is the key innovation. In Cosmix, everything is addressable. A dialog is no different.

```
┌─────────────────────────────────────────────────────────┐
│ AMP Address: dialog.{instance}.{node}.amp               │
│                                                         │
│ Commands:                                               │
│   show     → Display a dialog (DialogRequest in body)   │
│   update   → Update a live dialog (progress, table)     │
│   close    → Close a dialog by handle                   │
│   query    → Get current state of a dialog              │
│   list     → List open dialogs                          │
│                                                         │
│ Events (emitted):                                       │
│   dialog.opened   → {handle, kind}                      │
│   dialog.closed   → {handle, action, data}              │
│   dialog.changed  → {handle, field_id, value}           │
│   dialog.action   → {handle, button_id}                 │
└─────────────────────────────────────────────────────────┘
```

#### Remote Dialog Scenario

A Mix script running on `node2` can show a dialog on `node1`:

```mix
-- On node2: ask the operator sitting at node1 to confirm
send "dialog.main.node1" show kind="question" \
    text="node2 wants to restart nginx. Allow?" \
    yes="Allow" no="Deny"

if $result == "yes" then
    sh "systemctl restart nginx"
end
```

#### Live Updates Scenario

A progress dialog that updates in real-time from a long-running operation:

```mix
-- Show progress, get handle back
emit "dialog.main.local" show kind="progress" \
    text="Migrating mailboxes..." handle="migrate-01"

-- In the migration loop
for each $i, $mbox in $mailboxes
    $pct = ($i / len($mailboxes)) * 100
    emit "dialog.main.local" update handle="migrate-01" \
        value=$pct text="Migrating ${mbox}..."
next

emit "dialog.main.local" close handle="migrate-01"
```

#### Live Table Scenario

A monitoring dashboard as a dialog:

```mix
-- Show a live-updating table of AMP ports
$handle = dialog table "Active AMP Ports" \
    columns=["Port", "Node", "Status", "Uptime", "Messages"] \
    live=true sortable=true filterable=true

-- Background: feed data into the table
loop
    $ports = $(mix ports --json)
    send $handle set rows=$ports
    sleep 5
done
```

### 2.8 AMP Wire Format for Dialogs

Dialog requests and responses use the standard AMP markdown-frontmatter format:

```markdown
---
command: show
from: script.batch.node2.amp
to: dialog.main.node1.amp
id: msg-0042
---
{
  "kind": {
    "Question": {
      "text": "Restart nginx on node1?",
      "yes_label": "Restart",
      "no_label": "Cancel",
      "cancel": false
    }
  },
  "title": "Service Restart",
  "modal": true,
  "timeout": 60
}
```

Response:

```markdown
---
command: reply
from: dialog.main.node1.amp
to: script.batch.node2.amp
in-reply-to: msg-0042
rc: 0
---
{
  "action": "Yes",
  "data": "None",
  "rc": 0
}
```

### 2.9 CLI Compatibility Layer

For scripts that just want kdialog/zenity semantics, `mix-dialog` supports a compatibility CLI:

```bash
# kdialog-compatible
mix-dialog --msgbox "Hello"
mix-dialog --yesno "Continue?"
mix-dialog --inputbox "Name:" "default"
mix-dialog --password "Secret:"
mix-dialog --combobox "Pick:" "A" "B" "C"
mix-dialog --checklist "Select:" "1" "Alpha" on "2" "Beta" off
mix-dialog --getopenfilename "$HOME" "*.mx"
mix-dialog --progressbar "Working..." 100
mix-dialog --slider "Value:" 0 100 10

# zenity-compatible
mix-dialog --info --text="Done"
mix-dialog --entry --text="Name?"
mix-dialog --file-selection --multiple
mix-dialog --list --column="Name" --column="Size" "a.txt" "42"

# mix-dialog native (JSON return)
mix-dialog --json --form \
    --field="name:text" \
    --field="os:select:CachyOS,Alpine,Debian" \
    --field="ram:number:4:1:128" \
    --title="New VPS"
# Returns: {"name":"myhost","os":"Alpine","ram":8}
```

The `--json` flag switches output from flat strings to structured JSON — the key improvement for script consumption.

### 2.10 Theming

Dioxus-desktop renders via WebKitGTK, so theming is CSS. `mix-dialog` ships a default theme based on COSMIC desktop aesthetics but allows overrides:

```mix
-- Inline theme override
dialog info "Styled!" theme={
    bg: "#1a1a2e",
    fg: "#e0e0e0",
    accent: "#7c3aed",
    font_family: "JetBrains Mono",
    font_size: "14px",
    border_radius: "8px"
}

-- Theme file
dialog info "Themed!" theme_file="~/.mix/themes/cyberpunk.json"
```

The theme engine uses CSS custom properties, so the Dioxus components reference `var(--dialog-bg)`, `var(--dialog-fg)`, etc. COSMIC desktop detection auto-applies the system's cosmic-theme colors.

### 2.11 WASM Considerations

`mix-dialog` is split into two crates:

`mix-dialog-core` — The type definitions (`DialogRequest`, `DialogResult`, `DialogKind`, etc.), the AMP protocol encoding, and the Dioxus components. No system dependencies. Compiles to `wasm32-unknown-unknown`.

`mix-dialog` — The CLI binary, AMP service mode, window management (dioxus-desktop), and native file dialogs (rfd integration). Depends on system libraries, does not compile to WASM.

For WASM targets, `mix-dialog-core` provides the same Dioxus components that render in a browser. The dialog appears as a modal overlay in the web UI rather than a native window. The AMP protocol works identically over WebSocket transport.

### 2.12 Implementation Priority & Milestones

**Phase 1 — Foundation (2-3 weeks)**
Build the `DialogRequest`/`DialogResult` types, the Dioxus rendering framework, and the first 5 dialog types: message (info/warning/error), question (yes/no), entry, password, and file chooser (via rfd). CLI compatibility for these types. Basic theme support.

**Phase 2 — Input & Selection (2 weeks)**
ComboBox, checklist, radiolist, scale/slider, calendar. Form dialog with all field types. JSON output mode.

**Phase 3 — AMP Integration (2 weeks)**
AMP service mode. Dialog addressability. Progress bar with live updates via AMP messages. Remote dialog invocation. Handle-based dialog management.

**Phase 4 — Containers & Advanced (2 weeks)**
Notebook (tabbed), wizard (multi-step), table (sortable/filterable), text viewer. Notification toasts.

**Phase 5 — Mix Language Integration (1 week)**
`dialog` keyword in the Mix parser/evaluator. Command substitution integration. Form result destructuring.

**Phase 6 — WASM & Custom (ongoing)**
WASM build of `mix-dialog-core`. Custom HTML template dialogs. DioxusComponent registration for user-defined widgets.

---

## Part 3: Best Practices & Patterns

### 3.1 The Golden Rules (Learned from 30 Years of Shell Dialogs)

**Rule 1: Exit codes are sacred.** 0 = user confirmed, 1 = user cancelled, 2+ = error/timeout/other. Every dialog must set `$rc` or the process exit code. Scripts rely on `if dialog ... then` patterns — breaking exit code semantics breaks every script.

**Rule 2: Stdout is for data, stderr is for diagnostics.** The result value goes to stdout (or `$result` in Mix). Error messages and debug info go to stderr. Never mix them. This is what makes command substitution `$x = $(dialog entry "?")` work.

**Rule 3: Auto-size by default, override when needed.** Dialogs should size themselves to fit their content. Only override with `--width` / `--height` when the default is wrong. Never require the caller to specify dimensions for simple dialogs.

**Rule 4: Keyboard-navigable.** Tab between fields. Enter to confirm. Escape to cancel. Arrow keys for lists. These are non-negotiable accessibility requirements that also make dialogs usable without a mouse.

**Rule 5: Structured returns for structured input.** If the dialog collects structured data (a form), the return should be structured (JSON/map). Flat delimited strings are a legacy compromise, not a feature.

### 3.2 AMP-Specific Patterns

**Pattern: Fire-and-acknowledge.** For non-blocking notifications:
```mix
emit "dialog.main.local" show kind="notification" \
    text="Build complete" timeout=5
-- Script continues immediately, notification auto-dismisses
```

**Pattern: Request-reply.** For blocking user input:
```mix
send "dialog.main.local" show kind="question" text="Continue?"
-- Script blocks until user responds, result in $result/$rc
```

**Pattern: Handle-based lifecycle.** For long-lived dialogs:
```mix
send "dialog.main.local" show kind="progress" text="Working..."
-- $result contains the handle ID
$handle = $result
-- ... do work, send updates ...
send "dialog.main.local" update handle=$handle value=50
-- ... more work ...
send "dialog.main.local" close handle=$handle
```

**Pattern: Remote confirmation.** For operations requiring operator approval:
```mix
address "dialog.main.operator-node"
    send show kind="question" \
        text="node3 wants to scale to 16 VPS instances. Approve?" \
        timeout=300  -- 5 minute timeout
end
select $rc
    when 0 then proceed_with_scaling()
    when 5 then die "Operator did not respond within 5 minutes"
    otherwise die "Operator denied: rc=${rc}"
end
```

### 3.3 CSS Widget Addressability via AMP DNS Hierarchy

Every widget inside a dialog is addressable using the AMP DNS-style path convention, which enables the compositor and shell to target specific UI elements:

```
ok-btn.question-01.dialog.main.node1.amp     -- The OK button
name-field.form-02.dialog.main.node1.amp     -- A specific form field
tab-network.settings.dialog.main.node1.amp   -- A notebook tab
```

This means the cosmix-shell can script widget state changes from Mix:

```mix
-- Disable the submit button until form is valid
send "submit-btn.config-form.dialog.main.local" set enabled=false

-- Change a field's value programmatically
send "hostname.config-form.dialog.main.local" set value="new-host"

-- Focus a specific field
send "password.login-form.dialog.main.local" focus
```

This is the direct descendant of ARexx's ability to send commands to specific UI elements within applications — now extended across a mesh network.

---

## Part 4: Reference — Existing Tools Comparison Matrix

| Feature | dialog | whiptail | zenity | kdialog | YAD | gum | **mix-dialog** |
|---------|--------|----------|--------|---------|-----|-----|----------------|
| Rendering | ncurses | newt | GTK+ | Qt/KDE | GTK+ | TUI | Dioxus/WebKitGTK |
| WASM | No | No | No | No | No | No | **Yes** (core) |
| Structured returns | No | No | No | No | No | No | **Yes** (JSON/Map) |
| Form dialog | No | No | --forms | No | --form | No | **Yes** (typed fields) |
| Tabbed/Notebook | No | No | No | No | --notebook | No | **Yes** (composable) |
| Wizard | No | No | No | No | No | No | **Yes** |
| Live updates | gauge/stdin | gauge/stdin | progress/stdin | D-Bus | stdin | No | **AMP messages** |
| Remote display | No | No | No | No | No | No | **Yes** (mesh) |
| Widget addressability | No | No | No | No | No | No | **Yes** (AMP DNS) |
| Custom theming | No | No | GTK theme | KDE theme | GTK theme | Lipgloss | **CSS custom props** |
| Bidirectional IPC | No | No | No | D-Bus (limited) | stdin (limited) | No | **Full AMP** |
| Embeddable | No | No | No | No | No | No | **Yes** (lib crate) |
| Shell integration | stderr | stderr | stdout | stdout | stdout | stdout | **language primitive** |
| HTML/custom UI | No | No | No | No | --html (WebKit) | No | **Yes** (Dioxus) |
| Table with sort/filter | No | No | --list | No | --list | table | **Yes** (full) |

---

## Part 5: Summary

`mix-dialog` sits at the intersection of three design lineages:

1. **The dialog/zenity/kdialog/YAD tradition** — proven widget taxonomy, CLI contract, shell integration patterns. We keep the interaction model (spawn → render → interact → return) and the widget vocabulary, but fix the structured-return and composability gaps.

2. **The ARexx/AMP tradition** — addressable services, message-based IPC, scriptable UI elements. Dialogs become first-class AMP citizens, addressable from anywhere in the mesh, with live bidirectional updates via the same protocol that every other Cosmix component uses.

3. **The Dioxus/Rust/WASM tradition** — native performance, cross-platform rendering via WebKitGTK, WASM compilation for browser targets, no C dependencies in the core crate, Cargo ecosystem integration.

The result is a dialog system where `$name = dialog entry "What is your name?"` is as natural as `$name = readline("What is your name?")`, but behind that simple surface lies a mesh-addressable, live-updatable, structured-data-returning, WASM-portable dialog engine that no existing tool can match.
