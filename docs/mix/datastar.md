# datastar — Datastar SSE event framing

Three builtins that **frame** [Datastar](https://data-star.dev) v1 Server-Sent
Events: `ds_patch_elements`, `ds_patch_signals`, and `ds_sse`. They are the pure
half of the Datastar SDK — they build the exact wire bytes the Datastar JS
client reads to patch the DOM or update the client-side signal store, and
`ds_sse` glues those event strings into a `text/event-stream` response body.

> **The one rule:** these builtins **FRAME, they do not ESCAPE.** A caller MUST
> `html_escape()` untrusted content *before* passing it to `ds_patch_elements`,
> exactly as for any rendered page (see [strings](strings.md)). Framing wraps
> your bytes in SSE lines; it does nothing to neutralise `<script>` in them.

These builtins are feature-gated on the `datastar` feature; the `mix` binary
turns it on, so they are always present in the shipped binary. They are built on
the upstream [`datastar`](https://crates.io/crates/datastar) crate's
framework-agnostic wire serializer, so the format tracks upstream and never
drifts. One-line help: `mix what ds_patch_elements`. Added in **mix 0.18.1**.

## Mental model

Datastar's transport is plain SSE. Each event is a few lines:

```text
event: <datastar-event-type>
data: <field> <value>
data: <field> <value>
<blank line>
```

A blank line terminates the SSE frame. Mix gives you one builtin per Datastar
event type plus an assembler:

| Builtin | Event type | Purpose |
|---|---|---|
| `ds_patch_elements(html[, opts])` | `datastar-patch-elements` | morph HTML into the DOM |
| `ds_patch_signals(sig[, opts])`   | `datastar-patch-signals`  | update the client signal store |
| `ds_sse(event \| [events])`        | —                         | concatenate event strings into a response body |

Each `ds_patch_*` call returns a **string** — one fully-framed SSE event,
trailing blank line included. `ds_sse` just splices those strings (no
re-framing, no separators added — each event already carries its own blank-line
terminator).

## ds_patch_elements — patch the DOM

```
ds_patch_elements(html, [{selector, mode, view_transition}]) -> event string
```

The first argument is an HTML fragment. The optional second argument is a map of
options. Bare call, no options:

```mix
$ev = ds_patch_elements("<div id=\"clock\">12:34</div>")
print($ev)
```
```text
event: datastar-patch-elements
data: elements <div id="clock">12:34</div>

```

(The output ends with a blank line — the SSE frame terminator.)

### Options

| Key | Values | Default |
|---|---|---|
| `selector` | a CSS selector string | none (Datastar uses the element's `id`) |
| `mode` | `outer` `inner` `remove` `replace` `prepend` `append` `before` `after` | `outer` |
| `view_transition` | truthy → emit `useViewTransition true`; falsy → same as absent | off |

Two option-map behaviours to know: a `nil`-valued `selector`/`mode` is treated as
absent, and **unknown keys are silently ignored** — there is no strict allowlist
here (unlike `run`/`ssh_run` opts), so a typo'd `selectr:` produces a
default-targeted frame, not an error. A non-string first argument is coerced to
its string form (`ds_patch_elements(42)` frames the text `42`).

```mix
$ev = ds_patch_elements("<li>row</li>", {selector: "#list", mode: "append"})
print($ev)
```
```text
event: datastar-patch-elements
data: selector #list
data: mode append
data: elements <li>row</li>

```

`view_transition` wraps the patch in a browser View Transition:

```mix
$ev = ds_patch_elements("<p>hi</p>", {view_transition: true})
print($ev)
```
```text
event: datastar-patch-elements
data: useViewTransition true
data: elements <p>hi</p>

```

### mode = remove

`remove` deletes matching elements and **requires** a `selector` (there is no
HTML to send — pass `""` or anything; it is ignored):

```mix
$ev = ds_patch_elements("", {mode: "remove", selector: "#toast"})
print($ev)
```
```text
event: datastar-patch-elements
data: selector #toast
data: mode remove

```

Forgetting the selector is a clean runtime error, not a silent malformed frame:

```mix
ds_patch_elements("x", {mode: "remove"})
-- Runtime error: ds_patch_elements: mode 'remove' requires a 'selector' in the options map
```

An unknown mode is rejected up front:

```mix
ds_patch_elements("x", {mode: "swap"})
-- Runtime error: ds_patch_elements: unknown mode 'swap'
--   (expected one of outer/inner/remove/replace/prepend/append/before/after)
```

### Multi-line HTML is re-prefixed per line

If the HTML contains newlines, every line gets its own `data: elements` prefix —
that is how SSE carries a multi-line value, and it is what keeps an embedded `\n`
from forging a new frame:

```mix
$html = "<ul>\n  <li>one</li>\n  <li>two</li>\n</ul>"
print(ds_patch_elements($html))
```
```text
event: datastar-patch-elements
data: elements <ul>
data: elements   <li>one</li>
data: elements   <li>two</li>
data: elements </ul>

```

This composes cleanly with `markdown()` (documented below) — render author
content to HTML, then frame it:

```mix
$ev = ds_patch_elements(markdown("# Hi\n\nrender me"), {selector: "#post", mode: "inner"})
print($ev)
```
```text
event: datastar-patch-elements
data: selector #post
data: mode inner
data: elements <h1>Hi</h1>
data: elements <p>render me</p>

```

## ds_patch_signals — update the client signal store

```
ds_patch_signals(signals_map_or_json, [{only_if_missing}]) -> event string
```

Datastar keeps a reactive **signal store** in the browser. This event merges a
patch into it. Pass a **map** (or a **list** — it becomes a JSON array) and it
is JSON-encoded for you:

```mix
$ev = ds_patch_signals({count: 5, name: "ada"})
print($ev)
```
```text
event: datastar-patch-signals
data: signals {"count":5,"name":"ada"}

```

Pass a **string** and it is used **verbatim** — it must already be valid JSON
(e.g. from `json_encode()`):

```mix
$ev = ds_patch_signals("{\"open\":true}")
print($ev)
```
```text
event: datastar-patch-signals
data: signals {"open":true}

```

`only_if_missing` makes the patch a default — it only sets signals the client
does not already have (good for initial values that must not clobber user edits):

```mix
$ev = ds_patch_signals({theme: "dark"}, {only_if_missing: true})
print($ev)
```
```text
event: datastar-patch-signals
data: onlyIfMissing true
data: signals {"theme":"dark"}

```

Any other first-argument type (number, bool, nil) is rejected:

```mix
ds_patch_signals(42)
-- Runtime error: ds_patch_signals: first argument must be a signals map or JSON string, got number
```

The map/list path uses the same encoder as `json_encode`, with the same rules
(see [JSON & data](data.md)): a **NaN or ±inf number anywhere in the value
raises** a catchable error rather than silently corrupting the store (since
mix 0.21) —

```mix
ds_patch_signals({bad: sqrt(0 - 1)})
-- Runtime error: ds_patch_signals: encode signals:
--   non-finite number NaN has no JSON representation (JSON has no NaN/Infinity)
```

— and a lambda or bytes value inside the map encodes as `null`.

## ds_sse — assemble the response body

```
ds_sse(event | [events]) -> text/event-stream body
```

`ds_sse` takes one event string or a **list** of them and concatenates them into
a single body string. It does **not** re-frame or insert separators — each event
already carries its own blank-line terminator, so back-to-back events are
correctly delimited.

A single event (the round-trip is identity — `ds_sse` of one event is that event):

```mix
$body = ds_sse(ds_patch_signals({n: 1}))
print($body)
```
```text
event: datastar-patch-signals
data: signals {"n":1}

```

A multi-event stream — patch the DOM, then flip a signal, in one response:

```mix
$e1 = ds_patch_elements("<span>a</span>", {selector: "#x", mode: "inner"})
$e2 = ds_patch_signals({ready: true})
$body = ds_sse([$e1, $e2])
print($body)
```
```text
event: datastar-patch-elements
data: selector #x
data: mode inner
data: elements <span>a</span>

event: datastar-patch-signals
data: signals {"ready":true}

```

An empty list yields an empty body; a list element that is not a string is a
clear error (`ds_sse: list element N must be an SSE event string, got <type>`).

## Security — frame, never escape

Repeat, because it is the trap: **these builtins do not sanitise.** Whatever
bytes you hand `ds_patch_elements` go on the wire and into the DOM. For any
content that came from a user, a request, a database row, or any non-author
source, run `html_escape()` first:

```mix
$user = "<script>alert(1)</script>"
$ev = ds_patch_elements("<div>" .. html_escape($user) .. "</div>")
print($ev)
```
```text
event: datastar-patch-elements
data: elements <div>&lt;script&gt;alert(1)&lt;/script&gt;</div>

```

`html_escape(s)` escapes the five HTML-significant characters — `&` `<` `>` `"`
`'` (the apostrophe as `&#x27;`, the HTML5-universal form, not the HTML4-invalid
`&apos;`) — making a value safe to interpolate into HTML **element text** or an
**ordinary quoted attribute value**. That prevents *syntactic breakout* only: it
is NOT sufficient where the value is interpreted as code or a URL — event
handlers (`onclick=…`), `style=…`, `srcdoc`, or URL-valued attributes
(`href`/`src`: an entity-escaped `javascript:…` still runs). Those contexts need
scheme/context validation, not just entity escaping.

For author content authored in Markdown, `markdown()` already escapes raw HTML
and neutralises unsafe URL schemes (next section) — so
`ds_patch_elements(markdown($author_md), …)` is safe without a separate
`html_escape`.

### Frame-injection guards (built in)

The builtins fail closed against SSE frame injection even when you forget to
escape, so a stray control character cannot forge an extra event:

- **`selector`** goes into a single, un-split `data: selector …` line, so any CR or LF in it is **rejected** — a CSS selector never legitimately carries one:

```mix
ds_patch_elements("x", {selector: "#a\ndata: mode inner"})
-- Runtime error: ds_patch_elements: selector must not contain a line terminator (CR/LF)
--   — SSE frame-injection guard
```

- **`elements`** and a verbatim **`signals`** string are CR→LF normalised, then every `\n`-split line is re-prefixed with its field name — so an embedded newline (even a lone `\r` that `str::lines()` would miss) becomes another `data:` continuation line, never a frame break. A `\r\n` pair normalises to **one** `\n`, never a doubled newline (which would read as the blank-line frame separator).
- The **map path** for `signals` is JSON-encoded by serde, which escapes control characters, so its `data:` line is provably single-line.

These are defence-in-depth, **not** a substitute for `html_escape` — a guard
stops frame forgery; it does nothing about `<script>` inside an otherwise-valid
single-line fragment.

## markdown — render author content to HTML

```
markdown(md) -> HTML string
```

Renders **CommonMark + GFM** Markdown to HTML — the natural upstream of
`ds_patch_elements` for content authored in Markdown. The GFM extensions are
enabled: **tables**, **strikethrough** (`~~gone~~` → `<del>`), **task lists**
(`- [x]` → a disabled checkbox), and **footnotes**. Feature-gated on the
`markdown` feature (the `mix` binary turns it on); a Pure builtin, so always
callable in sandboxed contexts (see [capabilities](capabilities.md)).

Its output is **safe by default for author content**:

- **Raw HTML in the source is escaped** — emitted as literal text, never active markup:

```mix
print(markdown("hi <b>bold</b> <script>alert(1)</script>"))
```
```text
<p>hi &lt;b&gt;bold&lt;/b&gt; &lt;script&gt;alert(1)&lt;/script&gt;</p>
```

- **Unsafe URL schemes are neutralised** — a link or image URL whose scheme is `javascript:`, `data:`, or `vbscript:` is rewritten to `#`. The scheme check strips ASCII whitespace/control characters first (browsers ignore those inside a scheme, so a whitespace-obfuscated `<java script:…>` destination still counts as `javascript:`); relative URLs and ordinary schemes (`https:`, `mailto:`) pass through untouched:

```mix
print(markdown("[x](javascript:alert(1)) [y](https://ok.example)"))
```
```text
<p><a href="#">x</a> <a href="https://ok.example">y</a></p>
```

The companion `markdown_escape(s)` goes the other direction: it
backslash-escapes Markdown metacharacters (`\` `*` `_` `[` `]` `(` `)` `#`
`` ` `` `|` `>` `-`) so a plain-text value can be embedded in Markdown source
without becoming markup.

## webd integration

In Cosmix these builtins exist so a Mix SSR handler running inside the `webd`
web daemon can drive a live page over SSE without a client-side framework. The
shape of a handler is: build event strings, assemble with `ds_sse`, and return
the body with the right content type:

```mix
-- inside a webd Mix handler (illustrative)
$rows = map($items, $render_row)          -- each -> an HTML <li>
$ev   = ds_patch_elements(join($rows, "\n"), {selector: "#list", mode: "inner"})
$body = ds_sse($ev)

return {
  status:  200,
  headers: { "Content-Type": "text/event-stream" },
  body:    $body
}
```

The Datastar JS client (loaded once in the page shell) opens the SSE connection,
reads each framed event, and applies the DOM patch or signal update — no
per-widget JavaScript, no compiled UI framework. This is the "Datastar as a
transport, not a client library" posture: the server emits patches, the browser
applies them.

## Quick reference

```text
ds_patch_elements(html)                              -- morph by element id (mode=outer)
ds_patch_elements(html, {selector, mode})            -- target + how to patch
ds_patch_elements("", {mode:"remove", selector:"#x"})-- delete (selector required)
ds_patch_elements(html, {view_transition:true})      -- wrap in a View Transition
ds_patch_signals({k:v, ...})                         -- merge a signal patch (map/list -> JSON)
ds_patch_signals($json_string)                       -- verbatim JSON signals
ds_patch_signals(sig, {only_if_missing:true})        -- set defaults only
ds_sse($event)                                       -- body from one event
ds_sse([$e1, $e2, ...])                              -- body from several events
markdown($md)                                        -- CommonMark+GFM -> safe HTML
markdown_escape($text)                               -- escape text INTO markdown source
html_escape($text)                                   -- escape text INTO html element text

-- modes: outer(default) inner remove replace prepend append before after
-- unknown option-map keys are silently ignored (no strict allowlist)
-- a NaN/inf number in a signals map raises (JSON has no NaN/Infinity)
-- ALWAYS html_escape() untrusted content before ds_patch_elements()
```

## See also

- [strings](strings.md) — string handling, interpolation and escaping rules
- [builtins index](builtins.md) — all builtin categories (the `ds_*` builtins are not bucketed by `mix builtins` — use `mix what <name>`)
- [higher-order functions](hof.md) — `map`/`filter` for building fragment lists
- [JSON & data](data.md) — `json_encode` for a verbatim `ds_patch_signals` payload, and the shared encoder rules (non-finite numbers raise)
- [capabilities](capabilities.md) — the `ds_*` and `markdown` builtins are Pure
- [Bus messaging](bus.md) — the mesh transport behind a live-data handler
- Datastar — <https://data-star.dev> (hypermedia framework, SSE patch semantics)
- The Mix repo — <https://github.com/markc/mix>
- ARexx lineage (every app an addressable, scriptable port) — the ergonomic precedent for Mix as a control surface
- `mix what ds_patch_elements` · `mix what ds_patch_signals` · `mix what ds_sse` · `mix what markdown`

