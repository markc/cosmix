# Mix Enhancements for Cosmix AMP Scripting

**Date:** 2026-03-31
**Status:** Plan — implement in ~/.mix, then update ~/.cosmix

## Context

Cosmix is dropping TOML-based scripting in favour of Mix exclusively. Mix already has `send`, `address`, `emit`, `port_exists()`, `$rc`, `$result` — all working and wired to the cosmix hub via `cosmix-lib-script/mix_runtime.rs`. The TOML executor is being removed.

This document specifies what Mix needs to make the transition seamless.

## 1. Named args in address blocks

### The problem

The `send` statement supports named `key=value` args:

```mix
send "edit" edit.open path="/tmp/file.txt"
-- Produces AMP args: {"path": "/tmp/file.txt"}  ✓
```

But inside an `address` block, unknown function calls become sends with **positional** args:

```mix
address "edit"
    edit.open("/tmp/file.txt")
    -- Produces AMP args: {"_0": "/tmp/file.txt"}  ✗
end
```

The target app expects `{"path": "/tmp/file.txt"}`, not `{"_0": "/tmp/file.txt"}`.

### The fix

Inside an `address` block, bare-word statements should support the same `key=value` syntax as `send`:

```mix
address "edit"
    edit.open path="/tmp/file.txt"
    edit.goto line=50
end
```

This means the parser needs to recognise this pattern inside address blocks. Currently `edit.open path="/tmp/file.txt"` would be parsed as... what? The parser sees `edit.open` as an expression (dot access on `edit`), then `path=...` as an assignment.

### Implementation approach

There are two approaches:

**Option A: Statement-level pattern matching in address blocks**

When inside an `address` block (parser tracks this), and the current line starts with a bare identifier or dotted identifier followed by `key=value` pairs (not inside parens), parse it as an implicit send:

```
address_command := dotted_name (key "=" expr)*
dotted_name := IDENT ("." IDENT)*
```

This would be a new `StmtKind::AddressCommand { command: String, args: Vec<(String, Expr)> }` that the evaluator treats identically to a `send` using the current address target.

**Option B: Reuse send syntax without the target**

Inside an address block, allow `send`-style syntax with the target omitted:

```mix
address "edit"
    send edit.open path="/tmp/file.txt"    -- target comes from address
    send edit.goto line=50
end
```

This is simpler to parse (the `send` keyword triggers the existing send parser, just use the address stack for the target) but slightly more verbose.

**Recommendation:** Option A is more ARexx-like and reads better. The command name (`edit.open`) is unambiguous — it contains a dot, which no Mix variable or function name would normally have. Use the dot as the signal that this is an address command, not a variable assignment.

### Test cases

```mix
-- tests/scripts/send.mx additions

-- Test: named args in address block
address "test-port"
    greet name="hello"
end
-- Should produce send to "test-port" with command "greet" and args {"name": "hello"}

-- Test: multiple args
address "test-port"
    search query="inbox" limit=10
end
-- Args: {"query": "inbox", "limit": "10"} (or Value::Number for 10)

-- Test: variable interpolation in args
$file = "/tmp/test.txt"
address "test-port"
    open path=$file
end
-- Args: {"path": "/tmp/test.txt"}

-- Test: dotted command names
address "test-port"
    edit.open path="/tmp/file.txt"
    ui.set id="path" value="/tmp/new.txt"
end

-- Test: no-arg commands
address "test-port"
    ui.list
end
-- Args: {} (empty map)

-- Test: expression args
$base = "/tmp"
address "test-port"
    open path="${base}/file.txt" line=10+5
end
```

## 2. Cosmix prelude

### Location

`~/.config/cosmix/prelude.mx` — loaded by `cosmix-lib-script/mix_runtime.rs` before any user script executes. This is NOT the Mix standard prelude (`~/.mix/src/std/prelude.mx`) — it's cosmix-specific.

### Implementation in cosmix-lib-script

In `mix_runtime.rs`, after creating the evaluator and before executing the user script, check for and source the cosmix prelude:

```rust
// In execute_mix(), after create_evaluator():
let prelude_path = dirs_next::config_dir()
    .unwrap_or_else(|| PathBuf::from("/tmp"))
    .join("cosmix/prelude.mx");
if prelude_path.exists() {
    let prelude_src = std::fs::read_to_string(&prelude_path).unwrap_or_default();
    // Parse and execute prelude (errors are warnings, not fatal)
    if let Ok(tokens) = mix_core::lexer::Lexer::new(&prelude_src).tokenize() {
        if let Ok(stmts) = mix_core::parser::Parser::new(tokens).parse_program() {
            let _ = eval.execute(&stmts).await;
        }
    }
}
```

### Prelude contents

```mix
-- ~/.config/cosmix/prelude.mx
-- Cosmix-specific helpers loaded before every hub-connected script.

-- Launch an app if it's not already running on the hub.
-- Uses cosmix-menu if available, falls back to direct spawn.
function ensure_running($app)
    if port_exists($app) then
        return true
    end

    -- Try menu.launch first (menu knows all app binaries)
    if port_exists("menu") then
        send "menu" menu.launch app="cosmix-${app}"
        sleep 1
        return port_exists($app)
    end

    -- Fallback: direct spawn
    sh "cosmix-${app} &"
    sleep 1
    return port_exists($app)
end

-- Wait for a port to appear, with timeout in seconds.
-- Returns true if port appeared, false if timed out.
function wait_for_port($port, $timeout = 10)
    for $i = 1 to $timeout
        if port_exists($port) then
            return true
        end
        sleep 1
    next
    return false
end

-- List all services currently registered on the hub.
-- Returns the raw hub response.
function hub_services()
    send "hub" hub.list
    return $result
end
```

### No Mix changes needed

This is pure cosmix-side work. The prelude is just a `.mx` file executed by the existing evaluator.

## 3. Verify $result dot-access works end-to-end

### Current flow

1. App sends AMP response: `{"content": "# Hello World"}`
2. `HubAmpHandler::send()` receives `serde_json::Value::Object`
3. `json_to_mix()` converts to `Value::Map({"content": Value::String("# Hello World")})`
4. Evaluator sets `$result` to this Map
5. Script accesses `$result.content`

### What to verify

Does `$result.content` work as dot-access on a Map value in Mix? Test with:

```mix
send "edit" edit.get-content
print $result.content          -- should print the editor content
print type($result)            -- should print "map"
```

If this doesn't work, the issue is in mix-core's evaluator — dot access on a variable needs to resolve `$result` first, then do map lookup on `content`. This should already work since Mix supports `$config.host` style access, but verify with an actual AMP response.

### Multi-level access

Also verify:

```mix
send "edit" ui.get id="path"
-- Response might be: {"id": "path", "value": "/tmp/file.txt", "type": "input"}
print $result.value            -- should print "/tmp/file.txt"
```

## 4. What NOT to change in Mix

These are explicitly out of scope:

- **No OPTIONS RESULTS equivalent** — Mix always returns results from `send`. No opt-in needed.
- **No SIGNAL ON ERROR** — Mix has `try/catch` which is better.
- **No stack/queue (PUSH/PULL)** — Not needed. Variables and function args cover it.
- **No auto-unwrap of single-value responses** — `$result.content` is explicit and clear. Don't add magic.
- **No implicit address from shebang** — Scripts should explicitly `address` or `send`. No global default target.

## 5. Testing plan

### In Mix (unit/integration tests)

1. Named args in address blocks (new test cases in `tests/scripts/send.mx`)
2. Verify existing send tests still pass
3. Verify address blocks with no args still work

### In Cosmix (manual testing)

After both sides are updated:

1. Convert `edit/markdown-preview.toml` to `.mx`, run via User menu
2. Convert `view/edit-this-file.toml` to `.mx`, run via User menu
3. Test `ensure_running()` — kill cosmix-view, run a script that needs it
4. Test `$result.content` dot-access in a real cross-app script
5. Verify the User menu still populates correctly with only `.mx` files

## 6. Migration order

1. **Mix: implement named args in address blocks** (parser + evaluator change)
2. **Mix: add tests, build, verify** (`cargo test`)
3. **Cosmix: add cosmix prelude loading** to `mix_runtime.rs`
4. **Cosmix: create `~/.config/cosmix/prelude.mx`** with helpers
5. **Cosmix: convert 4 TOML scripts to .mx**, test each
6. **Cosmix: remove TOML support** from cosmix-lib-script (executor.rs, variables.rs, ScriptDef types)
7. **Cosmix: rebuild, test full round-trip**

Steps 1-2 are in `~/.mix`. Steps 3-7 are in `~/.cosmix`.
