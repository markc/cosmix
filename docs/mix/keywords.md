# Reserved words

Mix's keyword lexemes, verified against the lexer (mix 0.21.x). Since 0.21 the
old reserved-word footgun is retired: a keyword is accepted anywhere it is
unambiguously a **name** (bare map keys, `.field` access, strict-data keys,
`send` kwargs, `parse … with` delimiters) — see *Keywords as names* below.
What remains reserved: a keyword cannot be a **function name** or fill any
other bare-identifier slot.

## The full set

The lexer reserves **46 lexemes** (`fn` and `function` share one token):

| Group | Lexemes |
|---|---|
| Conditionals & matching | `if` `then` `else` `end` `select` `when` `otherwise` |
| Loops | `for` `each` `in` `to` `step` `while` `loop` `break` `continue` `label` — plus the legacy terminators `next` (for) and `done` (while/loop/on) |
| Functions | `function` `fn` `return` |
| Errors | `try` `catch` `finally` `die` |
| Template parsing | `parse` `with` |
| Bus messaging | `send` `address` `emit` `on` |
| Output | `print` `eprint` |
| Shell & loading | `export` `alias` `source` `include` `sh` |
| Operators & literals | `and` `or` `not` `eq` `ne` `true` `false` `nil` |

`mix keywords` prints a 34-word core list; the other 12 lexemes (`then` `each`
`to` `step` `next` `done` `with` `on` `include` `label` `eq` `ne`) are just as
reserved — each fails identically as an identifier
(`function step($x)` → `Parse error … expected identifier, got Step`).

## What is (and isn't) restricted

- **Function names** — no keyword may name a function: `function step(...)`, `fn parse(...)`, even `function eq(...)` are parse errors. Pick another name (`phase`, not `step`).
- **Other bare-identifier slots** — same rule: a loop label must be a plain identifier (`while true label outer` works, `label to` is a parse error).
- **`$`-sigil variables are unaffected.** The sigil is its own namespace — the lexer reads the name after `$` raw, so `$to = 1`, `$step = 5`, `$if = 1`, and even `$fn = 1` all work, as do keyword-named parameters (`function f($to)`).
- **Contextual barewords are not reserved** — e.g. `async` (from `on <cmd> async … end`) is a valid function name.
- **Builtins are a separate rule, not keywords.** Defining a user function with a builtin's name is accepted without error, but calls dispatch to the builtin — the user function is silently unreachable (`function trim($x) … end` then `trim(" a ")` still runs the builtin).

## Keywords as names (0.21+)

A keyword is accepted wherever it is unambiguously a name:

```mix
$m = {label: 1, to: "x", on: true, parse: 2}   -- bare map keys
print($m.to)                                    -- field access -> x
$m.label = 5                                    -- field assignment
$v = data_parse("{to: 1, on: true}")            -- strict-data keys, unquoted
send maild mailbox.move to=$dest                -- send/emit/address kwargs
parse $s with $a to $b                          -- literal delimiter word
```

- **`parse … with` exception:** the block/branch terminator set — `end` `else` `catch` `finally` `when` `otherwise` `done` `next` — still ends an inline `parse` statement, so those eight can't be delimiter words.
- **The one name exception is `fn`:** it lexes to the same token as `function`, so the original spelling is unrecoverable. `{fn: 1}` and `$m.fn` are parse errors (`expected identifier, got Function`) — use a quoted key `{"fn": 1}` and index access `$m["fn"]`.
- The operator lexemes `eq`/`ne` also work as bare keys and fields (`{eq: 1}`, `$m.eq`).
- Grammar adjacency holds: `for $i = $m.to to 7` parses — dot-field consumption never crosses the following keyword.

Most keywords are covered in [syntax](syntax.md),
[control flow](control-flow.md), [functions](functions.md),
[operators](operators.md), [errors](errors.md), [Bus messaging](bus.md) and
[data](data.md) (strict-data keys).

## See also

- [syntax](syntax.md) · [functions](functions.md) · [builtins index](builtins.md) · `mix keywords`
