# Builtin index

Every Mix builtin grouped by category, generated from `mix builtins` (mix 0.21.2). See the linked topical page for prose and examples; `mix what NAME` prints a one-line description of any single builtin (or keyword), and `mix help` prints the compact names-only summary of the same ten categories.

Some builtins are feature-gated on the `cosmix-lib-mix` crate (`json`, `regex`, `toml`, `datetime`, `url`, `crypto`, `http`, `sqlite`, `dkim`, `markdown`, `datastar`) so embedders can pull only what they need — the `mix` binary turns them all on.

## Machine-readable discovery (metadata_schema 1)

Since 0.29.0 every builtin carries a structured contract — per-argument names/kinds/optionality, derived min/max arity, a return shape, effect flags, an operational-failure mode, and any conditional capabilities. The human signature shown by `mix builtins <name>` is derived from the same contract, so it can never drift from the machine metadata.

- `mix builtins --json` — the full table as a JSON array; each entry carries `metadata_schema` (1), `kind` (`builtin` or `statement` for print/eprint), `name`, `category`, `capability` (kebab class, or `statement`), `conditional_capabilities` (`[{option, capability}]`), `description`, `signature`, `arity` (`{min, max, exact}` — `max: null` = variadic; `exact` is normally `null`, but when a builtin's accepted argument counts are NOT a contiguous range it lists them exhaustively, e.g. `random` has `exact: [0, 2]` — an arity checker must use `exact` when present), `args` (`[{name, required, variadic, kind}]`), `returns`, `effects`, and `operational_failure`.
- `mix builtins --data` — the same logical array as strict-data Mix source (parse with `data_parse` / `load_data`); `null` becomes `nil`.
- Value shapes (`kind`/`returns`) are `{type: "any"|"nil"|"bool"|"number"|"string"|"bytes"|"buffer"|"function"}` scalars, `{type: "list", items: shape}`, `{type: "map", shape: name-or-null, fields: [...]}`, or `{any_of: [shapes...]}` unions.
- `effects` flags: `must_use` (discarding the result hides a failure — `run_rc`, `ssh_run`, `http_*`, ...), `blocking` (unbounded/external wait: process, network, stdin, sleep), `shell` (backed by `/bin/sh`), `mutates_args` (`push`, `buffer_push`, ...), `terminates` (`exit`, `panic`).
- `operational_failure`: how I/O-level failure is reported — `not-applicable` (pure), `raises` (catchable error), `returns-result` (encoded in the returned value, e.g. `ok:false` / `status:0` / exit code), `terminates`. Argument-validation errors always raise regardless.
- Arity in the contract is the documented surface; a few older builtins tolerate surplus arguments at runtime. `mix lint` checks calls against the contract, which is deliberately stricter.

> Four builtin families carry table categories that `mix builtins` does not bucket (`db`, `jmap`, `bus`, `datastar`), so they are absent from the listings below — `mix what NAME` still finds each one. They are the capability-gated embedder seams `db_query` `db_exec`, `jmap` `jmap_upload`, and `bus_call` (call a host-injected Bus verb under delegated identity: `bus_call(verb, args) → reply`; v0.20.3) — see [capabilities](capabilities.md) — plus the Datastar SSE framers `ds_patch_elements` `ds_patch_signals` `ds_sse` — see [datastar](datastar.md). The Bus-layer callables `noded_register`, `subscribe`, `unsubscribe`, `bus_reconnect`, and `reply(...)` (inside `on` handlers) are evaluator forms on top of the keyword layer, not table builtins — `mix what` does not know them; see [bus](bus.md).

## string  — see [string](strings.md)

```text
  length          Return length of string, list, or map
  len             Alias for length()
  upper           Convert string to uppercase
  lower           Convert string to lowercase
  left            Return leftmost N characters
  right           Return rightmost N characters
  substr          Extract substring by codepoint position and length (splits emoji/combining; see grapheme_substr)
  pos             Find first position of needle in haystack (1-based, 0=not found)
  lastpos         Find last position of needle in haystack (1-based, 0=not found)
  strip           Remove leading/trailing whitespace
  trim            Alias for strip()
  replace         Replace all occurrences of old with new in string
  split           Split string into list by delimiter (default: space)
  join            Join list into string with delimiter (default: space)
  starts_with     Test if string starts with prefix
  ends_with       Test if string ends with suffix
  contains        Test if string/list contains a value
  repeat          Repeat string N times
  lpad            Left-pad string to width (codepoint count; see lpad_w for display cells)
  rpad            Right-pad string to width (codepoint count; see rpad_w for display cells)
  lpad_w          Left-pad to width in terminal display CELLS (UAX #11; CJK/emoji=2) — aligns wide-char columns
  rpad_w          Right-pad to width in terminal display CELLS (UAX #11; CJK/emoji=2) — aligns wide-char columns
  reverse         Reverse a string (by codepoint; splits emoji — see grapheme_reverse) or list
  words           Count whitespace-delimited words in string
  word            Extract Nth word from string (1-based)
  grep            Return lines from text matching pattern (regex when enabled)
  template        Substitute single-brace {key} placeholders in a string from a map
  word_wrap       Wrap text to a column width (codepoint budget; see word_wrap_w for display cells)
  word_wrap_w     Wrap text to a column width in terminal display CELLS (UAX #11; CJK/emoji=2)
  markdown_escape Escape markdown metacharacters in a string
  markdown        Render CommonMark + GFM markdown (tables, strikethrough, task lists, footnotes) to HTML; raw HTML is escaped and unsafe URL schemes neutralised (requires markdown feature)
  html_escape     Escape & < > " ' for HTML element text + quoted attribute values (not JS/CSS/URL/srcdoc contexts)
  sanitize        Make untrusted bytes safe for one-line diagnostics: collapse line breaks (incl. U+2028/9) to spaces, replace C0/C1 controls and Trojan-Source bidi/zero-width chars with '?'
  regex_match     Test if pattern matches string (requires regex feature)
  regex_find      Return first regex match, or nil if no match
  regex_replace   Replace regex matches with replacement text
  regex_split     Split string by regex pattern
  csv_parse       Parse CSV string into list of row lists
  ini_parse       Parse INI string into nested map of sections
  xml_parse       Parse a strict-XML string into a Value tree (simple SOAP-friendly map, or {mode:"tree"} full fidelity; requires xml feature)
  url_parse       Parse URL into {scheme, host, port, path, query, fragment}
  url_decode      Percent-decode a URL/form-encoded string ('+' → space)
  url_encode      Percent-encode a string for use in a URL/form body
  parse_query     Parse a k=v&k2=v2 query/form string into a map (url-decoded, last-wins)
  parse_form      Parse an x-www-form-urlencoded body into a map (alias of parse_query)
  byte_length     Length of a string in raw UTF-8 bytes (the pre-0.8.0 length() value for strings)
  byte_pos        Byte offset of needle in haystack (1-based, 0=not found) — byte twin of pos()
  byte_lastpos    Last byte offset of needle in haystack (1-based, 0=not found) — byte twin of lastpos()
  byte_index_of   Byte offset of needle in string (0-based, -1=not found) — byte twin of string index_of()
  grapheme_count  Count grapheme clusters (user-perceived chars: emoji/flags/combining count as 1)
  grapheme_substr Substring by grapheme cluster position and length (won't split emoji/combining marks)
  grapheme_reverse Reverse a string by grapheme cluster (emoji/combining-safe, unlike reverse())
  display_width   Terminal display width in cells (UAX #11; CJK/emoji=2, combining=0, East-Asian-ambiguous=1)
```

## type  — see [type](numbers.md)

```text
  type            Return type name: string, number, bool, list, map, nil
  to_number       Convert value to number (nil if not numeric)
  to_string       Convert value to its string representation
  is_number       Test if value is numeric or a numeric string
  is_empty        Test if string/list/map is empty, or value is nil ("0" is NOT empty)
```

## math  — see [math](math.md)

```text
  round           Round to nearest integer, half away from zero; round(x, n) to n decimal places (n<0 rounds to tens/hundreds) (v0.19.0)
  floor           Round down toward -inf; floor(x, n) to n decimal places (v0.19.0)
  ceil            Round up toward +inf; ceil(x, n) to n decimal places (v0.19.0)
  trunc           Truncate toward zero (drop the fraction); trunc(x, n) to n decimal places (v0.19.0)
  abs             Absolute value (v0.19.0)
  sign            Sign of x: -1, 0, or 1 (±0→0, NaN→NaN) (v0.19.0)
  sqrt            Square root (negative→NaN) (v0.19.0)
  cbrt            Cube root (defined for negatives) (v0.19.0)
  pow             Raise to a power: pow(base, exp) = base^exp (v0.19.0)
  exp             e raised to the x (v0.19.0)
  ln              Natural logarithm, base e (ln(0)→-inf, ln(neg)→NaN) (v0.19.0)
  log10           Base-10 logarithm (v0.19.0)
  log2            Base-2 logarithm (v0.19.0)
  log             Logarithm in an arbitrary base: log(x, base) (v0.19.0)
  min             Smallest of the number arguments, or of a single list argument: min(a, b, …) | min(list). Lexicographic when all args are strings; NaN-skipping (v0.19.0)
  max             Largest of the number arguments, or of a single list argument: max(a, b, …) | max(list). Lexicographic when all args are strings; NaN-skipping (v0.19.0)
  clamp           Constrain a number to a range: clamp(x, lo, hi) (errors if lo > hi) (v0.19.0)
  hypot           Euclidean distance sqrt(x²+y²) computed without intermediate overflow: hypot(x, y) (v0.19.0)
  sin             Sine of x in radians (v0.19.0)
  cos             Cosine of x in radians (v0.19.0)
  tan             Tangent of x in radians (v0.19.0)
  asin            Arcsine in radians (domain [-1, 1], else NaN) (v0.19.0)
  acos            Arccosine in radians (domain [-1, 1], else NaN) (v0.19.0)
  atan            Arctangent in radians (v0.19.0)
  atan2           Angle in radians of the point (x, y): atan2(y, x) (v0.19.0)
  pi              The constant π (v0.19.0)
  e               Euler's number e (v0.19.0)
  random          random() -> float [0,1); random(min, max) -> integer [min, max] inclusive (v0.23.0)
```

## list  — see [list](collections.md)

```text
  push            Append value to end of list (mutates)
  pop             Remove and return last element of list (mutates)
  shift           Remove and return first element of list (mutates)
  sort            Return sorted copy of list (all-number→numeric; else lexicographic)
  index_of        Find index of value in list (-1 if not found)
  unique          Return list with duplicates removed
  range           Generate list of numbers from start to end with optional step
  flat            Flatten nested lists into a single list
  concat          Concatenate 2+ lists into one new list (one level; each arg must be a list)
  slice           Sublist [start, end): negative indices and out-of-range clamp (v0.2.0)
  take            First N items of a list (negative N = last N) (v0.2.0)
  drop            Skip first N items of a list (negative N = drop last N) (v0.2.0)
  zip             Pair two lists element-wise into [a, b] tuples (v0.2.0)
```

## map  — see [map](collections.md)

```text
  keys            Return list of map keys
  values          Return list of map values
  has_key         Test if map contains a key
  merge           Merge two maps (second wins on conflicts)
  delete          Return map with key removed
```

## io  — see [io](io.md)

```text
  read_file       Read entire file contents as string
  read_file_bytes Read file contents as raw bytes. Optional 2nd arg caps the read: read_file_bytes(path, 8192) reads at most 8192 bytes (header-sniffing without slurping a huge file) (v0.3.1; cap v0.17.1)
  read_lines      Read file as a list of lines (trailing newline stripped, empty last line dropped) (v0.2.3)
  load_data       Read + parse a strict-data .mix file (bare-key `k: v`, the zones.mix/conf.mix form) into a Value — the non-executing twin of source/include, for substrate-internal data that must NOT run as code (v0.9.0)
  write_file      Write string or bytes to file (creates/overwrites). Bytes are written verbatim (v0.3.1).
  write_new       Atomically create a new file with mode. write_new(path, content, 0o600) — mode as a value (octal literal) or octal string "0600"; fails if path exists; mode applied at creation (no umask race)
  append_file     Append string to file
  exists          Test if path exists
  is_dir          Test if path is a directory
  is_file         Test if path is a regular file
  glob            List files matching a glob pattern (supports ** globstar in v0.2.1)
  ls              List directory entries
  mkdir           Create directory (and parents)
  chmod           Set file/directory permissions. chmod(path, 0o755) — mode as a VALUE (use an octal literal) or an octal string "0755" (v0.11.0: a number is now the value, not its decimal digits read as octal)
  chown           Set file owner/group by numeric uid/gid: chown(path, 1000, 1000). Follows symlinks. Numeric only (no name resolution) (v0.17.1)
  stat            Stat a path → map {uid, gid, nlink, size, mode, perm, ino, dev, ctime, mtime, atime, ctime_nsec, mtime_nsec, atime_nsec, is_file, is_dir, is_symlink}. ino/dev are STRINGS (u64, exceed f64 exact range); mode is full st_mode, perm = mode & 0o7777; *time are epoch seconds (f64) and *_nsec the sub-second part 0..=999999999 (v0.44.0) — compare the PAIR to see a same-second rewrite. Follows symlinks by default; stat(path, {follow_symlinks:false}) is lstat (v0.17.1)
  line_count      Count lines in a file by streaming — never loads the whole file (byte-oriented, so it works on non-UTF-8 files too) (streams since v0.28.1)
  head            First N lines of a file as a list (default 10) — streams and stops after N lines, never reads the rest (the no-slurp twin of take(read_lines(p), n)) (v0.28.1)
  tail            Last N lines of a file as a list (default 10) — reads backwards in blocks from EOF, never slurps the whole file (the no-slurp twin of take(read_lines(p), -n)) (v0.28.1)
  basename        Return the filename component of a path
  dirname         Return the directory component of a path
  extname         Return the file extension (including the leading dot)
  path_join       Join path components with the native separator
  path_parts      Decompose a path into {dir, base, stem, ext} (v0.2.1)
  walk            Recursive directory walk: walk(dir, {max_depth, follow_symlinks, include_dirs}); invalid max_depth raises instead of becoming unlimited (strict since v0.55.0)
  readline        Read a line from stdin (optional prompt argument)
  read_stdin      Read all of stdin to EOF as a string (for pipe/hook input)
  sqlopen         Open a SQLite database and return a handle
  sqlexec         Execute SQL on a SQLite handle, return result rows
  sqlclose        Close a SQLite database handle
  print           Print values to stdout with newline (statement, not a builtin call)
  eprint          Print values to stderr with newline (statement, not a builtin call)
```

## system  — see [system](system.md)

```text
  env             Get environment variable value
  time            Return current Unix timestamp as float
  pid             Return current process ID
  args            Return list of script arguments
  getopt          Parse args against a spec map: getopt(args(), {all:{short:"a"}, out:{short:"o", arg:true}}) -> {opts, rest, errors}. opts has every declared option (flag->bool, value->string|nil); rest=positionals (incl. post `--`); errors=collected unknown-option/missing-value strings ([]=clean). Forms: --long, -s, --k=v, --k v, -s v, -- terminator. Minimal: no bundling/abbrev (v0.12.0)
  exit            Exit with optional status code
  sleep           Sleep for N seconds (async)
  run             Run shell command via sh, return trimmed stdout as string. run(cmd, [{timeout: seconds}]) — 0 (default) = no deadline; a timed-out child is PG-killed and run dies (catchable)
  run_rc          Run shell command, return {rc, stdout, stderr, timed_out, interrupted} map. run_rc(cmd, [{timeout: seconds}]) — 0 (default) = no deadline; timeout → rc=-1 timed_out=true
  run_stream      Run an argv LIST directly (no sh), inheriting stdio so output streams live and the child can use the terminal (interactive when it has a pty, e.g. ssh -t); returns the exit code. run_stream(argv, [{env, clear_env, cwd}]) — v0.51.0; run_argv's other option keys are refused by name
  spawn           Start background process via /bin/sh -c, return PID
  kill            Send signal to process (default SIGTERM)
  shell_quote     Single-quote-wrap a string for safe interpolation into a POSIX shell command
  sql_quote       Escape a string for SQL string literals: doubles ' and escapes \ (MySQL/MariaDB-safe — the documented target; also safe for SQLite, where a literal backslash arrives doubled — use sqlexec binds for exact bytes); NUL bytes stripped
  random_password Generate an alphanumeric password (default len 16, no O/o, guaranteed upper+lower+digit)
  ssh_run         Run a command on a remote host via ssh; returns {stdout, stderr, exit_code, ok, duration_ms, host, timed_out, interrupted, utf8_lossy}
  ssh_must        ssh_run wrapper: returns stdout on success, throws a Mix error otherwise
  ssh_mix         Run Mix source on a remote host: ships the source over ssh stdin into `/opt/cosmix/bin/mix -`, bypassing ALL shell quoting. ssh_mix(host, source, [opts]) -> same map as ssh_run; opts {decode:"data"|"json"} adds a parsed `.value` from stdout. Accepts every ssh_run opt except stdin (source IS the stdin). (v0.20.4)
  process_alive   Test if a process exists (signal 0 check)
  panic           Abort via an uncatchable Rust panic (distinct from catchable die); the SPEC 18 §3.4 handler boundary isolates it in --serve mode
  hostname        Return the system hostname
  cwd             Return current working directory
  chdir           Change current working directory
  platform        Return OS platform string (linux, macos, windows, etc.)
  which           Locate an executable in PATH
  date_format     Format Unix timestamp with strftime pattern
  date_parse      Parse date string with strftime pattern into Unix timestamp
  now_iso         Current time as ISO 8601 string
  duration_format Format seconds as human-readable duration (e.g. "2h 15m")
  relative_time   Format timestamp as relative string (e.g. "3 hours ago")
  base64_encode   Encode string as base64
  base64_decode   Decode base64 string
  hash_blake3     BLAKE3 hash of string, return hex digest
  hash_sha256     SHA-256 hash of string, return hex digest
  hmac_sha256     HMAC-SHA256 (RFC 2104) hex digest of msg under a secret key — webhook signature verification (requires crypto feature)
  constant_time_eq Timing-safe equality for secrets/MACs — full-length compare, no early exit (requires crypto feature)
  uuid            Generate a new random UUID v4 string
  dkim_keygen     Generate a DKIM keypair. dkim_keygen("rsa", [bits=2048]) or dkim_keygen("ed25519") → {algorithm, private_pem, public_b64, dns_txt_record}
  http_get        HTTP GET. http_get(url, [headers], [{timeout: seconds}] — default 30, 0 disables) → {status, body, bytes} | {status:0, error}. `body` is the response decoded as UTF-8 (nil if not valid UTF-8); `bytes` is the raw byte buffer. Response bodies are capped at 64 MiB (over-cap → {status:0, error}).
  http_post       HTTP POST. http_post(url, body, [headers], [{timeout: seconds}] — default 30, 0 disables) → {status, body, bytes} | {status:0, error}. `body`/`bytes` semantics (incl. the 64 MiB body cap) match http_get.
  http_request    HTTP any-verb. http_request(method, url, [body], [headers], [{timeout: seconds}] — default 30, 0 disables) → {status, body, bytes} | {status:0, error}. `body`/`bytes` semantics (incl. the 64 MiB body cap) match http_get.
  bytes_len       Length of a Value::Bytes buffer in bytes (v0.3.1)
  string_to_bytes Convert a string to its UTF-8 byte representation (v0.3.1)
  bytes_to_string Convert a bytes buffer to a string; strict UTF-8, or pass {lossy:true} for a from_utf8_lossy decode (v0.17.2)
  dns_lookup      Resolve a hostname to a list of IP address strings
  help            Show Mix builtin help in the REPL
```

## format  — see [format](strings.md)

```text
  fmt             printf-style format string → string. Specs: %s %d %f %.Nf %Nd %-Ns %0Nd %% (v0.2.0; %0Nd zero-pad v0.54.0)
  printf          Formatted write to stdout (no trailing newline — include \n explicitly) (v0.2.0)
  eprintf         Formatted write to stderr (v0.2.0)
  format_bytes    Format byte count as human-readable size (e.g. "1.5 MB"); a non-numeric argument raises (strict since v0.55.0)
  format_number   Format number with thousands separators; non-numeric value/decimals arguments raise (strict since v0.55.0)
```

## json  — see [json](data.md)

```text
  json_parse      Parse JSON string into Mix value
  json_encode     Encode Mix value as JSON string
  jq              Run a jq filter; filter MUST yield 0 (→nil) or 1 (→value) output, >1 raises. jq(value, filter)
  jq_all          Run a jq filter, collect ALL outputs as a list (the stream case). jq_all(value, filter)
  read_json       Read a single-record JSON file directly into a Mix value (v0.2.3)
  read_jsonl      Read a JSON-lines file — list of records, strict by default, {skip_errors: true} for lenient (v0.2.3)
  toml_parse      Parse TOML string into Mix map
  toml_encode     Encode Mix value as TOML. Raises TOML_UNREPRESENTABLE with {path,type} details for nil, function, bytes, or buffer values instead of silently replacing them with empty strings (strict since v0.55.0)
  data_parse      Parse a strict-data `.conf.mix` string into a Mix value (inverse of data_encode) (v0.3.2)
  data_encode     Encode a Mix value as a strict-data `.conf.mix` string with correct \$ / \~ / \\ escaping; round-trips through data_parse. data_encode(value, [pretty]) — truthy 2nd arg emits multi-line indented output (v0.3.2)
```

## hof  — see [hof](hof.md)

```text
  sort_by         Return list sorted ascending by key function (stable) (v0.2.0)
  filter          Return new list of items where predicate returns truthy (v0.2.0)
  map             Return new list of transform(item) results (v0.2.0)
  reduce          Fold list left with an explicit init: reduce($xs, $init, function($a, $b) = ...) (v0.2.0)
  any             Short-circuit: true if any item matches predicate (v0.2.0)
  all             Short-circuit: true if every item matches predicate (v0.2.0)
  count           Count items where predicate returns truthy (v0.2.0)
  min_by          Return the ITEM (not the key) with minimum key-function value (v0.2.0)
  max_by          Return the ITEM with maximum key-function value (v0.2.0)
  sum_by          Sum of key(item) across all items, returns number (v0.2.0)
  group_by        Map of stringified-key → list of items (first-seen key order) (v0.2.0)
  unique_by       Dedup list by key function, first occurrence wins (v0.2.0)
```

## Prelude — Mix-defined helpers, not builtins

`mix help` also lists a small **prelude** (`lines`, `chars`, `sum`, `read_lines`, `avg`): plain Mix functions auto-loaded from `std/prelude.mix` before every script and `~/.mixrc`. They are not builtins — `mix builtins` and `mix what` do not know them. A builtin always shadows a same-named Mix function, which is why the old prelude `min`/`max`/`abs`/`clamp` shims were retired when the native `math` category landed in v0.19.0 (the `read_lines` **builtin** likewise shadows the prelude definition of the same name).

## See also

- [overview](overview.md) · [keywords](keywords.md) · [the mix CLI](cli.md)
- `mix builtins [CATEGORY]` lists one category with one-line descriptions (`mix builtins` alone lists all ten); `mix what NAME` describes one builtin or keyword; `mix help` prints the compact category / keyword / subcommand summary.
