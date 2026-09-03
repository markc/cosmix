# Builtin index

Every Mix builtin grouped by category, generated from `mix builtins` (mix 0.62.0). See the linked topical page for prose and examples; `mix what NAME` prints a one-line description of any single builtin (or keyword), and `mix help` prints the compact names-only summary of the same ten categories.

Some builtins are feature-gated on the `cosmix-lib-mix` crate (`json`, `regex`, `toml`, `datetime`, `url`, `crypto`, `http`, `sqlite`, `dkim`, `markdown`, `datastar`) so embedders can pull only what they need — the `mix` binary turns them all on.

## Machine-readable discovery (metadata_schema 1)

Since 0.29.0 every builtin carries a structured contract — per-argument names/kinds/optionality, derived min/max arity, a return shape, effect flags, an operational-failure mode, and any conditional capabilities. The human signature shown by `mix builtins <name>` is derived from the same contract, so it can never drift from the machine metadata.

- `mix builtins --json` — the full table as a JSON array; each entry carries `metadata_schema` (1), `kind` (`builtin` or `statement` for print/eprint), `name`, `category`, `capability` (kebab class, or `statement`), `conditional_capabilities` (`[{option, capability}]`), `description`, `signature`, `arity` (`{min, max, exact}` — `max: null` = variadic; `exact` is normally `null`, but when a builtin's accepted argument counts are NOT a contiguous range it lists them exhaustively, e.g. `random` has `exact: [0, 2]` — an arity checker must use `exact` when present), `args` (`[{name, required, variadic, kind}]`), `returns`, `effects`, and `operational_failure`.
- `mix builtins --data` — the same logical array as strict-data Mix source (parse with `data_parse` / `load_data`); `null` becomes `nil`.
- Value shapes (`kind`/`returns`) are `{type: "any"|"nil"|"bool"|"number"|"string"|"bytes"|"buffer"|"function"}` scalars, `{type: "list", items: shape}`, `{type: "map", shape: name-or-null, fields: [...]}`, or `{any_of: [shapes...]}` unions.
- `effects` flags: `must_use` (discarding the result hides a failure — `run_rc`, `ssh_run`, `http_*`, ...), `blocking` (unbounded/external wait: process, network, stdin, sleep), `shell` (backed by `/bin/sh`), `mutates_args` (`push`, `buffer_push`, ...), `terminates` (`exit`, `panic`).
- `operational_failure`: how I/O-level failure is reported — `not-applicable` (pure), `raises` (catchable error), `returns-result` (encoded in the returned value, e.g. `ok:false` / `status:0` / exit code), `terminates`. Argument-validation errors always raise regardless.
- Arity in the contract is the documented surface; a few older builtins tolerate surplus arguments at runtime. `mix lint` checks calls against the contract, which is deliberately stricter.

> Every table category is listed below (0.63.0 — the index is regenerated
> from `mix builtins --json` by `docs/build/gen-builtins-md.mix`; the
> previously-unbucketed `db`/`jmap`/`bus`/`datastar` families now have
> sections). The Bus-layer callables `noded_register`, `subscribe`,
> `unsubscribe`, `bus_reconnect`, and `reply(...)` (inside `on` handlers)
> remain evaluator forms on top of the keyword layer, not table builtins —
> `mix what` does not know them; see [bus](bus.md).

## string  — see [strings](strings.md)

  length          Return length of string, list, or map
  len             Alias for length()
  upper           Convert string to uppercase
  lower           Convert string to lowercase
  left            Return leftmost N characters
  right           Return rightmost N characters
  substr          Extract substring by codepoint position and length (splits emoji/combining; see grapheme_substr)
  pos             Find first position of needle in haystack (1-based, 0=not found — so `if pos(..)` reads correctly, since 0 is falsy). 0-based twin: index_of(), which takes its args the other way round and is NOT safe in a condition
  lastpos         Find last position of needle in haystack (1-based, 0=not found — safe in a condition, 0 is falsy). 0-based twin: last_index_of(), which takes its args the other way round
  strip           Remove leading/trailing whitespace, or codepoints in charset: strip(s[, charset]) (0.63.0 — the 2nd arg was silently IGNORED before)
  trim            Alias for strip(): trim(s[, charset]) — charset is a SET of codepoints to strip from both ends (0.63.0; was silently ignored). One-sided: ltrim/rtrim
  replace         Replace all occurrences of old with new in string
  split           Split string into list by delimiter (default: space)
  join            Join list into string with delimiter (default: space)
  starts_with     Test if string starts with prefix
  ends_with       Test if string ends with suffix
  contains        Test if string/list contains a value — the correct yes/no test, and what to use instead of a bare index_of() in a condition
  repeat          Repeat string N times
  lpad            Left-pad string to width (codepoint count; see lpad_w for display cells). Optional 3rd arg is the fill character, default space: lpad(s, 12, "0") (v0.54.0)
  rpad            Right-pad string to width (codepoint count; see rpad_w for display cells). Optional 3rd arg is the fill character, default space (v0.54.0)
  lpad_w          Left-pad to width in terminal display CELLS (UAX #11; CJK/emoji=2) — aligns wide-char columns. Optional 3rd arg is the fill character (must be 1 cell wide), default space (v0.54.0)
  rpad_w          Right-pad to width in terminal display CELLS (UAX #11; CJK/emoji=2) — aligns wide-char columns. Optional 3rd arg is the fill character (must be 1 cell wide), default space (v0.54.0)
  reverse         Reverse a string (by codepoint; splits emoji — see grapheme_reverse) or list
  words           Count whitespace-delimited words in string
  word            Extract Nth word from string (1-based)
  grep            Return lines from text matching pattern (regex when enabled)
  before          Text before the FIRST delim: before(s, delim) -> string | nil (nil when delim absent; "" is a real result — delim at the start). Empty delim raises
  after           Text after the FIRST delim: after(s, delim) -> string | nil (nil when delim absent; "" when delim at the end). Empty delim raises. Want a default? `after($s, "=") or ""`
  before_last     Text before the LAST delim -> string | nil (nil when absent). Empty delim raises
  after_last      Text after the LAST delim -> string | nil (nil when absent) — basename/extension: after_last(path, "/"), after_last(name, "."). Empty delim raises
  split_once      Split at the FIRST delim: split_once(s, delim) -> [head, tail] | nil (nil when absent — never a 1-element list). Empty delim raises
  rsplit_once     Split at the LAST delim -> [head, tail] | nil (nil when absent). Empty delim raises
  between         Text after the first a and before the NEXT b: between(s, a, b) -> string | nil (nil if either marker absent, in that order). Empty a or b raises
  strip_prefix    s without a leading p, else s UNCHANGED (never nil — "nothing to strip" is an answer). Empty p -> unchanged. Kills the starts_with+substr idiom
  strip_suffix    s without a trailing x, else s UNCHANGED (never nil). Empty x -> unchanged
  replace_first   Replace the FIRST occurrence of old; old absent -> s unchanged. Empty old mirrors replace(): inserts new at the start (replace_first("ab", "", "X") is "Xab")
  count_of        Non-overlapping occurrences of needle in s; 0 for an empty needle. (The HOF count(list, pred) is a different builtin)
  ltrim           Strip leading whitespace, or leading codepoints in charset: ltrim(s[, charset]) — PHP-style charset is a SET of codepoints, not a prefix string
  rtrim           Strip trailing whitespace, or trailing codepoints in charset: rtrim(s[, charset])
  lines           Split into lines: \n-separated, ONE trailing \r stripped per line (CRLF and LF both work; a lone \r is not a terminator), exactly one trailing empty element dropped (the final newline). lines("") -> []; "a\n\n" -> ["a", ""]. Native since 0.63.0 (was a prelude fn that kept \r and the trailing "")
  fields          awk-style fields: split on whitespace RUNS, no empties; fields("") -> []. 0-based access fields(s)[2]; the 1-based single-field form is word(s, n)
  chars           Codepoints as 1-char strings (grapheme_* builtins exist for clusters). Native since 0.63.0 (was a prelude fn)
  last_index_of   0-based codepoint index of the LAST occurrence in a string, or last index of a value in a list; -1 if absent. The 0-based twin of lastpos() (args reversed). List search compares with == — SCALAR elements only, exactly like index_of (a map/list element never matches; deep_eq is the structural comparison). ⚠ Like index_of, NEVER bare in a condition: -1 is truthy, 0 is falsy
  template        Substitute single-brace {key} placeholders in a string from a map
  word_wrap       Wrap text to a column width (codepoint budget; see word_wrap_w for display cells)
  word_wrap_w     Wrap text to a column width in terminal display CELLS (UAX #11; CJK/emoji=2)
  markdown_escape Escape markdown metacharacters in a string
  markdown        Render CommonMark + GFM markdown (tables, strikethrough, task lists, footnotes) to HTML; raw HTML is escaped and unsafe URL schemes neutralised (requires markdown feature)
  html_escape     Escape & < > " ' for HTML element text + quoted attribute values (not JS/CSS/URL/srcdoc contexts)
  sanitize        Make untrusted bytes safe for one-line diagnostics: collapse line breaks (incl. U+2028/9) to spaces, replace C0/C1 controls and Trojan-Source bidi/zero-width chars with '?'
  regex_match     Test if pattern matches string (requires regex feature)
  regex_find      Return ALL regex matches as a list of {match, start, end[, groups]} maps (empty list if none)
  regex_replace   Replace regex matches with replacement text
  regex_split     Split string by regex pattern
  re_match        Subject-first regex test: re_match(s, pattern) -> bool, true if pattern matches anywhere in s
  re_find         All matches as {match, start, end[, groups]} maps with CODEPOINT offsets (compose with substr/slice/index_of; legacy regex_find returns UTF-8 BYTE offsets). [] when none
  re_replace      Replace ALL matches: re_replace(s, pattern, replacement) — subject FIRST; $1/${name} backrefs in replacement
  re_split        Split s on each match of pattern (subject first)
  grep_lines      Lines of text matching pattern (subject first; regex when enabled, else substring) — grep() with the args the consistent way round
  csv_parse       Parse CSV string into a list of header-keyed row maps
  ini_parse       Parse INI string into nested map of sections
  xml_parse       Parse a strict-XML string (or bytes, e.g. an HTTP body) into a Value tree (requires xml feature). Default simple mode is the SOAP/RSS consumer shape: {RootName: …} with namespace prefixes stripped, attributes as @name keys, repeated sibling elements collapsed to a list, a leaf element's text as its value, mixed text under #text, xmlns declarations dropped. Pass {mode:"tree"} for full fidelity: nodes are {name, attrs, children} with prefixes + xmlns preserved and text children as plain strings. Strict XML only — real-world HTML is tag soup and will NOT parse.
  url_parse       Parse URL into {scheme, host, port, path, query, fragment}
  url_decode      Percent-decode a URL/form-encoded string ('+' → space)
  url_encode      Percent-encode a string for use in a URL/form body
  parse_query     Parse a k=v&k2=v2 query/form string into a map (url-decoded, last-wins)
  parse_form      Parse an x-www-form-urlencoded body into a map (alias of parse_query)
  byte_length     Length of a string in raw UTF-8 bytes (the pre-0.8.0 length() value for strings)
  byte_pos        Byte offset of needle in haystack (1-based, 0=not found) — byte twin of pos(), 0-based twin byte_index_of(). Safe in a condition (0 is falsy)
  byte_lastpos    Last byte offset of needle in haystack (1-based, 0=not found) — byte twin of lastpos()
  byte_index_of   Byte offset of needle in string (0-based, -1=not found) — byte twin of string index_of(), 1-based twin byte_pos(). ⚠ NEVER use bare in a condition: -1 is truthy, 0 is falsy (MIX-W2305)
  grapheme_count  Count grapheme clusters (user-perceived chars: emoji/flags/combining count as 1)
  grapheme_substr Substring by grapheme cluster position and length (won't split emoji/combining marks)
  grapheme_reverse Reverse a string by grapheme cluster (emoji/combining-safe, unlike reverse())
  display_width   Terminal display width in cells (UAX #11; CJK/emoji=2, combining=0, East-Asian-ambiguous=1)

## type  — see [numbers](numbers.md)

  deep_eq         Structural equality for any two values: maps compare by key set + deep_eq values (insertion order IGNORED), lists elementwise in order, scalars as ==. The answer `==` cannot give for maps/lists (it is always false there today). Caveats, inherited from ==: a FUNCTION value is never equal (even to itself — a callback-bearing map is not deep_eq its own copy) and Buffer-vs-Bytes is false (freeze() first). Raises past 512 nesting levels
  type            Return type name: string, number, bool, list, map, nil
  to_number       Convert value to number (nil if not numeric)
  to_string       Convert value to its string representation
  is_number       Test if value is numeric or a numeric string
  is_empty        Test if string/list/map is empty, or value is nil ("0" is NOT empty)

## math  — see [math](math.md)

  round           Round to nearest integer, half away from zero; round(x, n) to n decimal places (n<0 rounds to tens/hundreds) (v0.19.0)
  floor           Round down toward -inf; floor(x, n) to n decimal places (v0.19.0)
  ceil            Round up toward +inf; ceil(x, n) to n decimal places (v0.19.0)
  trunc           Truncate toward zero (drop the fraction); trunc(x, n) to n decimal places (v0.19.0)
  abs             Absolute value (v0.19.0)
  sign            Sign of x: -1, 0, or 1 (±0→0, NaN→NaN) (v0.19.0)
  band            Bitwise AND of two integers. Both arguments must be exact integers within ±2^53 (the range f64 represents without loss); a fraction, an infinity, a NaN or an out-of-range magnitude raises rather than silently truncating. Chiefly for permission bits: band(stat(p)["perm"], 0o111) != 0 asks whether any execute bit is set (v0.46.0)
  bor             Bitwise OR of two integers; same exact-integer domain as band (v0.46.0)
  bxor            Bitwise exclusive-OR of two integers; same exact-integer domain as band (v0.46.0)
  bnot            Bitwise NOT (one's complement) over 64-bit two's complement: bnot(0) is -1. Same exact-integer domain as band (v0.46.0)
  bshl            Shift left by n bits (n in 0..63). Raises rather than wrapping if the result leaves the exact-integer range (v0.46.0)
  bshr            Arithmetic shift right by n bits (n in 0..63); the sign bit is replicated, so bshr(-8, 1) is -4 (v0.46.0)
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
  random          random() -> float in [0,1); random(min, max) -> integer in [min, max] inclusive (v0.23.0)

## list  — see [collections](collections.md)

  push            Append value to end of list (mutates)
  pop             Remove and return last element of list (mutates)
  shift           Remove and return first element of list (mutates)
  sort            Return sorted copy of list (all-number lists sort numerically; else lexicographic)
  index_of        Find 0-based index of value in a list, or of a needle substring in a string (codepoint-based); -1 if absent. 1-based twin: pos() (args reversed). ⚠ NEVER use bare in a condition — -1 (absent) is TRUTHY and 0 (first position) is FALSY, so both answers invert; use contains() for yes/no or compare >= 0 (MIX-W2305)
  unique          Return list with duplicates removed
  range           Generate list of numbers from start to end with optional step. Bounds/step must be whole numbers within i64 — fractional or oversized values raise VALUE_OUT_OF_RANGE instead of silently saturating (strict since v0.59.0)
  flat            Flatten nested lists into a single list
  concat          Concatenate 2+ lists into one new list (one level; each arg must be a list)
  slice           Sublist [start, end): negative indices and out-of-range clamp (v0.2.0)
  take            First N items of a list (negative N = last N) (v0.2.0)
  drop            Skip first N items of a list (negative N = drop last N) (v0.2.0)
  zip             Pair two lists element-wise into [a, b] tuples (v0.2.0)

## map  — see [collections](collections.md)

  keys            Return list of map keys
  values          Return list of map values
  has_key         Test if map contains a key
  merge           Merge two maps (second wins on conflicts)
  delete          Return map with key removed

## buffer  — see [buffer](buffer.md)

  buffer          Create a reference-semantic MUTABLE byte buffer (the escape hatch from value semantics for large binary/audio/video). buffer() empty; buffer(n) n zero bytes; buffer(string) UTF-8; buffer(bytes|buffer) independent copy; buffer([items]) flat splice of int 0-255 / string / bytes / buffer. Append with buffer_push (O(1) amortized, aliases share); freeze() to a value-semantic bytes (v0.26.0)
  buffer_push     Append bytes to a buffer IN PLACE (reference-semantic: every alias sees the growth). Each item is an int 0-255, string (UTF-8), bytes, or buffer. Self-append-safe (v0.26.0)
  buffer_get      Byte at 0-based index i as a number 0-255, or nil if out of range (v0.26.0)
  buffer_set      Write byte (0-255) at 0-based index i, in place; errors if i is out of range — grow with buffer_push first (v0.26.0)
  freeze          Snapshot a buffer to a value-semantic bytes (a copy of the current content) — the bridge into write_file/hash/base64/http (v0.26.0)

## io  — see [io](io.md)

  read_file       Read entire file contents as string
  read_file_bytes Read file contents as raw bytes. Optional 2nd arg caps the read: read_file_bytes(path, 8192) reads at most 8192 bytes (header-sniffing without slurping a huge file) (v0.3.1; cap v0.17.1)
  read_lines      Read file as a list of lines (trailing newline stripped, empty last line dropped) (v0.2.3)
  load_data       Read + parse a strict-data .mix file (bare-key `k: v`, the zones.mix/conf.mix form) into a Value — the non-executing twin of source/include, for substrate-internal data that must NOT run as code (v0.9.0)
  write_file      Write string or bytes to file (creates/overwrites). Bytes are written verbatim (v0.3.1).
  write_new       Atomically create a new file with mode. write_new(path, content, 0o600) — mode as a value (octal literal) or octal string "0600"; fails if path exists; mode applied at creation (no umask race)
  append_file     Append string to file
  exists          Test if path exists. FOLLOWS symlinks by default, so a dangling link reads as absent — that is the right answer for "can I open something here" and the wrong one for "is this name taken". exists(path, {follow_symlinks: false}) is the lstat form and sees the link itself (v0.39.0).
  access          Ask the kernel whether this process can access path using its effective uid/gid: mode is a non-empty, duplicate-free string of r/w/x/f letters (f = existence and is redundant when combined). Follows symlinks. Unlike inspecting stat().perm, this honours POSIX ACLs. Ordinary absence/denial returns false; malformed input or an unexpected syscall failure raises (v0.45.0).
  is_dir          Test if path is a directory
  is_file         Test if path is a regular file
  realpath        Canonicalise a path: resolve every symlink + `.`/`..` to the absolute real path (like `readlink -f` / realpath(3)). The path MUST exist. realpath(path) -> string | nil (nil when it can't be resolved — a missing component, a symlink loop, or a non-UTF-8 resolved path). NORMALISATION ONLY, not a race-free authorization primitive: canonicalise-then-use is not atomic, so for an exec/open safety check, exec/open the RETURNED canonical path (which has no symlinks to re-traverse), not the original (v0.31.2)
  glob            List files matching a glob pattern (supports ** globstar in v0.2.1)
  ls              List directory entries
  mkdir           Create directory: mkdir(path[, {parents}]). parents defaults to true (create_dir_all). {parents: false} creates only the final component and fails if the parent is missing — the form to use when the parent was placed deliberately and re-creating it would hide its removal (v0.42.0)
  flock           Take a process-held advisory file lock: flock(path[, {shared, wait}]) -> bool. Exclusive and non-blocking by default; contention returns false, genuine filesystem errors raise. wait is seconds (0 = do not wait). Repeated acquisition of the same canonical path by this process is idempotent-true (v0.43.0)
  funlock         Release and close this process's advisory lock for path. Returns true when held, false when not held (v0.43.0)
  copy            Copy a single file: copy(src, dst). Overwrites dst; preserves the source permission bits. Use copy_tree for a directory (v0.22.0).
  copy_tree       Recursively copy a directory: copy_tree(src, dst). Creates dst, copies files (perms preserved) and symlinks (as symlinks); merges into an existing dst (v0.22.0).
  symlink         Create a symbolic link: symlink(target, linkpath) — symlink(2), arguments in symlink(2) order (target first, the link to create second). `target` is stored verbatim and is NOT resolved or validated: a relative target resolves against the link's own directory, and creating a dangling link is legal. Raises EEXIST if linkpath already exists. Read the other way with read_link() (v0.38.0).
  read_link       Read a symbolic link's target: read_link(path) -> string — readlink(2), returning the target EXACTLY as stored (possibly relative, possibly dangling), which is what distinguishes it from realpath()'s full resolution. Raises EINVAL when path is not a symlink; test first with stat(path, {follow_symlinks: false}).is_symlink (v0.38.0).
  rename          Rename/move a path within one filesystem: rename(src, dst) — rename(2), so replacing an existing dst is ATOMIC (a concurrent reader sees either the old file or the new one, never a partial write). This is the primitive for a safe in-place update: write a temp file beside the target, then rename over it. Raises EXDEV across filesystems (copy + remove instead) and ENOENT when src is missing (v0.37.0).
  remove          Remove a single file/symlink: remove(path). No-op if already gone (rm -f). Errors if path is a directory — use remove_dir (v0.22.0).
  remove_dir      Recursively remove a directory and its contents: remove_dir(path) (rm -rf). No-op if already gone (v0.22.0).
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
  print           Print values to stdout with newline (statement, not a builtin call); bare `print` emits a blank line
  eprint          Print values to stderr with newline (statement, not a builtin call); bare `eprint` emits a blank line

## system  — see [system](system.md)

  env             Get environment variable value ("" if unset); env(name, default) returns default when unset or empty
  time            Return current Unix timestamp as float
  pid             Return current process ID
  uid             Effective user id of this process (geteuid) — normally the id a file access is checked against (Linux checks fsuid, which tracks euid unless setfsuid(2) is called; no Mix script can call it, though an embedder can), so it is the one to compare a stat() map's `uid` against when deciding whether a path is yours (v0.41.0)
  gid             Effective group id of this process (getegid) — the companion to uid(), for comparing against a stat() map's `gid`; same fsgid caveat, and it answers only whether a file's group is the EFFECTIVE one, so use groups() to decide which permission class applies (v0.41.0)
  groups          Every group id this process is in — the getgroups(2) supplementary set plus the effective gid, sorted. This is what decides whether the kernel applies a file's GROUP or OTHER permission bits: gid() alone cannot answer it for a file grouped under one of your other groups (v0.42.0)
  args            Return list of script arguments
  getopt          Parse args against a spec map: getopt(args(), {all:{short:"a"}, out:{short:"o", arg:true}}) -> {opts, rest, errors}. opts has every declared option (flag->bool, value->string|nil); rest=positionals (incl. post `--`); errors=collected unknown-option/missing-value strings ([]=clean). Forms: --long, -s, --k=v, --k v, -s v, -- terminator. Minimal: no bundling/abbrev (v0.12.0)
  exit            Exit with optional status code
  sleep           Sleep for N seconds (async)
  run             Run shell command via sh, return trimmed stdout as string. run(cmd, [{timeout: seconds}]) — 0 (default) = no deadline; a timed-out child is PG-killed and run dies (catchable)
  run_rc          Run shell command, return {rc, stdout, stderr, timed_out, interrupted} map. run_rc(cmd, [{timeout: seconds}]) — 0 (default) = no deadline; timeout → rc=-1 timed_out=true
  run_stream      Run an argv LIST directly (no sh), inheriting stdio so output streams live and the child can use the terminal (interactive when it has a pty, e.g. ssh -t); returns the exit code. run_stream(argv, [{env, clear_env, cwd}]) — same env/cwd semantics as run_argv, so an interactive child gets variables without an `env` prefix exposing them in its ps argv (v0.51.0). The run_argv-only opts (timeout, stdin, stdout, stderr, max_output, stream) are rejected by name: this runner blocks until the child exits and captures nothing
  run_argv        Run an argv list directly (no shell) with structured stdio routing and a whole-call deadline that starts before route opening. opts: timeout; stdin nil|string|bytes|buffer|{file}|{null:true}; stdout capture|inherit|null|{file,append?,mode?}; stderr capture|inherit|null|stdout|{file,append?,mode?}; cwd/env/clear_env; max_output; stream. stdout/stderr default capture; output files default truncate, mode 0o600. Routed non-capture streams return "" with truncation false and are not capped. stderr:stdout merges into stdout. File-open failure or route-open deadline is a PROCESS_STDIO value and the child is not spawned. Captured output abandoned at a deadline is returned partially with its truncation flag true. stream:true + stdout:inherit and all bad options raise OPTION_INVALID before spawn. Ordinary command/setup failure is encoded in the VALUE; timeout default 30s; max_output default 8 MiB per captured stream
  run_argv_must   Fail-fast run_argv with the same structured stdio opts: returns captured stdout unchanged when ok and no captured stream truncated ("" when stdout is routed), else raises PROCESS_EXIT_NONZERO / PROCESS_TIMEOUT / PROCESS_SIGNAL / PROCESS_INTERRUPTED / PROCESS_OUTPUT_LIMIT or the result's setup/lifecycle error_code (PROCESS_STDIO / PROCESS_SPAWN / PROCESS_IO / PROCESS_INTERNAL) with the complete result map in $err.details.result
  run_pipeline    Run one or more argv stages without a shell, connecting each stdout to the next stdin. Stage maps accept argv/cwd/env/clear_env/stderr, plus stdin on the first stage and stdout on the last, using run_argv's stdio grammar. Every route and pipe is prepared before any stage runs, so PIPELINE_STDIO means no stage ran. Returns a distinct pipeline_result with final stdout/exit fields and per-stage outcomes. One whole-call deadline starts before route opening; captured output abandoned at that deadline is partial with its truncation flag true. Non-final SIGPIPE is NOT accepted by default: any stage killed by a signal makes the pipeline not-ok, matching `set -o pipefail`. Pass allow_signal:true to accept a non-final SIGPIPE when every downstream stage succeeded (the `yes | head -1` idiom). Ordinary failure is encoded in the VALUE — never raises
  run_pipeline_must Fail-fast run_pipeline twin: returns final stdout unchanged when the pipeline is ok and no captured output truncated; otherwise raises PIPELINE_* with the complete pipeline_result in $err.details.result
  spawn           Start background process via /bin/sh -c, return PID. Every argument must be a STRING and none is coerced — a non-string raises TYPE_MISMATCH rather than being stringified into a doomed sh command (spawn returns a PID, not a result map, so a misrun child would otherwise fail invisibly). There is no argv form: use run_argv for an argv list in the foreground (strict since v0.52.0)
  kill            Send signal to process (default SIGTERM); returns false when the signal could not be delivered. Both arguments must be whole NUMBERS and neither is coerced — a bool/string pid raises TYPE_MISMATCH rather than becoming 0 (which signals this process's whole group), and an unrecognised signal raises rather than silently defaulting to SIGTERM (strict since v0.52.0)
  shell_quote     Single-quote-wrap a string for safe interpolation into a POSIX shell command
  sql_quote       Escape a string for SQL string literals: doubles ' and escapes \ (MySQL/MariaDB-safe — the documented target; also safe for SQLite, where a literal backslash arrives doubled — use sqlexec binds for exact bytes); NUL bytes stripped
  random_password Generate an alphanumeric password (default len 16, no O/o, guaranteed upper+lower+digit)
  ssh_run         Run a command on a remote host via ssh; returns {stdout, stderr, exit_code, ok, duration_ms, host, timed_out, interrupted, utf8_lossy}
  ssh_must        ssh_run wrapper: returns stdout on success, throws a Mix error otherwise
  ssh_mix         Run Mix source on a remote host: ships the source over ssh stdin into `/opt/cosmix/bin/mix -`, bypassing ALL shell quoting. ssh_mix(host, source, [opts]) -> same map as ssh_run; bindings maps valid Mix identifier names to strict-data-encoded values prepended as `$name` assignments, and decode:"data"|"json" adds a parsed `.value` from stdout. Accepts every ssh_run opt except stdin/env_transport. Remote command failure stays in the result value; invalid arguments/options raise locally. (v0.20.4)
  ssh_exec        Run an argv list DIRECTLY on a remote host via a strict-data driver and remote run_argv. Remote stdio allowlist: stdin nil|string|{file}|{null:true} (a stdin STRING is always data, as locally — there is no stdin "inherit" route on either side); stdout capture|null|{file}; stderr capture|null|stdout|{file}. File paths resolve remotely. stdout/stderr inherit and stream:true raise OPTION_INVALID locally before ssh because they would corrupt or bypass the result envelope. Binary stdin also raises locally. Transport/protocol failures and remote command failure are returned in the process_result plus host; a remote without run_argv returns SSH_REMOTE_UNSUPPORTED without running the command
  process_alive   Test if a process exists (signal 0 check). pid must be a whole NUMBER and is not coerced — a bool/string pid raises TYPE_MISMATCH rather than becoming 0, which would make the reaping waitpid() collect an arbitrary child of this process group and then report a boolean as alive (strict since v0.52.0)
  panic           Abort via an uncatchable Rust panic (distinct from catchable die); the SPEC 18 §3.4 handler boundary isolates it in --serve mode
  raise           Raise a catchable structured error: raise(code, message[, details]) — code is UPPER_SNAKE (e.g. "VALIDATION_REQUIRED", stable identifiers, scripts may define their own); a non-string message is coerced to its string form; catch with `catch $msg, $err` and read $err.code / $err.details / $err.frames (v0.29.0)
  hostname        Return the system hostname
  cwd             Return current working directory
  chdir           Change current working directory
  platform        Return OS platform string (linux, macos, windows, etc.)
  which           Locate an EXECUTABLE in PATH: a PATH entry is returned only if it is a regular file the kernel says this process may execute (faccessat2 X_OK, so POSIX ACLs count), never merely a file that exists, and never a directory. cmd must be a string and is not coerced. Returns nil when nothing on PATH is runnable under that name (executability enforced since v0.52.0)
  date_format     Format Unix timestamp with strftime pattern
  date_parse      Parse date string with strftime pattern into Unix timestamp
  now_iso         Current time as ISO 8601 string
  duration_format Format seconds as human-readable duration (e.g. "2h 15m")
  relative_time   Format timestamp as relative string (e.g. "3 hours ago")
  base64_encode   Encode string as base64
  base64_decode   Decode base64 string
  hash_blake3     BLAKE3 hash of string, return hex digest
  hash_sha256     SHA-256 hash of string, return hex digest
  hmac_sha256     HMAC-SHA256 (RFC 2104) of a message with a secret key, return hex digest — webhook signature verification (Stripe-Signature etc). Accepts string/bytes/buffer for both args (requires crypto feature)
  constant_time_eq Timing-safe equality for secrets/MACs: compares full length with no early exit (plain == leaks a timing oracle). Use for webhook signature comparison. Accepts string/bytes/buffer
  hash_file       Streaming hex digest of a file: hash_file(path[, "sha256"|"blake3"]) (v0.24.0)
  uuid            Generate a new random UUID v4 string
  dkim_keygen     Generate a DKIM keypair. dkim_keygen("rsa", [bits=2048]) or dkim_keygen("ed25519") → {algorithm, private_pem, public_b64, dns_txt_record}
  http_get        HTTP GET. http_get(url, [headers], [{timeout, ssl_verify, ca_file, ca_pem}] — timeout default 30, 0 disables; ssl_verify default true, false skips TLS cert/hostname checks like curl -k; ca_file/ca_pem ADD a private CA to the default roots — mutually exclusive with each other and with ssl_verify:false, 4 MiB cap, bad PEM raises HTTP_TLS, v0.29.0) → {status, body, bytes, headers, final_url, duration_ms, error_code, error} (headers lowercase-name→list; final_url after redirects; transport failure = status:0 + HTTP_* error_code; v0.30.0). `body` is the response decoded as UTF-8 (nil if not valid UTF-8); `bytes` is the raw byte buffer. Response bodies are capped at 64 MiB (over-cap → {status:0, error}).
  http_post       HTTP POST. http_post(url, body, [headers], [{timeout, ssl_verify, ca_file, ca_pem}]) → {status, body, bytes, headers, final_url, duration_ms, error_code, error} (headers lowercase-name→list; final_url after redirects; transport failure = status:0 + HTTP_* error_code; v0.30.0). Opts (incl. ssl_verify: false → skip TLS verification like curl -k) and `body`/`bytes` semantics (incl. the 64 MiB body cap) match http_get.
  http_request    HTTP any-verb. http_request(method, url, [body], [headers], [{timeout, ssl_verify, ca_file, ca_pem}]) → {status, body, bytes, headers, final_url, duration_ms, error_code, error} (headers lowercase-name→list; final_url after redirects; transport failure = status:0 + HTTP_* error_code; v0.30.0). Opts (incl. ssl_verify: false → skip TLS verification like curl -k) and `body`/`bytes` semantics (incl. the 64 MiB body cap) match http_get.
  bytes_len       Length of a Value::Bytes buffer in bytes (v0.3.1)
  string_to_bytes Convert a string to its UTF-8 byte representation (v0.3.1)
  bytes_to_string Convert a bytes buffer to a string; strict UTF-8, or pass {lossy:true} for a from_utf8_lossy decode (v0.17.2). Also accepts a Buffer.
  dns_lookup      Resolve a hostname to a list of IP address strings
  help            Show Mix builtin help in the REPL
  require         Load a Mix module: evaluate the file once in an isolated scope, return its exports (map of top-level fns + $vars, or the file's top-level return value). Cached per canonical path; cycles error (v0.27.0)

## format  — see [strings](strings.md)

  fmt             printf-style format string → string. Specs: %s %d %f %.Nf %Nd %-Ns %0Nd %% (v0.2.0; %0Nd zero-pad v0.54.0 — numeric only, use lpad(s,n,"0") for strings). Dynamic width `*` takes the width from the next argument: %*s %-*s %*d %0*d %*f (v0.63.0; the width argument must be a non-negative integer)
  printf          Formatted write to stdout (no trailing newline — include \n explicitly) (v0.2.0)
  eprintf         Formatted write to stderr (v0.2.0)
  format_bytes    Format byte count as human-readable size (e.g. "1.5 MB"); a non-numeric argument raises (strict since v0.55.0)
  format_number   Format number with thousands separators; non-numeric value/decimals arguments raise (strict since v0.55.0)

## json  — see [data](data.md)

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

## validate  — see [data](data.md)

  require_key     Assert a map key is present with a non-nil value and return it; raises VALIDATION_REQUIRED with details {path, expected, actual_type} (v0.29.0)
  expect_type     Assert a value's type and return it: expect_type($v, "integer") — types: any nil bool number integer string bytes buffer list map function (integer = finite whole within ±2^53-1); raises VALIDATION_TYPE (v0.29.0)
  nonblank        Assert a string contains a non-whitespace character, return it UNTRIMMED; the optional label names the value in the error; raises VALIDATION_NONBLANK — the boundary guard against nil/"" flowing into hostnames and paths (v0.29.0)
  get_or          Map lookup with a default that covers BOTH an absent key and a nil value (the tolerant twin of require_key) (v0.29.0)
  validate        Validate a map against a field spec at a job/API boundary: validate($raw, {node: {type: "string", nonblank: true}, plan: {enum: ["gold", "silver"]}, vmid: {type: "integer", min: 100, max: 999999}, tags: {required: false, type: "list", items: {type: "string"}}, owner: {type: "map", schema: {name: {nonblank: true}}}}). Rules: required (default TRUE) / type (string or list of types) / nonblank / enum / min / max / min_length / max_length / items / schema. Returns the ORIGINAL map unchanged; optional absent-or-nil fields skip their rules; unknown INPUT fields pass through; unknown RULE keys raise VALIDATION_SPEC; violations raise VALIDATION_* with details {path, expected, actual_type} — paths like owner.name and tags[2] (v0.29.0)

## hof  — see [hof](hof.md)

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

## bus  — see [bus](bus.md)

  bus_call        Call a host-injected Bus verb under delegated identity: bus_call(verb, args) → reply. The embedder bounds which verbs are reachable and injects the delegation envelope; the script names no host/peer/actor
  publish         One-call topic publish (0.63.0): publish(topic, body[, opts]) builds the SPEC-02 wire frame and sends it via noded topic.publish — no hand-built ---\n frames, no body=/name= header-route trap. body is the payload STRING (json_encode a map first); opts: {retain: bool, command: string (inner frame header override, defaults to topic), headers: map}. Sets $rc/$result like `send`; returns rc (0 = published)

## db  — see [capabilities](capabilities.md)

  db_query        Query the host-injected scoped DB: db_query(sql, [params]) → rows
  db_exec         Exec on the host-injected scoped DB: db_exec(sql, [params]) → {affected, last_insert_id}

## jmap  — see [capabilities](capabilities.md)

  jmap            Call the host-injected JMAP upstream: jmap(method, args) → result, or jmap([[method,args,callId],…]) → methodResponses
  jmap_upload     Upload bytes as a JMAP blob via the host-injected upstream: jmap_upload(body[, content_type]) → blobId (the compose half of the mail seam; Email/set create is blob-only)

## datastar  — see [datastar](datastar.md)

  ds_patch_elements Frame an HTML fragment as a Datastar patch-elements SSE event: ds_patch_elements(html, [{selector, mode, view_transition}]) → event string. mode=outer(default)/inner/remove/replace/prepend/append/before/after. Caller MUST html_escape() untrusted content first — this only frames (requires datastar feature) (v0.18.1)
  ds_patch_signals Frame a signal update as a Datastar patch-signals SSE event: ds_patch_signals(signals_map_or_json, [{only_if_missing}]) → event string. A map is JSON-encoded; a string is used verbatim (requires datastar feature) (v0.18.1)
  ds_sse          Assemble a text/event-stream response body from one event string or a list of them: ds_sse(event | [events]) → body. Pair with headers={"Content-Type":"text/event-stream"} (requires datastar feature) (v0.18.1)

## Prelude — Mix-defined helpers, not builtins

`mix help` also lists a small **prelude** (`sum`, `avg`): plain Mix functions auto-loaded from `std/prelude.mix` before every script and `~/.mixrc`. They are not builtins — `mix builtins` and `mix what` do not know them. A builtin always shadows a same-named Mix function, which is why the old prelude `min`/`max`/`abs`/`clamp` shims were retired when the native `math` category landed in v0.19.0, and why the `lines`/`chars`/`read_lines` shims went with the 0.63.0 native `lines`/`chars` (the `read_lines` builtin had shadowed its shim since v0.2.3).

## See also

- [overview](overview.md) · [keywords](keywords.md) · [the mix CLI](cli.md)
- `mix builtins [CATEGORY]` lists one category with one-line descriptions (`mix builtins` alone lists all ten); `mix what NAME` describes one builtin or keyword; `mix help` prints the compact category / keyword / subcommand summary.
