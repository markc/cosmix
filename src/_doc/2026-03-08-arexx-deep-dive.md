# AmigaOS ARexx: A Deep-Dive Reference for AppMesh/Lua on Cosmic Desktop

> **Purpose:** This document is a comprehensive briefing for Claude Code — to implement ARexx-equivalent inter-application communication (IAC) in Rust + Lua on the COSMIC desktop. Every ARexx concept is described with concrete original examples, followed by the target Rust/Lua equivalent model.

---

## 1. What ARexx Actually Was — The Big Picture

ARexx (Amiga REXX, created 1987 by William S. Hawes) was the soul of AmigaOS 2.0 onward. It simultaneously served three roles:

1. **A scripting language** — an interpreted REXX dialect for writing automation scripts
2. **A system-level IPC bus** — any app could register a named "ARexx port" (a public OS message port) and receive string commands from any script or other app
3. **A universal macro language** — apps could invoke ARexx scripts to run their own internal macros, with the script automatically pre-addressed back to the calling app

The cultural rule that made it powerful: **every serious Amiga application had an ARexx port, and apps without one were not taken seriously.** This was codified in the official Amiga UI Style Guide, which said ARexx support was "about as much programming effort as supporting menus."

The result was a platform where thousands of separate commercial applications — word processors, image editors, databases, spreadsheets, comm programs, file managers — could be glued together into automated workflows by any user with a text editor.

---

## 2. The Core Architecture

### 2.1 RexxMast — The Daemon

ARexx ran a resident daemon called `RexxMast` that started at boot (added to `WBStartup` or `S:User-Startup`). Its job:

- Maintain a global list of all registered ARexx ports (application endpoints)
- Receive script execution requests (from the shell `rx` command, from apps, from icons)
- Spawn each ARexx script as its own OS task (process)
- Route string-command messages between scripts and app ports
- Manage the global Clip List (shared key/value store)

### 2.2 ARexx Ports — Named Public Message Ports

Every app that wanted to participate registered a named **public message port** with the OS. Port naming convention:

```
APPNAME.N
```

Where `N` was the instance slot number (0-based, but usually displayed as 1-based). For example:

- `IMAGEFX.1` — first running instance of ImageFX image editor
- `GOLDED.1` — first instance of GoldED text editor
- `DOPUS.1` — first instance of Directory Opus file manager
- `PAGESTREAM.1` — first instance of PageStream DTP
- `WORKBENCH` — the Workbench shell/desktop itself

Multiple instances of the same app got unique port names:
- `GOLDED.1`, `GOLDED.2`, `GOLDED.3` etc.

Port names were **always uppercase**. Apps were required to display their current port name in their "About" dialog so users could reference it in scripts.

### 2.3 The MESSAGE Model — Synchronous Request/Reply

The ARexx message flow was **synchronous by default**:

1. Script sends a string command to an app's named port (e.g., `"OPEN FILE RAM:foo.txt"` → `GOLDED.1`)
2. The OS delivers the message to the app's event loop
3. The app processes the command
4. The app replies, setting two result fields in the message struct:
   - `RC` (return code): 0=success, 5=warning, 10=error, 20=failure
   - `RESULT` (string): any data the app wants to return to the script
5. The ARexx interpreter reads `RC` and `RESULT` back into the script's variables
6. The script continues

**Scripts blocked while waiting**, but other OS tasks (including the target app's own GUI) continued running because AmigaOS was fully preemptive multitasking. An `ASYNC` keyword could make a script fire-and-forget.

---

## 3. The ADDRESS Command — Dialing Up Another App

The core ARexx keyword for IPC was `ADDRESS`. It set the "current command environment" — i.e., which named port unrecognised lines were sent to.

```rexx
/* Switch control to ImageFX image editor */
ADDRESS 'IMAGEFX.1'

/* These lines are NOT ARexx keywords — they get sent to ImageFX */
Screen2Front
LoadBrush "RAM:myimage.iff" 0
```

When ARexx's interpreter encounters a line it doesn't recognise as one of its own keywords, it forwards that entire line as a string command to the currently addressed app. The app interprets it according to its own command vocabulary.

You could switch between apps mid-script:

```rexx
/* Pull data from a database */
ADDRESS 'MICROFICHE.1'
QUERY FIELD "CustomerName" RECORD 42
customer_name = RESULT

/* Pass it to a word processor */
ADDRESS 'FINALWRITER.1'
MOVETO END
INSERT customer_name
```

---

## 4. Real-World App-Gluing Examples

### 4.1 The Classic: Database → Spreadsheet → Word Processor

This was the canonical ARexx power demo described in the official docs:

```rexx
/* Extract sales data from MicroFiche Filer database */
ADDRESS 'MICROFICHE.1'
OPENDB "Work:Databases/Sales.mfdb"
DO record = 1 TO 100
    QUERY FIELD "SalesTotal" RECORD record
    sales.record = RESULT
END

/* Pass all 100 values to Maxiplan spreadsheet */
ADDRESS 'MAXIPLAN.1'
CLEAR
DO i = 1 TO 100
    SETCELL row i col 1 VALUE sales.i
END
CALCULATE
GETCELL row 1 col 2   /* Get computed total */
total = RESULT

/* Insert table and total into Final Writer document */
ADDRESS 'FINALWRITER.1'
GOTOPAGE 3
MOVETO START
INSERTTABLE ROWS 10 COLS 2
INSERT "Total Sales: " || total
SAVEAS FILENAME "Work:Reports/SalesQ3.doc"
```

### 4.2 Communications: BBS Download → Spreadsheet

```rexx
/* Tell the telecomm app to dial, log in, download */
ADDRESS 'TERMINUS.1'
DIAL "FinancialBBS" 
WAITFOR "Login:"
SEND "myuser\n"
WAITFOR "Password:"
SEND "mypass\n"
WAITFOR "Ready>"
SEND "get stockdata.csv\n"
WAITDOWNLOAD "RAM:stockdata.csv"
DISCONNECT

/* Now hand the data to a spreadsheet */
ADDRESS 'MAXIPLAN.1'
IMPORTCSV FILE "RAM:stockdata.csv"
RUNMACRO "AnalyzeStocks"
EXPORTGRAPH FILE "RAM:stockchart.iff" FORMAT IFF
```

### 4.3 Image Processing Pipeline: Animation Builder

Wikipedia's documented use case — an animation builder that lacked image processing:

```rexx
/* Get list of raw frames */
ADDRESS COMMAND   /* AmigaDOS shell mode */
'list RAM:Frames/#?.iff to RAM:framelist.txt'

/* Process each frame through ImageFX */
ADDRESS 'IMAGEFX.1'
Screen2Front

DO WHILE ~EOF("RAM:framelist.txt")
    frame_path = READLN("RAM:framelist.txt")
    LoadBrush frame_path 0       /* load into buffer 0 */
    ApplyEffect "Sharpen"        /* ImageFX effect command */
    AdjustColor Brightness 10
    Contrast 15
    SaveBrush ("RAM:Processed/" || frame_path) IFF
END

/* Tell the animation assembler to rebuild */
ADDRESS 'ANIM8OR.1'
LOADFRAMES "RAM:Processed/"
BUILDANIM OUTPUT "Work:Final/animation.anim"
PLAY
```

### 4.4 MathScript → Final Writer: Formula Injection

A real documented example from Amiga Computing (1995) — MathScript (equation editor) had ARexx scripts to inject typeset equations into Final Writer at the correct font size:

```rexx
/* InsertFormula.rexx — run from WITHIN MathScript */
/* Get the current font size from Final Writer first */
ADDRESS 'FINALWRITER.1'
GETFONTSIZE
fw_size = RESULT

/* Tell MathScript to render at that size */
ADDRESS 'MATHSCRIPT.1'
SETSIZE fw_size
RENDER FORMAT EPS FILE "RAM:formula_temp.eps"

/* Tell Final Writer to import the rendered EPS at cursor */
ADDRESS 'FINALWRITER.1'
INSERTIMAGE "RAM:formula_temp.eps" INLINE
```

The two companion scripts did the reverse: `OpenMathScript.rexx` and `CloseMathScript.rexx` were called from Final Writer's ARexx menu.

### 4.5 Directory Opus: File Manager as Automation Hub

Directory Opus (DOpus) was the definitive ARexx power-user app. It had:

- Buttons that ran ARexx scripts
- Hotkeys bound to ARexx scripts
- Filetype actions: double-clicking an IFF file could trigger an ARexx script that opened it in the correct app
- An internal CLI to test scripts live

A DOpus button script to batch-process selected images:

```rexx
/* DOpus button: batch convert selected IFF files to PNG */
ADDRESS 'DOPUS.1'
GETSELECTED ALL    /* Returns list of selected files */
files = RESULT

ADDRESS 'IMAGEFX.1'
DO i = 1 TO WORDS(files)
    src = WORD(files, i)
    dst = CHANGEEXT(src, ".png")
    LoadBrush src 0
    SaveBrush dst PNG
    
    /* Tell DOpus to update the file listing */
    ADDRESS 'DOPUS.1'
    REFRESHLISTER
    ADDRESS 'IMAGEFX.1'  /* switch back */
END

ADDRESS 'DOPUS.1'
REQUESTNOTIFY TEXT "Batch conversion complete!"
```

### 4.6 PageStream DTP: ARexx for Publishing Automation

PageStream had one of the most comprehensive ARexx interfaces of any Amiga app. A script to auto-generate a product catalogue page:

```rexx
/* Generate catalogue page from database records */
ADDRESS 'MICROFICHE.1'
OPENDB "Work:Products.mfdb"
COUNTRECORDS
total = RESULT

ADDRESS 'PAGESTREAM.1'
NEWDOCUMENT TEMPLATE "Work:Templates/CataloguePage.pts"
SETPAGE 1

DO rec = 1 TO total
    ADDRESS 'MICROFICHE.1'
    QUERY FIELD "ProductName" RECORD rec ;  name = RESULT
    QUERY FIELD "Price" RECORD rec       ;  price = RESULT  
    QUERY FIELD "ImagePath" RECORD rec   ;  imgpath = RESULT
    
    ADDRESS 'PAGESTREAM.1'
    SELECTFRAME ("ProductName_" || rec)
    SETTEXT name
    SELECTFRAME ("Price_" || rec)
    SETTEXT ("$" || price)
    SELECTFRAME ("ProductImage_" || rec)
    IMPORTPICTURE imgpath
END

ADDRESS 'PAGESTREAM.1'
PRINT COPIES 1 COLLATE TRUE
SAVEDOCUMENT "Work:Output/CataloguePage.pts"
```

---

## 5. Apps Adding ARexx Menus at Runtime

One of ARexx's most powerful features was that apps could add a **"Macros" or "ARexx" menu** populated with scripts from a directory. This was how apps extended their own UI without developer involvement.

### 5.1 The Standard Pattern

Apps like GoldED, PageStream, Directory Opus, and Final Writer would:

1. Scan a designated directory (e.g., `REXX:GoldED/`) for `.rexx` files at startup
2. Build a menu (often called "Macros" or "ARexx Scripts") from the filenames
3. When the user selected a menu item, fire that script with the app's own port pre-set as the command environment

The user could add new scripts to the directory and they'd appear in the menu next launch. GoldED even had a "Rescan Macros" menu item to reload without quitting.

```rexx
/* A GoldED macro: selected text → ImageFX for processing as ASCII art */
/* This script lives in REXX:GoldED/TextToAsciiArt.rexx */

/* Get selected text from GoldED */
ADDRESS 'GOLDED.1'        /* pre-set by GoldED when it launched this script */
GETSELECTED
selected_text = RESULT

/* Write to temp file */
ADDRESS COMMAND
CALL OPEN f, "RAM:selected_text.txt", WRITE
CALL WRITELN f, selected_text
CALL CLOSE f

/* Call a shell tool to process it */
'asciiart RAM:selected_text.txt RAM:art_output.txt'

/* Read result and insert back into GoldED */
CALL OPEN f, "RAM:art_output.txt", READ
art_text = READALL(f)
CALL CLOSE f

ADDRESS 'GOLDED.1'
DELETESELECTED
INSERT art_text
```

### 5.2 The "Script Palette" Pattern (PageStream)

PageStream extended this with an explicit "Script Palette" — a floating window listing all macros. Users could:

- Record a sequence of GUI actions → export as ARexx script skeleton
- Add scripts to the palette
- Assign keyboard shortcuts to palette entries
- Export internal scripts to `.rx` format for external use

This was essentially a visual macro recorder that output ARexx code.

---

## 6. Data Passing Mechanisms

ARexx had multiple mechanisms for passing data between apps, ranging from simple to sophisticated.

### 6.1 RESULT Variable — Simple Return Values

Every command sent to an app could return a string in the special `RESULT` variable. The app set this before replying to the message.

```rexx
ADDRESS 'MAXIPLAN.1'
GETCELL ROW 5 COL 3
value = RESULT        /* e.g., "12345.67" */
SAY "Cell value is:" value
```

### 6.2 The Clip List — Global Key/Value Store

ARexx maintained a system-wide key/value store called the **Clip List**. Any script or app could read and write to it. It acted as a global shared memory for strings.

```rexx
/* App A publishes data */
SETCLIP("SHARED_CUSTOMER_ID", "CUS-0042")
SETCLIP("SHARED_CUSTOMER_NAME", "Acme Corp")
SETCLIP("SHARED_INVOICE_TOTAL", "1250.00")

/* App B (separate script/process) reads it */
cust_id = GETCLIP("SHARED_CUSTOMER_ID")
cust_name = GETCLIP("SHARED_CUSTOMER_NAME")
```

The Clip List was ideal for publishing "state" that multiple apps needed to read, like the currently selected record, the active filename, or a session token.

### 6.3 Files as Data Transport

For binary data (images, formatted documents, rendered output), the standard pattern was to pass **file paths** in messages, not the binary data itself:

```rexx
/* App A renders an IFF image and tells App B where to find it */
ADDRESS 'IMAGEFX.1'
output_path = "RAM:rendered_output.iff"
SAVEBRUSH output_path IFF

/* Now tell the layout app where the rendered file is */
ADDRESS 'PAGESTREAM.1'
SELECTFRAME "MainImage"
IMPORTPICTURE output_path
```

For text content, the RESULT variable could carry substantial strings. For structured data, apps would use:
- Comma-separated values in RESULT strings
- Delimited text files written to RAM: (the Amiga's RAM disk)
- The Clip List for named values

### 6.4 The External Data Stack — Pipeline Queue

ARexx's external data stack provided a FIFO queue that parent/child script processes could use:

```rexx
/* Parent script pushes work items */
PUSH "Process:file1.iff"
PUSH "Process:file2.iff"
PUSH "Process:file3.iff"

/* Child process (spawned script) pulls and processes */
DO WHILE QUEUED() > 0
    PULL work_item
    /* process work_item... */
END
```

### 6.5 Direct String Encoding for Structured Data

Apps commonly returned multiple values as a delimited string, which scripts parsed with `PARSE`:

```rexx
/* Request window dimensions from an app */
ADDRESS 'SOMEAPP.1'
GETWINDOWSIZE
dimensions = RESULT   /* Returns "640 480" */
PARSE VAR dimensions width height
SAY "Width:" width "Height:" height

/* Request a record with multiple fields */
ADDRESS 'MICROFICHE.1'
GETRECORD 42 FIELDS "Name,City,Phone"
/* Returns "Smith, John|Brisbane|555-1234" */
PARSE VAR RESULT name '|' city '|' phone
```

---

## 7. How Apps Registered Commands — The C-Side Implementation

On the application developer side, adding an ARexx port was deliberately simple. The modern AmigaOS 4.x way used BOOPSI objects:

```c
/* Define the command table */
static const struct ARexxCmd CommandTable[] = {
    { "OPEN",     cmd_Open,     "FILENAME/K,FORCE/S",  0,0,0,0 },
    { "SAVE",     cmd_Save,     NULL,                   0,0,0,0 },
    { "SAVEAS",   cmd_SaveAs,   "FILENAME/K",           0,0,0,0 },
    { "GETTEXT",  cmd_GetText,  NULL,                   0,0,0,0 },
    { "SETTEXT",  cmd_SetText,  "TEXT/A/F",             0,0,0,0 },
    { "QUIT",     cmd_Quit,     "FORCE/S",              0,0,0,0 },
    { NULL }
};

/* Create the ARexx port object at app init */
arexx_object = IIntuition->NewObject(
    NULL, "arexx.class",
    AREXX_HostName,    "MYAPP",          /* Port name */
    AREXX_Commands,    CommandTable,     /* Command table */
    AREXX_NoSlot,      FALSE,            /* Allow multiple instances */
    AREXX_ReplyHook,   &reply_hook,
    TAG_DONE
);
```

The older (OS 2.0) way used the `minrexx` library directly — about 50 lines of C for a basic port.

In the app's event loop, ARexx messages were processed alongside GUI events:

```c
/* Wait for either GUI event or ARexx message */
Wait((1L << window->UserPort->mp_SigBit) | arexx_signal_bit);

/* Dispatch any pending ARexx commands */
DispatchRexxPort();   /* calls the function registered for each command */
```

Each command handler received the raw argument string and was responsible for parsing it, performing the action, and setting the reply values.

---

## 8. The Standard ARexx Command Vocabulary

The Amiga UI Style Guide mandated a minimum set of commands every app should support, ensuring cross-app consistency:

| Command | Purpose |
|---------|---------|
| `NEW [PORTNAME port]` | Create new document; return new port name |
| `OPEN [FILENAME file] [FORCE]` | Open a file |
| `SAVE` | Save current file |
| `SAVEAS [FILENAME file]` | Save with new name |
| `CLOSE [FORCE]` | Close document |
| `QUIT [FORCE]` | Quit application |
| `CUT` | Cut selection to clipboard |
| `COPY` | Copy selection to clipboard |
| `PASTE` | Paste from clipboard |
| `UNDO` | Undo last action |
| `REDO` | Redo last undone action |
| `ACTIVATE` | Bring app to front |
| `HELP [COMMAND cmd]` | Return list of commands or help for one |
| `RX [COMMAND script]` | Execute an ARexx script from within the app |
| `FAULT errornum` | Return text for error number |

Apps were free to add hundreds of their own commands on top. PageStream had 200+. GoldED had 150+. Directory Opus had 100+.

---

## 9. Error Handling Convention

ARexx used a standardised error code system in the `RC` variable:

```rexx
ADDRESS 'GOLDED.1'
OPEN FILENAME "Work:Docs/myfile.txt"

IF RC > 0 THEN DO
    IF RC == 5 THEN SAY "Warning: " RESULT
    IF RC == 10 THEN SAY "Error: " RESULT
    IF RC == 20 THEN DO
        SAY "Fatal failure: " RESULT
        EXIT 20
    END
END
```

Standard return codes:
- `0` — Success
- `5` — Warning (e.g., user pressed Cancel)
- `10` — Error (e.g., wrong file type, locked resource)
- `20` — Failure (e.g., app crashed, resource unavailable)

---

## 10. The Workbench as an ARexx Host

Even the Workbench desktop registered an ARexx port (`WORKBENCH`) and accepted commands:

```rexx
ADDRESS 'WORKBENCH'
/* Open a drawer */
MENU WINDOW ROOT INVOKE WORKBENCH.OPENDRAWER "Work:Projects"

/* Launch an application */
MENU WINDOW ROOT INVOKE WORKBENCH.EXECUTE "Work:Apps/MyApp"

/* Trigger any menu item by path */
MENU WINDOW ROOT INVOKE WORKBENCH.ABOUT
```

This meant scripts could orchestrate the entire desktop — launching apps, managing windows, triggering any menu action — without touching the mouse.

---

## 11. Function Libraries — Extending ARexx Itself

Beyond app-to-app communication, ARexx supported **shared function libraries** (`.library` files loaded by RexxMast). These extended the ARexx language itself with new built-in functions:

```rexx
/* rexxdoslib.library — extended file/OS operations */
CALL ADDLIB 'rexxdoslib.library', 0, -30, 0

files = GETDIR("Work:Projects/")   /* new function from lib */
DO i = 1 TO WORDS(files)
    file = WORD(files, i)
    SAY FILESIZE(file)             /* another lib function */
END

/* rexxmathlib.library — math functions */
CALL ADDLIB 'rexxmathlib.library', 0, -30, 0
result = SIN(3.14159 / 2)
result2 = SQRT(144)
```

Libraries could expose C-implemented functions to ARexx scripts, making the language infinitely extensible without changing the core interpreter.

---

## 12. Practical Workflow Patterns

### 12.1 The "Watcher" Pattern

A long-running ARexx script monitors a condition and reacts:

```rexx
/* Monitor a download dir and auto-process new files */
DO FOREVER
    ADDRESS COMMAND
    'list RAM:Downloads/#?.iff to RAM:filelist.tmp'
    
    CALL OPEN f, "RAM:filelist.tmp", READ
    DO WHILE ~EOF(f)
        line = READLN(f)
        IF WORDS(line) > 0 THEN DO
            file = "RAM:Downloads/" || WORD(line, 1)
            IF ~ALREADY_PROCESSED(file) THEN DO
                CALL ProcessFile(file)
                CALL MARK_PROCESSED(file)
            END
        END
    END
    CALL CLOSE f
    
    CALL WAIT(5000)   /* check every 5 seconds */
END
```

### 12.2 The "Orchestrator" Pattern

One master script launches and coordinates multiple apps:

```rexx
/* Launch the whole production environment */
ADDRESS COMMAND
'run >NIL: Work:Apps/GoldED'
'run >NIL: Work:Apps/PageStream'
'run >NIL: Work:Apps/ImageFX'

/* Wait for all ports to register */
DO UNTIL SHOW('P', 'GOLDED.1') & SHOW('P', 'PAGESTREAM.1') & SHOW('P', 'IMAGEFX.1')
    CALL DELAY(10)   /* 10 ticks = ~0.2 sec */
END

/* Configure each app */
ADDRESS 'GOLDED.1'
OPEN FILENAME "Work:CurrentProject/article.txt"

ADDRESS 'PAGESTREAM.1'
OPEN FILENAME "Work:CurrentProject/layout.pts"

ADDRESS 'IMAGEFX.1'
LOADBRUSH "Work:Assets/header.iff" 0

SAY "Production environment ready."
```

### 12.3 The "GUI Extender" Pattern

An ARexx script adds UI behaviour an app didn't ship with:

```rexx
/* Add word-count display to GoldED via ARexx */
/* Run this from GoldED's ARexx menu or hotkey */
ADDRESS 'GOLDED.1'
GETTEXT
full_text = RESULT

word_count = WORDS(full_text)
char_count = LENGTH(full_text)
line_count = COUNTSTR(full_text, '0a'x)   /* count newlines */

/* Display via a system requester */
ADDRESS COMMAND
'requestchoice "Document Stats" "Words: ' || word_count || '~Lines: ' || line_count || '~Chars: ' || char_count || '" "OK"'
```

---

## 13. Mapping ARexx to Rust + Lua on COSMIC Desktop

This section maps every ARexx concept to the AppMesh/Lua equivalents Claude Code should implement.

### 13.1 Architecture Mapping

| ARexx Concept | AppMesh/Lua Equivalent |
|---|---|
| `RexxMast` daemon | `appmesh-daemon` Rust service (D-Bus activated) |
| ARexx port (named public message port) | D-Bus well-known name OR Unix socket endpoint registered with the daemon |
| `ADDRESS 'APPNAME.1'` | `appmesh.connect("appname.1")` in Lua OR D-Bus method call to `org.cosmic.AppMesh.AppName` |
| `RESULT` variable | Return value from Lua IPC call |
| `RC` error code | Lua error/return code from IPC call |
| Clip List (`SETCLIP`/`GETCLIP`) | appmesh-daemon's in-memory key/value store, accessible via IPC |
| External Data Stack | Named async queue managed by daemon |
| ARexx script file (`.rexx`) | Lua script file (`.lua`) in `~/.config/appmesh/scripts/` |
| `REXX:` directory | `~/.config/appmesh/scripts/` |
| `RexxMast` executing a script | Daemon spawning `lua appmeshscript.lua` as subprocess |
| Function Library (`.library`) | Lua module (`require("appmesh.imageprocessing")`) |
| App's ARexx command table | App's Lua-exposed command registry, registered with daemon at startup |

### 13.2 The Port Registration Pattern (Rust App Side)

Every app that wants to participate should call the daemon at startup:

```rust
// In the app's Rust init code
use appmesh::AppMeshClient;

let amp = AppMeshClient::connect().await?;
amp.register_port("MYAPP", &[
    ("OPEN",    open_handler),
    ("SAVE",    save_handler),
    ("GETTEXT", gettext_handler),
    ("SETTEXT", settext_handler),
    ("QUIT",    quit_handler),
]).await?;

// In the app's event loop
tokio::select! {
    msg = amp.recv() => {
        amp.dispatch(msg).await; // calls registered handler
    }
    event = cosmic_event_queue.recv() => {
        handle_gui_event(event);
    }
}
```

### 13.3 The Lua Script Pattern (Equivalent to ARexx Scripts)

```lua
-- ~/.config/appmesh/scripts/database_to_report.lua
-- Equivalent to the ARexx database→spreadsheet→wordprocessor example

local amp = require("appmesh")

-- Connect to the database app
local db = amp.connect("MICROFICHE.1")
db:send("OPENDB", {file = "~/Documents/Sales.db"})

local sales = {}
for i = 1, 100 do
    local result = db:send("QUERYFIELD", {field="SalesTotal", record=i})
    sales[i] = result.value
end

-- Pass data to spreadsheet
local sheet = amp.connect("MAXIPLAN.1")
sheet:send("CLEAR")
for i, v in ipairs(sales) do
    sheet:send("SETCELL", {row=i, col=1, value=v})
end
sheet:send("CALCULATE")
local total = sheet:send("GETCELL", {row=1, col=2}).value

-- Insert into document
local doc = amp.connect("FINALWRITER.1")
doc:send("GOTOPAGE", {page=3})
doc:send("MOVETO", {position="start"})
doc:send("INSERT", {text="Total Sales: " .. total})
doc:send("SAVEAS", {filename="~/Reports/SalesQ3.odt"})

print("Pipeline complete. Total:", total)
```

### 13.4 The ADDRESS Equivalent — Context Manager Pattern

```lua
-- Lua's cleaner version of ADDRESS switching
local amp = require("appmesh")

-- Wrap the ADDRESS pattern in a with-style call
amp.with("IMAGEFX.1", function(fx)
    fx:send("Screen2Front")
    fx:send("LoadBrush", {path="RAM:/myimage.png", buffer=0})
    fx:send("ApplyEffect", {name="Sharpen"})
    local saved_path = "/tmp/processed_output.png"
    fx:send("SaveBrush", {path=saved_path, format="PNG"})
    return saved_path
end):then_(function(saved_path)
    amp.with("PAGESTREAM.1", function(ps)
        ps:send("SelectFrame", {name="MainImage"})
        ps:send("ImportPicture", {path=saved_path})
    end)
end)
```

### 13.5 The Clip List Equivalent (Shared State Store)

```lua
-- Writing to the shared state store
local amp = require("appmesh")
amp.setclip("SHARED_CUSTOMER_ID", "CUS-0042")
amp.setclip("SHARED_INVOICE_TOTAL", "1250.00")

-- Reading from it (in another script or app)
local cust_id = amp.getclip("SHARED_CUSTOMER_ID")
```

The daemon backs this with an in-memory `HashMap<String, String>` with optional TTL, accessible to all registered apps and scripts.

### 13.6 Dynamic Menu Building (The ARexx Macro Menu)

This is critical to replicate. The pattern for COSMIC apps:

**Daemon side (Rust):** At startup, scan `~/.config/appmesh/scripts/<appname>/` for `.lua` files. Build a menu descriptor.

**App side (Rust):** Subscribe to daemon's script directory watch events. When scripts appear/disappear, rebuild the "Scripts" menu.

```rust
// App subscribes to script updates for its namespace
amp.watch_scripts("myapp", |scripts: Vec<ScriptInfo>| {
    // Rebuild the Scripts menu
    let menu = build_scripts_menu(&scripts);
    app_state.set_scripts_menu(menu);
});

// When user clicks a script menu item:
fn on_script_menu_item(script: &ScriptInfo, app_port: &str) {
    // Daemon launches the script with this app pre-addressed
    amp.run_script(&script.path, Some(app_port));
}
```

**The pre-addressing trick:** When the daemon launches a script triggered by an app's menu, it pre-sets the default `amp.connect()` target to that app's port name. This mirrors ARexx's behaviour where app-invoked macros didn't need an explicit `ADDRESS` command.

```lua
-- This script is in ~/.config/appmesh/scripts/myeditor/wordcount.lua
-- When launched from MyEditor's Scripts menu, 'amp.self()' returns 'MYEDITOR.1'

local amp = require("appmesh")
local editor = amp.self()   -- returns the calling app's port, pre-set by daemon

local text = editor:send("GETTEXT").value
local words = #vim.split(text, "%S+")  -- or a Lua word-count function
local lines = select(2, text:gsub("\n", "")) + 1

-- Show result (could use a Lua dialog or send to the app's status bar)
editor:send("SETSTATUSBAR", {text = string.format("Words: %d  Lines: %d", words, lines)})
```

### 13.7 Data Transport Patterns

```lua
-- Text data: inline in message
local result = app:send("GETSELECTION")
local text = result.text  -- string payload

-- Structured data: key/value table (maps to JSON/msgpack over socket)
local result = app:send("GETRECORD", {id=42, fields={"name","city","phone"}})
local name = result.name
local city = result.city

-- Binary data: file path reference (same as ARexx pattern)
-- DON'T send image bytes over IPC — send a path
local result = app:send("RENDERIMAGE", {width=800, height=600})
local image_path = result.path   -- "/tmp/appmesh-render-abc123.png"
-- Now a different app opens that path directly

-- Shared memory for large data (the Rust daemon owns a memfd)
local handle = amp.alloc_shared(1024 * 1024)  -- 1MB shared buffer
app_a:send("WRITERAW", {handle=handle.id, format="rgba32"})
app_b:send("DISPLAYIMAGE", {handle=handle.id, width=512, height=512})
amp.free_shared(handle)
```

### 13.8 The Orchestrator Pattern in Lua

```lua
-- Launch and coordinate the full production environment
local amp = require("appmesh")
local process = require("process")

-- Launch apps
process.spawn("cosmic-text-editor")
process.spawn("cosmic-image-editor") 
process.spawn("cosmic-page-layout")

-- Wait for ports to register (with timeout)
local function wait_for_port(name, timeout_ms)
    local deadline = os.clock() * 1000 + timeout_ms
    while os.clock() * 1000 < deadline do
        if amp.port_exists(name) then return true end
        amp.sleep(100)
    end
    return false
end

assert(wait_for_port("TEXTEDITOR.1", 5000), "Text editor didn't start")
assert(wait_for_port("IMAGEEDITOR.1", 5000), "Image editor didn't start")
assert(wait_for_port("PAGELAYOUT.1", 5000), "Page layout didn't start")

-- Configure each app
amp.connect("TEXTEDITOR.1"):send("OPEN", {file="~/Projects/article.txt"})
amp.connect("PAGELAYOUT.1"):send("OPEN", {file="~/Projects/layout.scribus"})
amp.connect("IMAGEEDITOR.1"):send("OPEN", {file="~/Assets/header.png"})

print("Production environment ready.")
```

### 13.9 App Command Table — The Rust Side

Every AppMesh-enabled app registers its command vocabulary. This is the equivalent of the ARexx command table in C:

```rust
// Macro to build the command registry cleanly
amp_commands! {
    "OPEN"     => (open_cmd,     ["FILENAME/K", "FORCE/S"]),
    "SAVE"     => (save_cmd,     []),
    "SAVEAS"   => (saveas_cmd,   ["FILENAME/K"]),
    "GETTEXT"  => (gettext_cmd,  []),
    "SETTEXT"  => (settext_cmd,  ["TEXT/A"]),
    "GETSEL"   => (getsel_cmd,   []),
    "INSERT"   => (insert_cmd,   ["TEXT/A"]),
    "QUIT"     => (quit_cmd,     ["FORCE/S"]),
    "HELP"     => (help_cmd,     ["COMMAND/K"]),
    "ACTIVATE" => (activate_cmd, []),
}

// Each handler
async fn gettext_cmd(ctx: &CmdContext, _args: CmdArgs) -> CmdResult {
    let text = ctx.app_state.get_document_text();
    CmdResult::success_with_value(text)
}
```

### 13.10 Error Code Convention (Same as ARexx)

```lua
-- Use ARexx's established error code convention
-- 0 = success, 5 = warning, 10 = error, 20 = failure

local result = app:send("OPEN", {file="~/nonexistent.txt"})
if result.rc == 0 then
    print("Opened:", result.value)
elseif result.rc == 5 then
    print("Warning:", result.message)  -- e.g., "File already open"
elseif result.rc == 10 then
    print("Error:", result.message)    -- e.g., "File not found"
elseif result.rc == 20 then
    error("Fatal: " .. result.message) -- e.g., "App crashed"
end
```

---

## 14. The SHOW() Function — Port Discovery

ARexx had `SHOW('P', 'PORTNAME')` to check if a named port was registered. AppMesh needs this:

```lua
-- Check if an app is running
if amp.port_exists("IMAGEEDITOR.1") then
    local fx = amp.connect("IMAGEEDITOR.1")
    fx:send("ACTIVATE")
else
    process.spawn("cosmic-image-editor")
    amp.wait_for_port("IMAGEEDITOR.1", 5000)
end

-- List all registered ports
local ports = amp.list_ports()
for _, port in ipairs(ports) do
    print(port.name, port.app, port.pid)
end
```

---

## 15. Key Design Principles (Learned from ARexx's Success)

These are the cultural and technical lessons from ARexx that made it work:

1. **Zero friction to add a port.** The AmigaOS style guide said ARexx support was "as much effort as supporting menus." Every Rust app must be able to add AppMesh support in under 20 lines of code.

2. **String-based commands are discoverable.** Unlike binary RPC, text commands can be typed, logged, and inspected. The `HELP` command must return a list of supported commands.

3. **Consistent command names across apps.** `OPEN`, `SAVE`, `QUIT`, `GETTEXT`, `SETTEXT` mean the same thing everywhere. Define a standard vocabulary and enforce it.

4. **Pre-addressed scripts.** When an app launches a script from its own menu, that script should not need to specify which app it's talking to — the daemon pre-wires the default connection.

5. **The Clip List is essential.** A global named string store (SETCLIP/GETCLIP) is how loosely-coupled processes share lightweight state without explicit message passing.

6. **File paths for binary data.** Never try to pass image bytes or file content through the IPC pipe. Pass the path. Let apps open the file themselves.

7. **Return codes are a contract.** 0/5/10/20 RC codes let scripts write generic error handling that works with any app.

8. **Multiple instances need unique port names.** `MYAPP.1`, `MYAPP.2` etc. The NEW command should return the new port name when spawning a new document window.

9. **Async is opt-in.** Default is synchronous (script waits for reply). Async (`ASYNC` flag) is for fire-and-forget. This keeps scripts simple.

10. **Platform social contract.** ARexx worked because it was *expected*. Every AppMesh-enabled COSMIC app should display its port name in its About dialog and document its command vocabulary.

---

## 16. Appendix: Notable ARexx-Enabled Apps Reference

| App | Port Pattern | Key ARexx Features |
|-----|--------------|--------------------|
| GoldED (text editor) | `GOLDED.1` | Full text manipulation, macro menu, cursor/selection control |
| Directory Opus (file manager) | `DOPUS.1` | Filetype actions, batch operations, lister control |
| ImageFX (image editor) | `IMAGEFX.1` | Load/save/effect/render pipeline |
| PageStream (DTP) | `PAGESTREAM.1` | Frame manipulation, text flow, print control, script palette |
| Final Writer (word processor) | `FINALWRITER.1` | Text/style/layout control, inline image import |
| MicroFiche Filer (database) | `MICROFICHE.1` | Record query/update, report generation |
| Maxiplan (spreadsheet) | `MAXIPLAN.1` | Cell get/set, formula, graph export |
| AmigaVision (multimedia) | `AMIGAVISION.1` | Can control other apps — scripted presentations |
| Workbench (desktop) | `WORKBENCH` | Menu invocation, drawer/app launching |
| Terminus (terminal/comm) | `TERMINUS.1` | Dial, send, receive, script BBS sessions |
| GnuPlot (graphing) | `GNUPLOT.1` | Graph generation, data import, format export |

---

*Document compiled from: AmigaOS Documentation Wiki, AmigaOS UI Style Guide, ARexx Wikipedia, AmiWest Lesson 8, KDE Blogs (Simon Edwards, 2005), Amiga Computing (August 1995), p.j.hutchison.org ARexx tutorial, Mikael Lundin's ARexx guide, Directory Opus 5.5 manual.*
