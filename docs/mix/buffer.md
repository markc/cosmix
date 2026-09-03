# buffer — the reference-semantic mutable byte buffer

`Buffer` is the one type in Mix with **reference semantics**. Everything else —
strings, numbers, lists, maps, `bytes` — is value-semantic: `$b = $a` gives `$b`
an independent copy, and passing a value into a function can't mutate the
caller's copy. A `Buffer` is the deliberate exception: `$b = $a` makes `$b` an
**alias** of the same underlying byte store, and an in-place append through
either name is visible through both.

It exists for one job: building or editing **large binary blobs** (audio/video
samples, `.mid`/`.wav`/image bytes) without the O(n²) cost of value-semantic
append. `buffer_push` grows the store in place (O(1) amortized); the
value-semantic alternative — re-building a `bytes` each step — copies the whole
growing blob every time. When you're done, `freeze` snapshots the buffer to a
value-semantic `bytes` for the write_file / hash / base64 / http sinks.

List the family live with `mix builtins buffer`; one-line help with
`mix builtins NAME`.

## The builtins

| Call | Result | Notes |
|---|---|---|
| `buffer()` | empty buffer | |
| `buffer(n)` | buffer of `n` zero bytes | allocate — `n` a non-negative integer |
| `buffer("MThd")` | buffer of the string's UTF-8 bytes | |
| `buffer($bytes)` / `buffer($buf)` | an **independent** copy | fresh backing store |
| `buffer([items])` | flat splice of each item | each item: int 0-255 / string / bytes / buffer |
| `buffer_push($buf, item, ...)` | `nil` — appends in place | reference-semantic; variadic; same item kinds |
| `buffer_get($buf, i)` | byte at 0-based `i` as a number, or `nil` if out of range | |
| `buffer_set($buf, i, byte)` | `nil` — writes `byte` (0-255) at `i` | errors if `i` is out of range |
| `bytes_len($buf)` | length in bytes | the same builtin works on `bytes` |
| `freeze($buf)` | a value-semantic `bytes` snapshot | the bridge to the byte sinks |

`bytes_to_string`, `base64_encode`, `hash_sha256`/`hash_blake3`, and
`write_file`/`append_file`/`write_new` all also accept a `Buffer` directly, so
you rarely need an explicit `freeze` just to hand a buffer to a sink.

Since **v0.64.0** a buffer also answers the generic sequence operations and the
whole `bytes_*` family — `$buf[i]`, `length($buf)`, `slice($buf, s, e)`,
`for each $x in $buf`, `bytes_find`, `bytes_split`, `bytes_to_hex` and the rest;
see [io](io.md#bytes-as-a-sequence-v0640). Three properties matter because a
buffer is reference-semantic:

- **`slice` returns an independent `bytes`**, never a view — a later
  `buffer_push` cannot change a slice you already took.
- **`for each` is pinned at loop entry** — growing the buffer inside the body
  does not extend the loop.
- **`$buf[i]` is NOT a synonym for `buffer_get($buf, i)`.** They agree on an
  in-range non-negative integer, and both answer `nil` when a non-negative
  index is past the end — but they part company on the other index forms:
  `$buf[i]` follows Mix's universal *indexing* rules (a negative index counts
  from the end, a fractional one truncates), while `buffer_get` is a strict
  accessor that **raises** on both:

  ```mix
  $buf = buffer("ab")
  print($buf[-1])              -- 98  (the last byte)
  print(buffer_get($buf, -1))  -- raises: index must be a non-negative integer
  ```

  Neither is wrong; `$buf[i]` is consistent with `$list[i]`/`$str[i]`/`$b[i]`,
  and `buffer_get` is consistent with `buffer_set`, which must refuse a
  computed-nonsense index rather than write somewhere surprising.

Writing through the index (`$buf[i] = 65`) is still refused; the in-place write
is `buffer_set`.

## Building binary — the media-pipeline idiom

```mix
-- a MIDI header chunk: ASCII magic + raw length/format bytes in one call
$mid = buffer(["MThd", 0, 0, 0, 6, 0, 1, 0, 1, 0, 0x60])
buffer_push($mid, "MTrk")          -- append the track magic in place
buffer_push($mid, 0x90, 60, 100)   -- a note-on event, byte by byte
write_file("out.mid", $mid)        -- the buffer writes verbatim
```

`buffer([...])` and `buffer_push` take the same flat item kinds — an integer
0-255 becomes one byte (a non-integral or out-of-range number is an error, since
Mix numbers are f64), a string splices its UTF-8 bytes (inline ASCII magic like
`"RIFF"`/`"P6"`), and a `bytes`/`buffer` splices its current content.

## Reference semantics — the one thing to remember

```mix
$a = buffer([1, 2])
$b = $a                 -- $b ALIASES $a — same backing store
buffer_push($b, 3)
print(bytes_len($a))    -- 3  (the append through $b is visible through $a)

$c = buffer($a)         -- buffer($x) makes an INDEPENDENT copy
buffer_push($c, 9)
print(bytes_len($a))    -- 3  (unchanged — $c has its own store)

$snap = freeze($a)      -- value-semantic bytes; frozen at this moment
buffer_push($a, 99)
print(bytes_len($snap)) -- 3  (the snapshot doesn't move)
```

Gotchas that fall out of reference semantics:

- **A buffer inside a copied list stays shared.** Copying a list deep-copies its structure, but a `Buffer` element is a reference — the copy's element and the original's element are the same buffer. Use `freeze($buf)` or `buffer($buf)` to store an independent snapshot in a collection.
- **`==` is content equality**, like `bytes`: two distinct buffers with the same bytes compare equal. A `Buffer` and a `bytes` are never equal — `freeze` first to compare across the two types.
- **`print`/interpolation show `<buffer:N>`**, never the raw bytes (dumping high-bit bytes corrupts terminals — the same reason `bytes` prints `<bytes:N>`). Reach the payload with `bytes_to_string`, `base64_encode`, or `write_file`.
- **An empty buffer is falsy**; a non-empty one is truthy.

## When to use which

- Reach for **`bytes`** (value-semantic) for a fixed binary payload you read once — an HTTP body, a `read_file_bytes` result, a `base64_decode` output — and for anything you store in a list/map or pass around expecting copy semantics.
- Reach for **`Buffer`** (reference-semantic) when you are *constructing* or *editing* a blob incrementally — appending in a loop, patching bytes in place — where the value-semantic copy-per-step would be O(n²).
- `freeze` converts buffer → bytes when you're done building; `buffer($bytes)` converts the other way when you want to start editing.
