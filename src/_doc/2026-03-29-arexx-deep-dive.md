# ARexx: A Deep Dive into the Amiga's Scripting Language

## Origins

ARexx is the Amiga implementation of the REXX language, originally designed by Mike Cowlishaw at IBM in 1979. REXX (REstructured eXtended eXecutor) was intended as a scripting and macro language — a "people language" where readability and ease of use took priority over machine efficiency. William Hawes ported REXX to the Amiga as ARexx, shipping it as part of AmigaOS from version 2.0 onward (1990). What made ARexx special wasn't just the language itself — it was the deep integration with the Amiga's message-passing OS architecture via ARexx ports.

---

## Language Fundamentals

### Everything is a String

REXX's most distinctive design choice: **every value is a string**. Numbers are strings that happen to contain digits. Booleans are strings that happen to be `"1"` or `"0"`. There are no type declarations, no type annotations, no type errors. The interpreter simply applies numeric operations when the context demands it and string operations otherwise.

```rexx
x = 42          /* x is the string "42" */
y = x + 8       /* y is the string "50" — arithmetic performed on string contents */
z = x || "kg"   /* z is "42kg" — string concatenation */
```

This means you can do things that would be type errors in most languages:

```rexx
weight = "75"
label = weight "kg"     /* "75 kg" — abuttal concatenation with space */
doubled = weight * 2    /* 150 — numeric operation, result is still a string */
```

### Variables and Assignment

Variables are untyped and don't need declaration. An uninitialized variable evaluates to its own name in uppercase:

```rexx
SAY unset_var    /* prints "UNSET_VAR" */
name = "Mark"
SAY name         /* prints "Mark" */
```

Variable names are case-insensitive:

```rexx
Foo = 10
SAY foo          /* prints "10" */
SAY FOO          /* prints "10" */
```

### Compound Variables (Stems)

Stems are REXX's equivalent of arrays/dictionaries. A stem variable ends with a dot, and tail values index into it:

```rexx
/* Array-like usage */
item.0 = 3           /* convention: .0 holds the count */
item.1 = "apple"
item.2 = "banana"
item.3 = "cherry"

DO i = 1 TO item.0
   SAY item.i
END

/* Dictionary-like usage */
capital.australia = "Canberra"
capital.france = "Paris"
capital.japan = "Tokyo"

country = "australia"
SAY capital.country   /* prints "Canberra" — variable tail is resolved */
```

The tail resolution is key: `capital.country` doesn't look up the literal key "country" — it resolves the variable `country` to "australia" and then looks up `capital.australia`. This is powerful but can be surprising.

You can also initialise all elements of a stem at once:

```rexx
counts. = 0           /* every possible counts.xxx is now "0" */
counts.apples = 5     /* override one specific tail */
SAY counts.oranges    /* prints "0" — the default */
```

### Operators

**Arithmetic:** `+`, `-`, `*`, `/`, `%` (integer division), `//` (remainder), `**` (power)

**Comparison (loose — leading/trailing spaces ignored):**
`=`, `\=` (not equal), `>`, `<`, `>=`, `<=`, `><` or `<>` (not equal)

**Comparison (strict — exact string match):**
`==`, `\==`, `>>`, `<<`, `>>=`, `<<=`

**Logical:** `&` (AND), `|` (OR), `&&` (XOR), `\` (NOT, prefix)

**String concatenation:**
- Space concatenation: `"hello" "world"` → `"hello world"` (just juxtapose with a space)
- Abuttal: `"hello"||"world"` → `"helloworld"` (explicit `||` operator)
- Adjacent without space: `"count:"x` → `"count:5"` if x=5

### String Operations (Built-in Functions)

REXX has an exceptionally rich set of built-in string functions:

```rexx
/* Searching */
POS("world", "hello world")          /* 7 — position of substring */
LASTPOS("/", "path/to/file")         /* 8 */
WORDPOS("banana", "apple banana cherry")  /* 2 — word number */

/* Extraction */
SUBSTR("hello world", 7, 5)          /* "world" */
LEFT("hello", 3)                     /* "hel" */
RIGHT("hello", 3)                    /* "llo" */
WORD("one two three", 2)             /* "two" */
SUBWORD("one two three", 2, 1)       /* "two" */

/* Transformation */
REVERSE("hello")                     /* "olleh" */
UPPER("hello")                       /* "HELLO" — ARexx extension */
COPIES("ha", 3)                      /* "hahaha" */
STRIP("  hello  ")                   /* "hello" */
STRIP("  hello  ", "L")              /* "hello  " — leading only */
TRANSLATE("hello", "ABCDEFGHIJKLMNOPQRSTUVWXYZ", "abcdefghijklmnopqrstuvwxyz")
                                     /* "HELLO" — character translation */

/* Information */
LENGTH("hello")                      /* 5 */
WORDS("one two three")               /* 3 */
DATATYPE("42")                       /* "NUM" */
DATATYPE("hello")                    /* "CHAR" */

/* Formatting */
CENTER("title", 40)                  /* "title" centered in 40-char field */
RIGHT(num, 8, "0")                   /* right-justify, pad with zeros */
INSERT("***", "hello", 5)            /* "hello***" */
OVERLAY("XX", "abcdef", 3)           /* "abXXef" */
```

### PARSE — The Parsing Instruction

`PARSE` is REXX's most powerful and distinctive feature. It destructures strings into variables using a template:

```rexx
/* Simple word splitting */
PARSE VAR fullname first last
/* "Mark Smith" → first="Mark", last="Smith" */

/* Positional parsing */
PARSE VAR record 1 id 5 name 25 dept
/* columns 1-4 → id, 5-24 → name, 25+ → dept */

/* Literal delimiter parsing */
PARSE VAR datestr year "-" month "-" day
/* "2026-03-29" → year="2026", month="03", day="29" */

/* From different sources */
PARSE ARG param1, param2       /* function/program arguments */
PARSE PULL line                /* from the stack or stdin */
PARSE VALUE expr WITH template /* parse an expression */

/* Combining techniques */
PARSE VAR line word1 "=" value
/* "key=hello world" → word1="key", value="hello world" */

/* UPPER variant — fold to uppercase during parse */
PARSE UPPER VAR input cmd rest
/* "quit now" → cmd="QUIT", rest="NOW" */
```

The `PARSE` instruction replaces what would typically require regex or multiple string function calls in other languages. It's the workhorse of ARexx text processing.

---

## Control Structures

### Conditionals

```rexx
IF condition THEN
   instruction
ELSE
   instruction

/* Multi-line with DO/END */
IF count > 10 THEN DO
   SAY "Too many"
   count = 10
END
ELSE DO
   SAY "OK"
END

/* SELECT — the REXX equivalent of switch/case */
SELECT
   WHEN age < 13 THEN category = "child"
   WHEN age < 18 THEN category = "teen"
   WHEN age < 65 THEN category = "adult"
   OTHERWISE category = "senior"
END
```

### Loops

```rexx
/* Counted loop */
DO i = 1 TO 10
   SAY i
END

/* With step */
DO i = 0 TO 100 BY 10
   SAY i
END

/* Conditional loop */
DO WHILE line \= ""
   PARSE PULL line
   SAY line
END

DO UNTIL response = "YES"
   SAY "Continue? (YES/NO)"
   PULL response           /* PULL is shorthand for PARSE UPPER PULL */
END

/* Infinite loop with LEAVE */
DO FOREVER
   PULL command
   IF command = "QUIT" THEN LEAVE
   SAY "You said:" command
END

/* ITERATE — skip to next iteration */
DO i = 1 TO 100
   IF i // 3 = 0 THEN ITERATE   /* skip multiples of 3 */
   SAY i
END
```

### Subroutines and Functions

```rexx
/* Internal function */
SAY double(21)
EXIT

double: PROCEDURE
   PARSE ARG n
   RETURN n * 2

/* PROCEDURE EXPOSE — selectively share variables */
increment: PROCEDURE EXPOSE counter
   counter = counter + 1
   RETURN counter

/* Without PROCEDURE, all caller variables are visible */
noprotection:
   /* can see and modify all variables from the caller */
   RETURN

/* CALL syntax vs function syntax */
CALL myFunc arg1, arg2       /* result in special variable RESULT */
x = myFunc(arg1, arg2)       /* result as return value */
```

The `PROCEDURE` keyword creates a new variable scope. Without it, subroutines share the caller's variable pool — which is actually useful for utility routines that intentionally manipulate shared state (very ARexx-on-Amiga).

---

## ARexx-Specific Features (Beyond Standard REXX)

### The ARexx Port System

This is the heart of what made ARexx transformative on the Amiga. Applications could open named "ARexx ports" and receive commands from ARexx scripts. The inter-process communication was built on the Amiga's native message-passing (Exec message ports).

```rexx
/* Send a command to an application's ARexx port */
ADDRESS "REXXMAST"           /* target the REXX master */

/* Switch the current host to a specific application */
ADDRESS "GOLDED"             /* GoldEd text editor */
'OPEN "myfile.txt"'          /* commands sent as string literals */
'GOTO LINE 50'
'MARK BLOCK'
'GOTO LINE 75'
'CUT'

/* Multi-app orchestration — the killer feature */
ADDRESS "GOLDED"
'GETATTR TEXT VAR mytext'    /* get text from the editor */

ADDRESS "TURBOPRINT"
'PRINT' mytext               /* send it to the printer manager */

ADDRESS "DOPUS"
'LISTER NEW'                 /* open a new directory lister */
```

The `ADDRESS` instruction is the pivot point of the entire ARexx inter-app communication model. It sets the "current host" — all subsequent string expressions (in quotes) are sent to that host as commands.

### Sending and Receiving

From the script side:

```rexx
/* One-shot command to a named port */
ADDRESS "IBROWSE" 'GotoURL "http://example.com"'

/* Check if a port exists */
IF SHOW("PORTS", "GOLDED") THEN DO
   ADDRESS "GOLDED"
   'REQUEST "Editor is running!"'
END
ELSE
   SAY "GoldEd is not running"

/* Wait for a port to appear */
DO WHILE \SHOW("PORTS", "GOLDED")
   ADDRESS COMMAND "wait 1"   /* Amiga CLI wait */
END
```

### The Command Host Model

When an ARexx script ran a string expression, the flow was:

1. Script evaluates the string
2. ARexx runtime sends it as a `RexxMsg` to the named port
3. The target application receives the message
4. Application parses the command, executes it, and sends back:
   - A return code (`RC`) — 0 for success, non-zero for error
   - An optional result string (`RESULT`)
5. Script continues with `RC` and `RESULT` set

```rexx
OPTIONS RESULTS              /* tell ARexx we want RESULT strings back */

ADDRESS "GOLDED"
'GETATTR TEXT'               /* ask GoldEd for the current text */

IF RC = 0 THEN
   SAY "Text is:" RESULT
ELSE
   SAY "Error:" RC
```

The `OPTIONS RESULTS` instruction is important — without it, the host application doesn't bother packaging a result string.

### Clipboard and Stack

ARexx uses the REXX "external data queue" (stack) plus Amiga-specific clipboard integration:

```rexx
/* Stack (FIFO/LIFO queue for inter-script data passing) */
PUSH "last in, first out"    /* LIFO push */
QUEUE "first in, first out"  /* FIFO queue */
PARSE PULL item              /* pulls from stack first, then stdin */

/* Check stack depth */
IF QUEUED() > 0 THEN
   PARSE PULL nextitem
```

### Function Libraries

ARexx supports loading external function libraries (shared libraries on the Amiga):

```rexx
/* Load a function library */
IF \SHOW("LIBRARIES", "rexxsupport.library") THEN
   CALL ADDLIB("rexxsupport.library", 0, -30, 0)

/* Now functions from that library are available */
CALL DELAY(50)               /* wait 50 ticks (1 second) */
files = SHOWDIR("RAM:", "F") /* list files in RAM: */
SAY files
```

Common libraries included:
- `rexxsupport.library` — file I/O, system functions, delays
- `rexxmathlib.library` — trigonometry, advanced math
- `rexxreqtools.library` — GUI requesters (file dialogs, message boxes)
- Application-specific libraries

### GUI via rexxreqtools

```rexx
CALL ADDLIB("rexxreqtools.library", 0, -30, 0)

/* File requester */
filename = rtfilerequest("DH0:Documents", "*.txt", "Pick a file")
IF filename \= "" THEN
   SAY "You picked:" filename

/* String requester */
name = rtgetstring("", "Enter your name:", "Input")

/* Easy requester (message box) */
answer = rtezrequest("Save changes?", "_Yes|_No|_Cancel", "Confirm")
/* returns 1, 2, or 0 for the buttons */

/* Multi-line list */
choices = "Red" || "0A"x || "Green" || "0A"x || "Blue"
pick = rtezrequest(choices, "_OK", "Colours")
```

---

## Practical ARexx Script Examples

### Example 1: Batch File Renamer

```rexx
/* rename-lower.rexx — rename all files in a directory to lowercase */
PARSE ARG directory

IF directory = "" THEN directory = ""   /* current dir */

CALL ADDLIB("rexxsupport.library", 0, -30, 0)

files = SHOWDIR(directory, "F")         /* get all filenames */

DO i = 1 TO WORDS(files)
   oldname = WORD(files, i)
   newname = LOWER(oldname)             /* ARexx extension */
   IF oldname \= newname THEN DO
      ADDRESS COMMAND "Rename" directory || oldname directory || newname
      SAY oldname "→" newname
   END
END

SAY "Done."
```

### Example 2: Multi-Application Workflow

```rexx
/* report.rexx — pull data from a database, format in editor, print */
OPTIONS RESULTS

/* Step 1: Query the database */
IF \SHOW("PORTS", "FINALCALC") THEN DO
   SAY "Please start FinalCalc first."
   EXIT 10
END

ADDRESS "FINALCALC"
'GETCELL A1:A20'
IF RC \= 0 THEN DO
   SAY "Could not read spreadsheet"
   EXIT 10
END
data = RESULT

/* Step 2: Format in the text editor */
ADDRESS "GOLDED"
'NEW'                                /* new document */
'TEXT "Monthly Report"'
'NEWLINE'
'TEXT "==============="'
'NEWLINE'
'NEWLINE'

DO i = 1 TO WORDS(data)
   line = i". " WORD(data, i)
   'TEXT "'line'"'
   'NEWLINE'
END

/* Step 3: Save and print */
'SAVEAS "RAM:report.txt"'
ADDRESS "TURBOPRINT"
'PRINT "RAM:report.txt"'

SAY "Report generated and sent to printer."
```

### Example 3: Interactive Script with Error Handling

```rexx
/* backup.rexx — interactive backup with confirmation */
SIGNAL ON ERROR
SIGNAL ON BREAK_C

CALL ADDLIB("rexxreqtools.library", 0, -30, 0)

source = rtfilerequest("DH0:", , "Select source directory", "DRAWERSONLY")
IF source = "" THEN EXIT

dest = rtfilerequest("DH1:", , "Select backup destination", "DRAWERSONLY")
IF dest = "" THEN EXIT

answer = rtezrequest("Backup" source "to" dest "?", "_OK|_Cancel", "Confirm Backup")
IF answer = 0 THEN EXIT

ADDRESS COMMAND "Copy" source dest "ALL QUIET"

IF RC = 0 THEN
   CALL rtezrequest("Backup complete!", "_OK", "Success")
ELSE
   CALL rtezrequest("Backup failed! RC=" RC, "_OK", "Error")

EXIT

ERROR:
   SAY "Error" RC "at line" SIGL
   EXIT RC

BREAK_C:
   SAY "Interrupted by user."
   EXIT
```

### Example 4: ARexx as Application Glue (ImageFX + DOpus)

```rexx
/* thumbs.rexx — generate thumbnails for a directory of images */
OPTIONS RESULTS

ADDRESS "DOPUS.1"
'LISTER QUERY ACTIVE PATH'
path = RESULT

'LISTER QUERY ACTIVE SELFILES'
files = RESULT

IF files = "" THEN DO
   'REQUEST "No files selected!" OK'
   EXIT
END

/* Make output directory */
thumbdir = path || "Thumbs/"
ADDRESS COMMAND "MakeDir" thumbdir

ADDRESS "IMAGEFX.1"
DO WHILE files \= ""
   PARSE VAR files '"' filename '"' files

   'LoadBuffer "' || path || filename || '"'
   IF RC = 0 THEN DO
      'Scale 160 120'
      'SaveBufferAs JPEG "' || thumbdir || filename || '"'
      SAY "Thumbnail:" filename
   END
   ELSE
      SAY "Skipped:" filename
END

ADDRESS "DOPUS.1"
'LISTER NEW' thumbdir
'REQUEST "Thumbnails generated!" OK'
```

### Example 5: String Processing and PARSE Power

```rexx
/* parse-log.rexx — parse Apache-style log entries */
logfile = "RAM:access.log"

IF \EXISTS(logfile) THEN DO
   SAY logfile "not found."
   EXIT 5
END

CALL OPEN("log", logfile, "R")

hits. = 0
total = 0

DO WHILE \EOF("log")
   line = READLN("log")
   IF line = "" THEN ITERATE

   /* Parse: 192.168.1.1 - - [29/Mar/2026:10:15:32] "GET /index.html HTTP/1.1" 200 1234 */
   PARSE VAR line ip " - - [" timestamp "] " '"' method " " url " " protocol '"' " " status " " size

   IF DATATYPE(status, "W") THEN DO
      total = total + 1
      hits.status = hits.status + 1

      IF status >= 400 THEN
         SAY "ERROR" status ":" ip url
   END
END

CALL CLOSE("log")

SAY "Total requests:" total
SAY "200 OK:" hits.200
SAY "404 Not Found:" hits.404
SAY "500 Server Error:" hits.500
```

---

## The ARexx Execution Model

Understanding the execution model helps explain why ARexx felt the way it did:

1. **Interpreted, line-by-line** — no compilation step. You could edit a script and run it instantly. The interpreter tokenised and cached clauses internally, but the mental model was pure interpretation.

2. **Single-threaded per script** — each running script was its own process (Amiga task) with its own variable pool, but internally sequential. Concurrency came from multiple scripts running simultaneously, each in their own task.

3. **Blocking message sends** — when a script sent a command to an application port via `ADDRESS`, the script blocked until the application replied. This made orchestration scripts simple and sequential even though the underlying mechanism was asynchronous message passing.

4. **Global master process** — `RexxMast` (the REXX master) managed the ARexx port namespace, dispatched messages, and loaded/unloaded scripts. It was the system service that made everything work.

5. **No import system** — there was no module system in the modern sense. Function libraries were loaded dynamically at runtime via `ADDLIB()`. Scripts were self-contained files that could `CALL` other scripts by filename.

6. **SIGNAL for exception handling** — `SIGNAL ON ERROR`, `SIGNAL ON BREAK_C`, etc. provided structured exception handling. When triggered, control transferred to a label matching the condition name. Not try/catch, but effective.

---

## What Made ARexx Special

The language itself — standard REXX — is pleasant but unremarkable by modern standards. What made ARexx extraordinary was the **ecosystem contract**: any serious Amiga application was expected to provide an ARexx port with a comprehensive command vocabulary. This wasn't an afterthought API — it was a first-class design consideration:

- **Text editors** (GoldEd, CygnusEd, TurboText) exposed every editing operation
- **Paint programs** (ImageFX, ADPro, PPaint) exposed every filter and transformation
- **File managers** (Directory Opus) exposed lister operations, selection, and file manipulation
- **Databases** (Final Calc, TurboCalc) exposed cell operations and queries
- **Communications** (AmiTCP, Miami) exposed network operations
- **Music** (OctaMED) exposed pattern editing and playback control

The result was that a simple, readable scripting language could orchestrate arbitrarily complex multi-application workflows. A photographer could write a 20-line script that pulled filenames from Directory Opus, batch-processed them through ImageFX, and catalogued the results in a database — all without any application needing to know about the others.

This is the architectural pattern that's so hard to recapture: not just IPC, but a **cultural norm** that applications should be scriptable and composable by end users.
