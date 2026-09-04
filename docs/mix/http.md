# http — HTTP client builtins

The HTTP client builtins — a small, blocking HTTP/1.1 client built on
[`ureq`](https://docs.rs/ureq). Three calls cover the whole surface:
`http_get`, `http_post`, and the any-verb `http_request`. They are
**feature-gated** (the `http` cargo feature) in `cosmix-lib-mix`; the shipped
`mix` binary turns the feature on, so they are always present in the CLI. In
the builtin catalogue they list under the `system` category
(`mix builtins system`); one-line help for any name: `mix what http_get`.

> Mental model: every call **returns a map, never raises, for anything that
> happens on the network**. A successful or HTTP-error response is
> `{status, body, bytes}`; a transport failure — including a
> [timeout](#timeouts) — is `{status: 0, error}`. You branch on `status`, you
> don't wrap the call in [try/catch](errors.md). (Argument mistakes — wrong
> arity, a bad opts map, a bytes header value — do raise: they are script
> bugs, not network weather.)

```mix
$r = http_get("https://example.com")
print("status=" .. $r["status"])
print("body_len=" .. length($r["body"]))
```
```text
status=200
body_len=559
```

## The three calls

```
http_get(url, [headers], [{timeout, ssl_verify, ca_file, ca_pem}])                      -- GET
http_post(url, body, [headers], [{timeout, ssl_verify, ca_file, ca_pem}])               -- POST with a body
http_request(method, url, [body], [headers], [{timeout, ssl_verify, ca_file, ca_pem}])  -- any verb
```

- `url` — a string (anything else is stringified via the usual coercion).
- `headers` — an optional **map** of header name → value (see [Headers](#headers)).
- `body` — a string (sent as UTF-8) or a `bytes` buffer (sent raw); see [Request bodies](#request-bodies).
- `method` (`http_request` only) — upper-cased internally, so `"get"`, `"Get"`, `"GET"` are equivalent. It must be a valid [RFC 7230](https://www.rfc-editor.org/rfc/rfc7230) token (no spaces / control bytes) or the call returns the error shape — this guards against request-line injection.
- `{timeout, ssl_verify, ca_file, ca_pem}` — an optional trailing **opts map**. `timeout` bounds the whole request (default 30 s, `{timeout: 0}` disables — see [Timeouts](#timeouts)). `ssl_verify` defaults to `true`; `ssl_verify: false` **skips TLS certificate and hostname verification** for that call (like `curl -k`) — see [Skipping TLS verification](#skipping-tls-verification).
- `ca_file` / `ca_pem` (v0.29.0) — trust a **private CA** for this call: `ca_file` reads PEM certificate(s) from a path, `ca_pem` takes them inline (string/bytes/buffer). The certificates are **ADDED to the default (Mozilla webpki) roots** — chain building and hostname verification still run in full, so this is the right way to talk to an internal endpoint with a proper private-CA-issued cert (the readiness-check case), unlike `ssl_verify: false` which proves nothing. Mutually exclusive with each other and with `ssl_verify: false`; input capped at 4 MiB; a missing/unreadable file, invalid PEM, or PEM with no certificates raises a catchable `HTTP_TLS` structured error (see [errors](errors.md)). In a capability-sandboxed embedder, `ca_file` additionally requires the `fs-read` class (declared as a conditional capability in the builtin metadata).

`http_get` takes 1–3 arguments, `http_post` 2–4, `http_request` 2–5. Too few
raises a runtime error, and — unlike the Mix-wide minimum-arity convention —
**too many also raises**, so a misplaced opts map can't be silently ignored
and leave a call unbounded:

```mix
http_get()
-- Runtime error at line 1: http_get() expects at least 1 argument(s), got 0
http_post("https://example.com")
-- Runtime error at line 1: http_post() expects at least 2 argument(s), got 1
http_get($url, {}, {}, {})
-- Runtime error at line 1: http_get() expects at most 3 argument(s), got 4
```

## The return shape

Every call resolves to one of two map shapes.

### Success / HTTP-error response

| key | type | meaning |
|---|---|---|
| `status` | number | the HTTP status code (`200`, `404`, `503`, …) |
| `body` | string \| nil | the body **decoded as UTF-8**; `nil` when the bytes are not valid UTF-8 |
| `bytes` | bytes | the **raw** response byte buffer, always present |
| `headers` | map | response headers — **lowercase** names → **list** of string values (repeated fields preserved), e.g. `$r.headers["content-type"]` is `["text/html"]` (v0.30.0) |
| `final_url` | string | the URL after any redirects (v0.30.0) |
| `duration_ms` | number | wall-clock request duration (v0.30.0) |
| `error_code` | string \| nil | `nil` on any HTTP response (incl. 4xx/5xx); an `HTTP_*` code only on a transport failure (v0.30.0) |
| `error` | string \| nil | `nil` on an HTTP response; the message on a transport failure |

The `headers`/`final_url`/`duration_ms`/`error_code` keys are **additive** —
pre-0.30 scripts reading `status`/`body`/`bytes` are unaffected.

A **4xx / 5xx is a response, not a failure** — it carries the real code plus
whatever error payload the server sent, so REST callers can read both:

```mix
$r = http_get("https://example.com/does-not-exist-xyz")
print("status=" .. $r["status"])
print("body=" .. substr($r["body"], 0, 40))
```
```text
status=404
body=<!doctype html><html lang="en"><head><ti
```

The documented success test is `status == 200` (or a `2xx` range check) — it is
unaffected by error statuses landing in `status` rather than the error shape.

### Transport failure — `{status: 0, error}`

Only a genuine transport error — DNS failure, TLS handshake, connection refused,
a [deadline expiry](#timeouts), a mid-stream read error, an invalid method, or
an over-cap body — collapses to `status == 0` with a human-readable `error`
string and **no** `body`/`bytes`:

```mix
$r = http_get("http://127.0.0.1:1/")
print("status=" .. $r["status"])
print("error=" .. $r["error"])
```
```text
status=0
error=http://127.0.0.1:1/: Connection Failed: Connect error: Connection refused (os error 111)
```

`status: 0` is the one sentinel that means "no HTTP exchange happened" — a real
server never returns code 0, so a single `if $r["status"] == 0` cleanly
separates "the network broke" from "the server answered". The canonical guard:

```mix
$r = http_get($url)
if $r["status"] == 0 then
  print("transport error: " .. $r["error"])
else
  -- $r["status"] is a real HTTP code; $r["body"]/$r["bytes"] are present
end
```

## Text vs binary — `body` and `bytes`

`bytes` always carries the raw buffer. `body` is the **UTF-8 decode** of that
buffer, or `nil` when the bytes are not valid UTF-8 — so a binary response
(image, archive, anything high-bit) does **not** silently corrupt through a
lossy decode. An *empty* body decodes to the empty string `""`, not `nil` —
`nil` strictly means "not UTF-8". Reach for `.body` for JSON/text, `.bytes`
for binary:

```mix
$r = http_get("https://www.google.com/favicon.ico")
print("status=" .. $r["status"])
print("body_is_nil=" .. ("" .. ($r["body"] == nil)))
print("bytes_present=" .. ("" .. (not is_empty($r["bytes"]))))
```
```text
status=200
body_is_nil=true
bytes_present=true
```

For a text response, `body` is a normal string and `bytes` holds the same data
as a raw buffer — write it straight to disk with `write_file` (a bytes value
is written verbatim), or pass it to a [bytes helper](data.md):

```mix
$r = http_get("https://example.com")
write_file("/tmp/example.html", $r["bytes"])
```

To measure the buffer use `bytes_len($r["bytes"])` or, since **v0.64.0**, plain
`length($r["bytes"])` — both answer the byte count. The raw body can also be
indexed, sliced, iterated and searched directly (`bytes_find`, `bytes_split`,
`bytes_starts_with`); see [io](io.md#bytes-as-a-sequence-v0640).

## Headers

The optional `headers` argument is a **map**. Keys are header names, values are
stringified the usual way. If you don't set a `User-Agent`, `ureq`'s default
(`ureq/2.12.1`) goes out — the examples here set their own because some APIs
care:

```mix
$r = http_get("https://api.github.com/zen", { "User-Agent": "mix-docs" })
print("status=" .. $r["status"])
print("body=" .. $r["body"])
```
```text
status=200
body=Encourage flow.
```

A `bytes` **value** is rejected (not stringified) so the internal `<bytes:N>`
placeholder never ships over the wire — HTTP headers are text. If you genuinely
need binary header metadata, encode it yourself:

```mix
http_get($url, { "X-Token": $raw_bytes })
-- Runtime error: http: header `X-Token` does not accept bytes; base64_encode($v) first
```

One carve-out: a **sole** map in this slot whose keys are **all** opts keys
(`timeout`, `ssl_verify`, `ca_file`, `ca_pem`) is read as the opts map, not as headers — see
[the trailing-map rule](#the-trailing-map-rule). Any real-world header map
(carrying a non-opts key like `Authorization`) is unaffected.

## Request bodies

`http_post` (and `http_request`'s optional 3rd slot) send a body. A **string**
is sent as UTF-8; a **bytes** buffer is sent raw (so a binary upload doesn't go
through string coercion). Set `Content-Type` yourself — the client does not
guess it:

```mix
$payload = "{\"hello\":\"world\"}"
$r = http_post("https://httpbin.org/post", $payload, { "Content-Type": "application/json" })
print("status=" .. $r["status"])
$j = json_parse($r["body"])
print("echoed=" .. $j["data"])
```
```text
status=200
echoed={"hello":"world"}
```

`http_request`'s body slot is **`nil`-tolerant**: an absent or `nil` 3rd
argument sends a bodyless request (the right behaviour for `GET`/`DELETE`/
`OPTIONS`):

```mix
$r = http_request("GET", "https://example.com")   -- no 3rd arg → bodyless
print("status=" .. $r["status"])
```
```text
status=200
```

`HEAD` reports the real status with an empty body — a bodyless response is
not a transport failure:

```mix
$r = http_request("HEAD", "https://example.com")
print($r["status"] .. " / body=" .. length($r["body"]) .. " bytes")
```
```text
200 / body=0 bytes
```

> A `HEAD` response mirrors the headers a `GET` would return — including a
> `Content-Encoding` — but the server omits the body. The body is skipped
> rather than drained; before this fix, a `HEAD` from a server that sends
> `Content-Encoding` (example.com, github.com, …) collapsed to `{status: 0,
> error}` because the (empty) body was still run through a decoder.

An invalid method short-circuits to the error shape (no request is sent):

```mix
$r = http_request("BAD METHOD", "https://example.com")
print($r["status"] .. " / " .. $r["error"])
```
```text
0 / http: invalid request method "BAD METHOD" (must be an RFC 7230 token)
```

## Timeouts

Every call runs under a **total-request deadline — 30 seconds by default** —
one wall-clock budget covering connect, TLS, request write, and response read
(`ureq` itself sets no timeout; before mix 0.21 a stalled server could hang
the evaluator, and a login shell, forever). Override it per call with the
trailing opts map:

```mix
$r = http_get($url, {timeout: 5})            -- 5 s deadline
$r = http_get($url, $headers, {timeout: 5})  -- headers AND a deadline
$r = http_get($url, {timeout: 0})            -- no deadline (deliberately long transfer)
```

`timeout` takes a **non-negative integer, in seconds**. A fractional,
negative, or non-number value is a loud runtime error, and so is any other
key in the opts map — a typo can't silently produce an unbounded call:

```mix
http_get($url, {}, {timeot: 5})
-- Runtime error at line 1: http_get: unknown opt "timeot" (supported: timeout)
http_get($url, {timeout: 1.5})
-- Runtime error at line 1: http_get: timeout must be a non-negative integer, got 1.5
```

A deadline expiry is a **transport failure, not a raise** — the usual
`{status: 0, error}` shape. (There is no `timed_out` key; that is a
[`run_rc`](system.md) convention.)

```mix
$r = http_get("http://127.0.0.1:8098/", {timeout: 1})   -- a hanging server
print($r["status"] .. " / " .. $r["error"])
```
```text
0 / http://127.0.0.1:8098/: Network Error: Network Error: Error encountered in the status line: timed out reading response
```

### The trailing-map rule

The opts map rides in the **last** slot, after the optional headers (and, for
`http_request`, body) slots. To keep the common case short, a **sole trailing
map whose keys are all opts keys (`timeout`, `ssl_verify`, `ca_file`, `ca_pem`) is always read as the
opts map** — never as a headers map, and never as a request body:

```mix
http_get($url, {timeout: 1})                     -- deadline, not a `timeout` header
http_get($url, {ssl_verify: false})              -- skip TLS verification
http_post($url, $body, {timeout: 1})             -- deadline, not a header
http_request("GET", $url, {timeout: 1})          -- deadline, not a body
http_request("POST", $url, $body, {timeout: 1})  -- deadline, not a header
```

To genuinely send a literal `timeout` HTTP header, spell out the opts slot —
an empty opts map keeps the 30 s default:

```mix
$r = http_get($url, {timeout: "60s"}, {})   -- sends the header `timeout: 60s`
```

A map carrying any non-opts key (e.g. `Authorization`) is a plain headers map
(or, in `http_request`'s 3rd slot, a stringified body), so real-world maps never
trip the rule.

## Skipping TLS verification

TLS is on for every `https://` URL, and a bad certificate surfaces as a
transport error (`{status: 0, error}`), not a panic. Some internal endpoints —
Proxmox VE (`:8006`), Proxmox Backup Server (`:8007`), and other appliances —
serve a **self-signed** certificate that no public CA vouches for. Rather than
pin their CA, pass `ssl_verify: false` in the opts map to skip certificate and
hostname verification for that call, exactly like `curl -k`:

```mix
$r = http_get("https://b1.example:8006/api2/json/version",
              {"Authorization": $tok}, {ssl_verify: false})
```

The TLS handshake still happens — the connection is encrypted and the peer's
signatures are checked against the ring provider's algorithms — but the
certificate-chain / hostname **trust** decision is bypassed. That removes
protection against a man-in-the-middle, so use it **only** for endpoints you
reach over a trusted path (a private/WireGuard network, localhost, an SSH
tunnel). `ssl_verify` defaults to `true`; there is no way to disable
verification globally — it is always per-call and explicit.

## The 64 MiB body cap

A response body is buffered fully into memory and **capped at 64 MiB**
(`67_108_864` bytes). The cap exists so a huge or endlessly-streaming response
can't OOM a process embedding the Mix evaluator. An over-cap body collapses to
the transport-error shape:

```text
{status: 0, error: "http: response body exceeds the 67108864 byte cap (64 MiB)"}
```

This is a hard limit, not a tunable — these builtins are for API calls and
modest fetches, not for streaming multi-gigabyte downloads. For a large file,
shell out to a streaming tool by full path (see [run](system.md)), which takes
its own `{timeout: seconds}` opts map if you want the download bounded too:

```mix
run("/usr/bin/curl -fsSL https://example.com/big.iso -o /tmp/big.iso", {timeout: 600})
```

## Parsing responses

`body` is just a string, so feed it to the [data](data.md) builtins. JSON is
the common case — `json_parse` turns the body into a map/list with dotted /
indexed field access:

```mix
$r = http_get("https://api.github.com/repos/markc/mix",
  { "User-Agent": "mix-docs", "Accept": "application/vnd.github+json" })
if $r["status"] == 200 then
  $j = json_parse($r["body"])
  print("name=" .. $j["name"])
  print("default_branch=" .. $j["default_branch"])
end
```
```text
name=mix
default_branch=main
```

For richer extraction use `jq(...)`; for a numeric field, `to_number(...)`. See
[data](data.md) for the full parse / serialize surface (`json_parse`, `jq`,
`data_encode`, the `bytes_*` helpers).

## Worked example — a guarded JSON fetch helper

The idiom in one function — single `status == 0` transport guard, then branch
on the HTTP code, parse on success. It uses the
[pass-in / return / reassign](functions.md) discipline (no shared globals):

```mix
fn fetch_json($url)
  $r = http_get($url, { "User-Agent": "mix-docs", "Accept": "application/json" })
  if $r["status"] == 0 then
    return { ok: false, why: $r["error"] }
  end
  if $r["status"] != 200 then
    return { ok: false, why: "http " .. $r["status"] }
  end
  return { ok: true, data: json_parse($r["body"]) }
end

$res = fetch_json("https://api.github.com/zen")
if $res["ok"] then
  print("got: " .. ("" .. $res["data"]))
else
  print("failed: " .. $res["why"])
end
```

## Notes & sharp edges

- **Blocking & synchronous.** Each call blocks until the response is buffered, the deadline expires (30 s default — see [Timeouts](#timeouts)), or the body cap / a transport error trips. There is no async variant — for latency- sensitive paths, tighten `{timeout: N}` and branch on `status == 0`.
- **No automatic retries, no cookie jar, no redirect-body replay** — `ureq`'s defaults apply (redirects are followed). Add your own retry loop if you need one.
- **TLS is on for `https://`.** A bad certificate surfaces as a transport error (`status: 0`), not a panic.
- **Headers and methods are validated** to block request-line / header injection: a non-token method and a `bytes` header value are both rejected in-band (the former as `{status: 0, error}`, the latter as a runtime error).
- **Don't confuse Mix-string `$(...)` with the URL.** In a double-quoted Mix string, `${name}` interpolates but `$(...)` is literal text — build URLs with `..` concat or `${...}`, e.g. `"https://example.com/api/" .. $id`. See [strings](strings.md).

## Serving — `http_serve`, `http_recv` (v0.75.0)

The other direction, deliberately minimal. **The boundary first**: no TLS
ever, and no dynamic handlers — certificates, vhosts, and request
dispatch to code are [webd]'s job and serve-mode citizens' job. What
lives here is the pair of shapes an agent keeps needing locally:

```
http_serve(root[, {port, host, duration, index, listing, render_md,
                   requests, spa, clean_urls}])
    -> requests served (BLOCKING)
http_recv(port[, {timeout, host, max, respond}])
    -> {method, path, query, headers, body, bytes, from_host, from_port} | nil
```

**`http_serve` is the `python -m http.server` slot**: serve a directory
to a browser — docs preview, a build artifact, a report. GET and HEAD
only; anything else answers 405. `port: 0` (the default) binds an
ephemeral port and **prints the URL** before serving, so the caller
always learns where. `host` defaults to `127.0.0.1`; exposing to the LAN
is one explicit `host: "0.0.0.0"`. `duration` bounds the run in seconds
(0 = until interrupted) — an in-flight request gets at most a 10-second
read grace past it, so one slow client cannot hold the server open;
`requests` stops after N exchanges; `listing`
opts into directory indexes; `render_md` (markdown feature) serves `.md`
files as styled HTML — `http_serve("docs/mix", {render_md: true})` is
this manual in your browser, served by Mix.

Traversal is the defended edge: the request path is percent-decoded
*first*, then the resolved file is canonicalised and must remain under
the canonicalised root — `../`, `%2e%2e%2f`, and symlinks inside the
root pointing out all answer 403/404, and the tests prove each one
against raw sockets (a polite HTTP client normalizes `../` away; an
attacker's socket does not).

**Serving a site with clean URLs, two shapes.** A static server maps
`/bus/props-core` to a file of that exact name and 404s when there isn't
one — but real sites use extensionless URLs. Two options cover the two
site shapes, and they compose (clean_urls is tried first — a specific
page beats a generic fallback):

- **`clean_urls: true`** — a would-be 404 on an extensionless path serves
  `<path>.html` when it exists (`/bus/props-core` → `/bus/props-core.html`).
  This mirrors GitHub Pages' pretty-URL behaviour, and is the fix for a
  **pre-rendered page-per-route site** — a real page or a router stub at
  every clean URL. A section page (`bus.html`) sitting beside its
  children directory (`bus/`) resolves too: `/bus` serves the page,
  `/bus/props-core` its child, and `/bus/` (trailing slash) stays a
  directory request. `http_serve("docs", {clean_urls: true})` serves the
  cosmix.dev docs tree locally, fully styled, as Pages does.
- **`spa: true`** (or `spa: "shell.html"`) — a would-be 404 on an
  extensionless path serves ONE shell, so a **single-shell SPA**'s
  client-side router boots on every deep URL (`/users/42/settings` →
  the shell → the router renders). Point it at any Vite/React build.
  The shell path is checked at startup, not per request.

Both are frontend-only, and the boundary is exact: a **static SPA** —
assets from root, data from static files or a *separate* API — works
completely; `http_serve` serves the app, never runs its backend. A path
**with** an extension (`/main.css`, `/logo.png`) is never rewritten by
either option, so a genuinely missing asset stays an honest 404 instead
of silently becoming the HTML shell.

**`http_recv` catches ONE request and answers it** — the OAuth
localhost-redirect wait, the webhook test, the "curl me when the job is
done" rendezvous. It blocks up to `timeout` seconds (default 30, 0 =
forever), returns `nil` on timeout (an ordinary answer — poll again),
and hands back the full request: `body` as UTF-8 or nil, `bytes` always
the truth, headers lowercase-keyed. `respond` shapes the reply
(`{status, body, content_type, headers}`, default `200 ok`); `max` caps
the request body (default 1 MiB, refused with 413 beyond it).

```mix
-- wait for an OAuth redirect, answer the browser, extract the code
$r = http_recv(8910, {timeout: 120, respond: {body: "You can close this tab.\n"}})
if $r == nil then die "no redirect within 2 minutes" end
$code = parse_query($r["query"])["code"]
```

Both are sequential, `Connection: close`, one exchange per connection —
a browser and a script, not a load. The moment the words "certificate",
"virtual host", or "route this to a function" appear, the answer is webd.

## See also

- [data](data.md) — `json_parse`, `jq`, `data_encode`, `bytes_to_string` for parsing and serializing HTTP payloads
- [run](system.md) — shelling out (`curl`/`wget`) for streaming or oversized transfers
- [strings](strings.md) — building URLs and bodies (`..` concat, interpolation rules)
- [functions](functions.md) — the pass-in / return / reassign idiom used in the helper above
- [errors](errors.md) — why these builtins return a shape instead of raising
- [builtins index](builtins.md) — the full builtin catalogue
- [Bus messaging](bus.md) — in-mesh RPC (`send`/`emit`); HTTP is for the world *outside* the mesh
- [the manual index](README.md) — every page in this manual
- `mix what http_get` · `mix what http_post` · `mix what http_request` · `mix builtins system` · the [mix repo](https://github.com/markc/cosmix)

