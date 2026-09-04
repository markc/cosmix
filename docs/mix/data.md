# data — data & serialization

Mix reads and writes structured data through a single mental model: **every
format parses to the same `Value` tree** (string · number · bool · list · map ·
nil), and every encoder turns that tree back into text. There is one in-memory
shape; the format is just the doorway. So `json_parse` → walk with `.field` /
`[i]` → `json_encode` (or `toml_encode`, or `data_encode`) is the universal
round-trip.

List the category live with `mix builtins json`; one-line help for any name with
`mix what NAME`.

| Format | Text → Value | Value → text | File reader |
|---|---|---|---|
| JSON | `json_parse` | `json_encode` | `read_json`, `read_jsonl` |
| TOML | `toml_parse` | `toml_encode` | — (use `read_file` + `toml_parse`) |
| YAML | `yaml_parse` | `yaml_encode` | — (use `read_file` + `yaml_parse`) |
| strict-data `.mix` | `data_parse` | `data_encode` | `load_data` |
| CSV | `csv_parse` | — | — |
| INI | `ini_parse` | — | — |
| XML | `xml_parse` | — | — |
| jq query | `jq`, `jq_all` (over a `Value`) | — | — |
| numeric coercion | `to_number` | — | — |

`json_parse`/`json_encode`, `jq`/`jq_all`, `read_json`/`read_jsonl` are gated
behind the `json` feature, `toml_parse`/`toml_encode` behind `toml`,
`yaml_parse`/`yaml_encode` behind `yaml`, and `xml_parse` behind `xml`; the shipped `mix` binary turns them all on.
`data_parse`/`data_encode`, `load_data`, `csv_parse`, `ini_parse`, `to_number`,
and `template` are core — always present, no feature gate.

This page also covers [`template`](#template--filling-text-from-a-map)
(placeholder substitution from a map) and the typed
[`sqlexec` binds](#sqlexec--typed-sql-binds) — the other two places Mix values
cross a text/SQL serialization boundary.

## JSON

`json_parse(string)` turns a JSON document into a Mix value; field access is
`.key` for objects, `[i]` for arrays. `json_encode(value[, pretty])` is the
inverse.

```mix
$m = json_parse("{\"name\":\"alpha\",\"port\":25,\"tags\":[\"a\",\"b\"]}")
print($m.name)
print($m.port)
print($m.tags[0])
```

```text
alpha
25
a
```

```mix
$m = { name: "alpha", port: 25, on: true }
print(json_encode($m))
```

```text
{"name":"alpha","on":true,"port":25}
```

> Since 0.21 a Mix **keyword works as a bare map key** — `on: true`, `to: "x"`,
> `label: 1` all parse unquoted. The one exception is `fn` (it lexes identically
> to `function`): write `{ "fn": … }`. Keys with a dash/dot/space still need
> quotes. See [maps](collections.md).

A truthy second argument selects multi-line indented output:

```mix
$m = { name: "alpha", nested: { a: 1, b: 2 } }
print(json_encode($m, true))
```

```text
{
  "name": "alpha",
  "nested": {
    "a": 1,
    "b": 2
  }
}
```

### Key ordering — the one JSON footgun

A Mix `Map` is insertion-ordered, but the JSON doorway is not — **in either
direction**. `json_encode` goes through `serde_json` **without** `preserve_order`,
so it emits keys **sorted alphabetically**; `json_parse` returns object keys
sorted the same way, **not** in document order. If byte-stable, order-preserving
output matters (a generated config a human will diff, a signed canonical form),
use [`data_encode`](#strict-data-mix--the-substrate-format) instead — it
preserves insertion order.

```mix
$m = { zebra: 1, apple: 2, mango: 3 }
print("literal:     " .. ("" .. $m))   -- map literal: insertion order
print("data_encode: " .. data_encode($m))
print("json_encode: " .. json_encode($m))
print("json_parse:  " .. data_encode(json_parse("{\"zebra\":1,\"apple\":2}")))
```

```text
literal:     {zebra: 1, apple: 2, mango: 3}
data_encode: {"zebra": 1, "apple": 2, "mango": 3}
json_encode: {"apple":2,"mango":3,"zebra":1}
json_parse:  {"apple": 2, "zebra": 1}
```

### JSON value mapping

| JSON | Mix |
|---|---|
| `null` | `nil` |
| `true` / `false` | bool |
| number | `Number` (f64) |
| string | `String` |
| array | `List` |
| object | `Map` (`IndexMap`) |

```mix
$m = json_parse("{\"a\":null,\"b\":3.5,\"c\":1000000000000}")
print($m.a == nil)
print($m.b)
print($m.c)
```

```text
true
3.5
1000000000000
```

Mix numbers are f64, so an integer above 2^53 loses exact precision on the way
through — a documented constraint, not a bug (see [numbers](numbers.md)). JSON
object keys must be strings, and they come back **sorted**, not in document
order (§above).

### What JSON can't represent

`json_encode` is **loud on non-finite numbers**: a `NaN` / `±inf` anywhere in
the value raises a catchable error (JSON has no spelling for them — encoding
`0` silently would corrupt data):

```mix
try
  print(json_encode({ bad: sqrt(-1) }))
catch $e
  print("" .. $e)
end
```

```text
json_encode: non-finite number NaN has no JSON representation (JSON has no NaN/Infinity)
```

A **function** or **bytes** value inside the tree encodes as `null` — JSON
simply has no such type:

```mix
print(json_encode({ f: function($x) return $x end, s: "ok" }))
```

```text
{"f":null,"s":"ok"}
```

`data_encode` is stricter on both counts — it **raises** on functions and bytes
too (§strict-data below).

### Reading JSON from disk

`read_json($path)` is `read_file` + `json_parse` in one call (one record per
file). `read_jsonl($path[, opts])` reads a **JSON-lines** file — one independent
record per line — into a list, skipping blank lines.

```mix
-- /tmp/rec.json  = {"name":"alpha","port":25,"tags":["a","b"]}
$r = read_json("/tmp/rec.json")
print($r.name)
print($r.tags[1])
```

```text
alpha
b
```

```mix
-- /tmp/recs.jsonl =
--   {"id":1,"ok":true}
--   {"id":2,"ok":false}
--   {"id":3,"ok":true}
$recs = read_jsonl("/tmp/recs.jsonl")
print(length($recs))
print($recs[0].id)
print($recs[2].ok)
```

```text
3
1
true
```

`read_jsonl` is **strict by default** — one malformed line aborts the whole read
with a line number. Pass `{ skip_errors: true }` to silently drop bad lines
(useful for a log whose rotated tail can be truncated):

```mix
-- /tmp/bad.jsonl has a junk middle line
$strict = read_jsonl("/tmp/bad.jsonl")            -- raises: "... line 2: ..."
$lenient = read_jsonl("/tmp/bad.jsonl", { skip_errors: true })
print(length($lenient))                            -- the two good records
```

```text
2
```

### Parse errors raise

`json_parse` / `read_json` / `read_jsonl` raise a catchable runtime error on
malformed input (the same fail-fast policy as the rest of Mix — a broken literal
is a script bug, not absent data). Wrap with [`try`/`catch`](errors.md):

```mix
try
  $m = json_parse("{oops")
catch $e
  print("bad json: " .. ("" .. $e))
end
```

```text
bad json: json_parse: key must be a string at line 1 column 2
```

## jq — querying a Value

`jq(value, filter)` and `jq_all(value, filter)` run a real [jq](https://jqlang.github.io/jq/)
filter (embedded [jaq](https://github.com/01mf02/jaq) engine) over any Mix value
— typically `json_parse` output, but any list/map tree works. The `value` is a
**Mix value, not a JSON string**: `json_parse` is the text→value door, `jq` is the
query door — any list/map literal works too, no JSON round-trip needed. Design
notes: the jq-builtin design doc (operator control repo since 2026-07-23).

The split is about **output cardinality**, the thing jq makes hard for a
single-return language:

- **`jq(value, filter)`** — for a filter that yields **0 or 1** output. 0 → `nil`, 1 → that value unwrapped. A filter that returns an *array as one value* (`map(.x)`, `[ .a[] ]`) belongs here — it's one output. **More than one output raises.**
- **`jq_all(value, filter)`** — for a **stream** filter (`.items[]`, `.[] | select(…)`). **Always a list**: 0 outputs → `[]`, N → `[v0, …]`. You can always `for each` it.

```mix
$m = json_parse("{\"a\":{\"b\":{\"c\":42}}}")
print(jq($m, ".a.b.c"))
```

```text
42
```

```mix
$m = json_parse("{\"items\":[{\"n\":\"x\"},{\"n\":\"y\"}]}")
print(jq($m, "[ .items[].n ]"))      -- bracketed: one array output -> jq()
```

```text
[x, y]
```

```mix
$m = json_parse("{\"items\":[{\"n\":\"x\",\"sz\":50},{\"n\":\"y\",\"sz\":150}]}")
$big = jq_all($m, ".items[] | select(.sz > 100) | .n")   -- stream -> jq_all()
print($big)
```

```text
[y]
```

A stream filter handed to `jq()` raises with the fix spelled out:

```mix
$m = json_parse("[1,2,3]")
print(jq($m, ".[]"))
```

```text
Runtime error at line 2: jq(): filter produced more than one output; use jq_all(), or wrap the filter as `[ … ]` / `map(...)` / add `first` to make it a single value
```

### Absent data vs. errors

A valid filter over absent data is **not** an error — `.missing` → jq `null` →
Mix `nil` for `jq()`, and an empty stream → `[]` for `jq_all()`. There is no
`default` argument; use jq's own alternative operator `//`:

```mix
$m = json_parse("{\"a\":1}")
print(jq($m, ".missing // \"none\""))   -- absent -> the alternative
$r = jq($m, ".nope")
if $r == nil then print("got nil") end
```

```text
none
got nil
```

A **malformed filter** or a **jq runtime type error** (e.g. indexing a number)
raises, like `json_parse` on bad JSON:

```mix
$m = json_parse("{\"a\":1}")
print(jq($m, ".a |"))
```

```text
Runtime error at line 2: jq(): cannot parse filter ".a |": Parse([(Term, "")])
```

> jq (the embedded jaq engine) **preserves** object key insertion order — `jq($m,
> ".")` returns keys in their original order, and constructed objects like `jq($m,
> "{b: .b, a: .a}")` keep the written order. The alphabetical look in JSON examples
> above comes from `json_parse`/`json_encode`, not from jq. For canonical,
> order-stable serialization reach for `data_encode`.

## YAML (v0.71.0)

`yaml_parse(string[, {docs: true}])` → value; `yaml_encode(value)` → a YAML
string. This is the surface the provisioned-YAML fleet runs on — Grafana
alerting/dashboards/datasources, Prometheus and Alloy config that deploy
scripts generate, edit, and **gate on**: a deploy script that cannot parse
what it deploys cannot validate it either (this used to shell out to
`ruby -ryaml`).

```mix
$y = yaml_parse(read_file("rules.yaml"))
for each $g in $y["groups"]
  print($g["name"] .. ": " .. len($g["rules"]) .. " rule(s)")
end
write_file("rules.yaml", yaml_encode($y))
```

The mapping mirrors JSON's: scalars → nil/bool/number/string, sequences →
lists, mappings → maps in document order. The details that differ from a
naive reading:

- **Documents.** By default the input must hold at most one document —
  none is `nil`, more than one **raises** (naming the fix). Pass
  `{docs: true}` for a list of every document in the stream.
- **Keys.** Mix maps are string-keyed: scalar YAML keys keep their natural
  text (`25:` becomes `"25"`); a sequence or mapping used as a key raises.
- **Anchors and aliases** are resolved during parsing; tags are not
  preserved. **Merge keys are NOT resolved**: yaml-rust2 has no `<<`
  handling, so `<<: *defaults` parses as a literal `"<<"` entry whose
  inherited fields are *not* flattened into the map — a gating script
  must flatten them itself or avoid `<<` in files it validates.
  `.inf`/`.nan`-style reals that Rust cannot parse keep their text as
  strings rather than being guessed at.
- **Encoding** emits no leading `---` marker, guarantees a trailing
  newline, writes whole numbers as integers, and quotes only what YAML
  requires — including keys like `on`/`yes` that YAML 1.1 would read as
  booleans. `nil`/bool/number/string/list/map only; bytes, buffer or
  function values raise — and so does a **non-finite number** (bare
  `inf`/`NaN` would read back as *strings*; `json_encode` and
  `toml_encode` refuse the same way).
- **Nesting is capped at 256 levels** in both directions (a hostile
  deep-nesting input raises a catchable error, like `serde_json`'s 128).
- Round trip holds: `yaml_parse(yaml_encode($v))` rebuilds the same value.

## TOML

`toml_parse(string)` → map; `toml_encode(value)` → a pretty TOML string. Tables
become nested maps; a whole-valued number encodes as a TOML integer, a fractional
one as a float.

```mix
$t = toml_parse("name = \"alpha\"\nport = 25\n\n[server]\nhost = \"192.0.2.1\"")
print($t.name)
print($t.port)
print($t.server.host)
```

```text
alpha
25
192.0.2.1
```

```mix
$m = { name: "alpha", port: 25, server: { host: "192.0.2.1" } }
print(toml_encode($m))
```

```text
name = "alpha"
port = 25

[server]
host = "192.0.2.1"
```

There is no `read_toml`; compose it — `toml_parse(read_file($path))`. TOML
datetimes parse to a string.

TOML has no representation for Mix `nil`, function, `bytes`, or `buffer`
values. `toml_encode` therefore raises `TOML_UNREPRESENTABLE` instead of
silently replacing any of them with `""`. The structured error's `details`
map carries the exact `path` and `type`, including list indexes:

```mix
try
  print(toml_encode({jobs: [{payload: freeze(buffer([1, 2]))}]}))
catch $message, $error
  print($error.code)
  print($error.details.path)
  print($error.details.type)
end
```

```text
TOML_UNREPRESENTABLE
$.jobs[0].payload
bytes
```

Convert deliberately before encoding: `base64_encode($bytes)` for binary
payloads, omit an absent field rather than storing `nil`, and never put a
function in a data tree.

## strict-data `.mix` — the substrate format

Mix has its own JSON-equivalent data format: a `.conf.mix` / `.spec.mix` file is a
JSON-shaped tree with a friendlier surface — bareword keys, optional top-level
braces, `#` comments, trailing commas, `nil`. Crucially it **parses only as
data**: no `$vars`, no `$(cmd)`, no `${...}`, no function calls, no control flow.
It is the format for substrate-internal config that must never run as code.

- `data_parse(string)` — text → value (the non-executing parser).
- `data_encode(value[, pretty])` — value → text, with correct escaping; **round-trips** through `data_parse` and **preserves key order**.
- `load_data($path)` — read + `data_parse` a file (the non-executing twin of [`source`/`include`](functions.md), which *run* a file).

Literal heredocs are accepted as strings, including their normal trailing-newline
semantics. A heredoc containing `${...}` interpolation or `$(...)` command
substitution is rejected like every other executable strict-data construct.

```mix
$cfg = { name: "alpha", port: 25, tags: ["a", "b"] }
print(data_encode($cfg))
print(data_encode($cfg, true))   -- truthy 2nd arg = multi-line
```

```text
{"name": "alpha", "port": 25, "tags": ["a", "b"]}
{
  "name": "alpha",
  "port": 25,
  "tags": [
    "a",
    "b"
  ]
}
```

`data_encode` emits exactly the escaping the strict-data lexer round-trips —
`\$`, `\"`, `\\`, `\n`, `\t`, `\r`, `\e`; any other control character as
`\u{…}`; and a leading `\~` when (and only when) the string is `~` or starts
with `~/`, the two shapes the lexer would expand to `$HOME` — so a string that
ends in `$` (a regex) or contains a backslash survives intact. This is what
makes it **safe to splice hostile values**
into a generated config or a remote script: the value arrives as inert data, never
as a command. (It is **not** a shell escaper — don't feed its output to `/bin/sh`.)

```mix
$v = { rx: "ends-with$", win: "a\\b" }
print(data_encode($v))
```

```text
{"rx": "ends-with\$", "win": "a\\b"}
```

The round-trip is exact for any data-shaped tree:

```mix
$orig = { name: "alpha", regex: "x$", tags: ["a", "b"], n: 7 }
$back = data_parse(data_encode($orig))
print($back.regex)
print($back.tags[1])
print($back.n)
```

```text
x$
b
7
```

`data_parse` accepts the canonical no-braces top-level map (newline- or
comma-separated `key: value`):

```mix
$d = data_parse("name: \"alpha\"\nport: 25\nenabled: true")
print($d.name)
print($d.enabled)
```

```text
alpha
true
```

Strict-data does **not** accept executable Mix's `;` statement separator — use
newlines or commas. `data_parse("a: 1; b: 2")` raises a line-numbered
strict-data violation rather than quietly widening the configuration format.

…and **rejects every executable construct** with a line-numbered error — that
rejection is the whole point:

```mix
print(data_parse("x: now()"))
```

```text
Runtime error at line 1: data_parse: Strict-data violation at line 1: function call `now(...)` not allowed in data files. data files cannot invoke functions; use a literal value
```

Keys that are Mix **keywords** need no quoting (since 0.21): a `.conf.mix` key
named `to`, `on`, `in`, or `label` parses bare. The one exception is `fn` — it
lexes identically to `function`, so quote it (`"fn": 1`).

`load_data` is the file-reading form, for data the Mix tooling and a Rust
consumer should read through the one same parser (e.g. a mesh inventory):

```mix
-- /tmp/inv.mix (strict-data, NOT a script):
--   name: "mesh-inventory"
--   nodes: [
--     { host: "node1", addr: "192.0.2.1" },
--     { host: "node2", addr: "192.0.2.2" },
--   ]
$inv = load_data("/tmp/inv.mix")
print($inv.name)
print($inv.nodes[0].host)
print($inv.nodes[1].addr)
```

```text
mesh-inventory
node1
192.0.2.2
```

**Every way `load_data` can fail is catchable** (since 0.36.1) — missing file,
unreadable file, syntax error in the data, or an executable construct the
strict-data rules reject. All four arrive as ordinary runtime errors naming the
path, so a `try/catch` around a config read actually holds:

```mix
$cfg = nil
try
  $cfg = load_data("/etc/cosmix/app.conf.mix")
catch $e
  print("cannot read config: " .. $e)
  exit(2)
end
```

> Before 0.36.1 a *malformed* file raised a raw parse error, and
> [`try/catch`](errors.md) deliberately does not catch those — the script
> aborted with exit 1 straight through the handler above, while a *missing*
> file was caught normally. If you rely on that `catch`, check
> `mix --version`.

> `data_encode` cannot represent a non-finite number (`NaN`/`±inf`), a function
> value, or a bytes value — it **raises** a catchable error rather than emit
> silently-lossy text (where `json_encode` degrades functions/bytes to `null`,
> §above).

## CSV & INI — text tables and config

`csv_parse(string[, delim])` reads the **first line as a header** and returns a
list of maps keyed by those headers. Fields are trimmed. The optional second
argument is a one-character delimiter (default `,`).

```mix
$rows = csv_parse("name,port\nalpha,25\nbeta,143")
print($rows[0].name)
print($rows[1].port)
print(length($rows))
```

```text
alpha
143
2
```

```mix
$rows = csv_parse("a|b\n1|2", "|")
print($rows[0].a)
print($rows[0].b)
```

```text
1
2
```

Three sharp edges: the parser is a **naive split** — no RFC-4180 quote handling,
so a quoted field containing the delimiter splits in two (`"x,y"` becomes the
fields `"x` and `y"`); only the **first character** of the delimiter argument is
used; and a row with more fields than headers keys the extras by 0-based field
index (`col1`, `col2`, …). Blank lines are skipped.

`ini_parse(string)` reads `[section]` headers and `key = value` lines into a
nested map. Lines starting `#` or `;` are comments. Keys before any section land
under `_global`.

```mix
$ini = ini_parse("debug = true\n[server]\nhost = 192.0.2.1\nport = 25\n[log]\nlevel = info")
print($ini["_global"].debug)
print($ini.server.host)
print($ini.server.port)
print($ini.log.level)
```

```text
true
192.0.2.1
25
info
```

> **Every CSV / INI value is a string** — even `25` and `true`. These parsers do
> no type inference (a value with leading zeros, a hostname, and a port look the
> same as text). Coerce numerics yourself with `to_number` (next section). JSON,
> TOML, and strict-data preserve the types they can represent; each serializer
> documents what happens when a Mix-only type reaches its boundary. TOML and
> strict-data raise rather than substituting a different value.

## XML — SOAP/RSS documents

`xml_parse(string[, {mode}])` parses **strict XML** into a Value tree. The
default `simple` mode is shaped for consuming SOAP/RSS-style API responses:
namespace prefixes are stripped, attributes become `@name` keys, repeated
sibling elements collapse into a list, a leaf element's trimmed text becomes
its value, mixed text lands under `#text`, and `xmlns` declarations are
dropped. The result is `{RootName: …}`, so a SOAP response navigates as plain
map access:

```mix
$r = xml_parse("<e:Envelope xmlns:e=\"urn:x\"><e:Body><status>OK</status><ns><s>ns1.example.net</s><s>ns2.example.net</s></ns></e:Body></e:Envelope>")
print($r.Envelope.Body.status)
print($r.Envelope.Body.ns.s[1])
print(length($r.Envelope.Body.ns.s))
```

```text
OK
ns2.example.net
2
```

Attribute and mixed-text keys start with `@`/`#`, so they need bracket access:

```mix
$a = xml_parse("<a id=\"7\">hi &amp; bye</a>")
print($a.a["@id"])
print($a.a["#text"])
```

```text
7
hi & bye
```

Pass `{mode: "tree"}` for full fidelity: every element is a
`{name, attrs, children}` node with namespace prefixes and `xmlns` attributes
preserved and child order intact; text children appear as plain strings
(whitespace-only text between elements is dropped in both modes).

```mix
$t = xml_parse("<ns:a x=\"1\">t<b/>u</ns:a>", {mode: "tree"})
print($t.name)
print($t.attrs.x)
print($t.children[0])
print($t.children[1].name)
```

```text
ns:a
1
t
b
```

Sharp edges: **strict XML only** — real-world HTML is tag soup and will not
parse (that needs an HTML5 parser, deliberately not this builtin). Every parsed
value is a **string** (like CSV/INI — coerce with `to_number`). Simple mode is
**lossy by design**: ordering across differently-named siblings, mixed-content
interleaving, and namespace identity are all dropped — reach for `{mode:
"tree"}` when they matter. A single child element is a scalar and two-plus
collapse to a list, so a maybe-repeated element needs a `type()` check (or
navigate the tree mode instead). Only predefined (`&amp;` `&lt;` `&gt;`
`&apos;` `&quot;`) and numeric character entities resolve — no DTD, so custom
entities error. Input must be UTF-8 (a `bytes` payload from `http_get` is
accepted and decoded strictly). Nesting is capped at 256 levels; malformed
input, mismatched/unclosed tags, multiple roots, and text outside the root all
error rather than best-guess.

## to_number — numeric coercion

`to_number(value)` returns the value as a `Number`, or `nil` if it can't coerce.
It accepts a number, a numeric string (surrounding whitespace trimmed), or a bool
(`true`→1, `false`→0); anything else → `nil`. This is the bridge from
string-typed CSV/INI/form data back to arithmetic.

```mix
print(to_number("3.14"))
print(to_number("  42  "))
print(to_number(true))
print(to_number("nope"))
print(to_number([1, 2]))
```

```text
3.14
42
1
nil
nil
```

Coercion is **strict about non-finite spellings**: `"inf"`, `"infinity"`,
`"nan"`, and an overflowing `"1e999"` all return `nil` — a word the Rust float
parser happens to accept is not a Mix numeric string. `is_number` agrees on the
string case. Number **values** still propagate IEEE-754 non-finites through
math (`sqrt(-1)` is `NaN`) — it is the string doorway that's strict. Ordinary
scientific notation is fine (`to_number("1e6")` → `1000000`). See
[numbers](numbers.md).

```mix
print(to_number("inf"))
print(to_number("1e999"))
print(is_number("nan"))
print(sqrt(-1))
```

```text
nil
nil
false
NaN
```

The `nil`-on-failure return is the validation idiom — branch instead of crashing:

```mix
$rows = csv_parse("name,port\nalpha,25")
$p = to_number($rows[0].port)
if $p == nil then
  print("port is not a number")
else
  print($p + 1)
end
```

```text
26
```

## template — filling text from a map

`template(tmpl, map)` substitutes single-brace `{key}` placeholders (not
`{{key}}`) with the map's values, stringified the way `print` would. The second
argument must be a map — anything else raises.

```mix
print(template("{name} is a {role}", { name: "alpha", role: "relay" }))
print(template("{n} items, ok={ok}", { n: 3, ok: true }))
```

```text
alpha is a relay
3 items, ok=true
```

Three rules make it safe over untrusted values:

- **Single-pass**: a substituted value is emitted verbatim and never rescanned, so data containing `{other_key}` cannot inject a second substitution — and the output never depends on map iteration order:

```mix
print(template("{a} and {b}", { a: "{b}", b: "SECRET" }))
```

```text
{b} and SECRET
```

- A `{key}` not present in the map, a `{` with no closing `}`, and a nested `{` all stay **literal** — no error, no silent empty string.
- Matching is **byte-exact** — no Unicode normalization, no case-folding (see [strings](strings.md)).

## sqlexec — typed SQL binds

The embedded SQLite builtins (`sqlite` feature; on in the shipped binary) are
the other place Mix values cross a serialization boundary. `sqlopen(path)`
opens **read-only**; `sqlopen(path, "rw")` opens read-write (WAL mode, 5s busy
timeout). `sqlexec(handle, sql[, params])` executes with `?` placeholders;
`sqlclose(handle)` closes.

Parameters bind **typed** — never stringified:

| Mix value | SQLite bind |
|---|---|
| `nil` | `NULL` |
| bool | `INTEGER` `0`/`1` |
| whole finite number (i64 range) | `INTEGER` |
| any other number (fractional, huge) | `REAL` |
| string | `TEXT` |
| bytes | `BLOB` |
| list / map / function | **loud error** — no SQL representation |

`params` is a list (one element per `?`) or a single bare value. A non-reading
statement (`INSERT`/`UPDATE`/`CREATE` …) returns `{affected: N}`; a
row-returning statement (`SELECT`, a row-returning `PRAGMA` — decided by SQLite
itself, not a keyword match) returns a **list of maps**, mapping back `NULL` →
`nil`, `INTEGER`/`REAL` → number, `TEXT` → string, and a `BLOB` → the
placeholder string `"<blob N bytes>"`.

```mix
$db = sqlopen(":memory:", "rw")
sqlexec($db, "create table t (a, b, c)")
print(data_encode(sqlexec($db, "insert into t values (?, ?, ?)", [nil, true, 2.5])))
print(data_encode(sqlexec($db, "select typeof(a) as ta, typeof(b) as tb, typeof(c) as tc from t")))
sqlclose($db)
```

```text
{"affected": 1}
[{"ta": "null", "tb": "integer", "tc": "real"}]
```

Binds are the **exact-bytes** path — prefer them over composing SQL text with
`sql_quote` (which escapes for a string literal and doubles a backslash on the
way into SQLite; see [system](system.md)).

## Boundary validation (v0.29.0)

Mix's tolerant `nil` semantics stay the default everywhere — a missing map key
reads as `nil`, and general string coercion renders that as `"nil"`, which is
exactly how a blank provisioning field once became part of a constructed
hostname. The validation family makes strictness a one-call choice at your
job/API/form boundary; failures raise structured `VALIDATION_*` errors with
`details {path, expected, actual_type}` (see [errors](errors.md)):

```mix
$job = validate($raw, {
  node: {type: "string", nonblank: true},
  host: {type: "string", nonblank: true},
  vmid: {type: "integer", min: 100, max: 999999},
  plan: {enum: ["gold", "silver"]},
  tags: {required: false, type: "list", items: {type: "string", nonblank: true}},
  owner: {required: false, type: "map", schema: {name: {nonblank: true}}}
})
```

- `validate(value, spec)` — validates a map against a field spec, returns the ORIGINAL map unchanged (composable; no hidden normalization). Rules per field: `required` (default TRUE), `type` (one name or a list — `any nil bool number integer string bytes buffer list map function`; `integer` = finite whole within ±2^53-1), `nonblank`, `enum` (normal Mix equality, so `"8080"` matches `8080`), `min`/`max` (inclusive numeric), `min_length`/`max_length` (string codepoints / list items / map entries), `items` (rule map for every list element), `schema` (nested field spec). Optional fields that are absent or nil skip their rules; unknown INPUT fields pass through; a typo'd RULE key raises `VALIDATION_SPEC` instead of silently no-opping. Violation paths read `owner.name`, `tags[2]`.
- `require_key(map, key)` — assert present + non-nil, return the value (`VALIDATION_REQUIRED`).
- `expect_type(value, kind)` — assert the type, return the value (`VALIDATION_TYPE`).
- `nonblank(value[, label])` — assert a non-blank string, return it UNTRIMMED; the label names the value in the error (`VALIDATION_NONBLANK`).
- `get_or(map, key, default)` — the tolerant twin: default covers both absent and nil.

Using `validate(...)`/`require_key(...)` as a bare statement is a legitimate
assertion — the raise is the point; the return value is for composing.

## Choosing a format

- **JSON** — interop with the outside world (HTTP APIs, JMAP, other tools). Loses key order in both directions (keys come back sorted); types preserved.
- **strict-data `.mix`** (`data_*` / `load_data`) — substrate-internal config and data a human authors or an agent generates. Order-preserving, comment-friendly, safe-by-construction (never runs as code), exact round-trip. The default for anything Mix owns end-to-end.
- **TOML** — interop with TOML-native tooling.
- **CSV / INI** — ingesting legacy text tables / config; string-typed, no encoder.
- **jq** — querying/filtering a value you already have, especially predicate selection over a stream.

## See also

- [strings](strings.md) — string vs. raw quoting, `${...}` interpolation, `..` concat
- [collections](collections.md) — lists, maps, indexing, `.field` / `[i]` access
- [functions](functions.md) — and the HOFs (`map`/`filter`/`sort_by`) you run over parsed data
- [files](io.md) — `read_file` / `write_file` / `read_lines` / `read_file_bytes`
- [errors](errors.md) — `try`/`catch` around a parse that may raise
- [modules](functions.md) — `source` / `include` (which *run* a file) vs. `load_data` (which doesn't)
- [Bus messaging](bus.md) — `data_encode` for safely splicing values into a remote script
- [math](math.md) — numeric builtins over the numbers `to_number` produces
- [numbers](numbers.md) — f64 precision, radix literals, non-finite propagation
- [system](system.md) — `sql_quote` / `shell_quote` and when binds beat quoting
- The [mix repo](https://github.com/markc/cosmix)

```
mix builtins json     list every JSON / TOML / data builtin
mix what NAME         one-line description of a single builtin (e.g. mix what jq)
mix help              the full categorized builtin reference
```
