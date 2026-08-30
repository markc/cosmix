# Dates & time

Mix represents a moment in time as a **Unix timestamp** — a plain number of seconds since the epoch (1970-01-01 00:00:00 UTC). Everything else is conversion: format a timestamp into text, parse text back into a timestamp, or describe an interval. There is no opaque "date object" — a timestamp is just a [number](numbers.md), so you do date arithmetic with `+`, `-`, and the rest of the numeric operators.

```mix
$now = time()          -- seconds since the epoch, as a float
$tomorrow = $now + 86400   -- one day later — just arithmetic
print(date_format($tomorrow, "%Y-%m-%d"))
```

The six builtins:

| Builtin | Returns | Purpose |
|---|---|---|
| `time()` | number (float seconds) | current Unix timestamp |
| `now_iso()` | string | current local time as ISO 8601 / RFC 3339 |
| `date_format(ts [, fmt])` | string | render a timestamp via strftime (**local** time) |
| `date_parse(s)` | number | parse a date string → Unix timestamp |
| `duration_format(secs)` | string | render a span of seconds as `1d 2h 3m 4s` |
| `relative_time(ts)` | string | render a timestamp as `3h ago` / `in 5m` |

`time()` is always available. The five conversion builtins are **`datetime`-feature-gated** in the library, but the shipping `mix` binary turns the feature on, so they are always present in the `mix` CLI and as a login shell. All six are `Pure`-class (no I/O beyond reading the wall clock) and register under the `system` builtin category — `mix builtins system` lists them. For *pausing* rather than measuring, `sleep(n)` lives on the [system](system.md) page.

## time() — the current timestamp

`time()` returns the current Unix time as a **float** with sub-second precision (it is `SystemTime::now()` as `as_secs_f64`).

```mix
print(time())
```
```text
1783052394.0787532
```

The value obviously changes every call — examples below show the *shape*, not a fixed number. Because it is a number you can compare and subtract directly:

```mix
$t = time()
print("" .. ($t > 1700000000))
```
```text
true
```

To time a block of work, capture `time()` before and after and feed the difference to [`duration_format`](#duration_format--human-readable-spans):

```mix
$start = time()
-- ... do work ...
$elapsed = time() - $start
print("took " .. duration_format($elapsed))
```

> `time()` is a float. `date_format` / `relative_time` truncate it to whole seconds internally (`as i64`), so passing the raw `time()` value is fine — you never need to round it yourself.

The numeric inputs accept numbers and numeric strings. A supplied value that
`to_number` cannot parse raises `TYPE_MISMATCH`; it never silently becomes the
epoch or a zero-duration result.

## now_iso() — current local time as a string

`now_iso()` takes no arguments and returns the current **local** time as an RFC 3339 string (chrono's `Local::now().to_rfc3339()`), including the fractional seconds and the offset from UTC.

```mix
print(now_iso())
```
```text
2026-07-03T14:19:54.080883176+10:00
```

The trailing `+10:00` is the machine's local UTC offset (here AEST). Use `now_iso()` for log lines and timestamps you want a human to read at a glance; use `time()` when you need a number to do arithmetic on.

## date_format(ts [, fmt]) — timestamp → text

`date_format` turns a Unix timestamp into a string using a **strftime** pattern. The timestamp is interpreted as UTC seconds, then **converted to the machine's local timezone** before formatting.

- Argument 1: the timestamp (number; a float is truncated to whole seconds).
- Argument 2 (optional): the strftime format string. **Default is `"%Y-%m-%d %H:%M:%S"`.**

```mix
print(date_format(1700000000))
```
```text
2023-11-15 08:13:20
```

(That output is in local time, +10; the same instant is `2023-11-14 22:13:20` in UTC.)

Custom patterns — these are [chrono](https://docs.rs/chrono/latest/chrono/format/strftime/index.html) strftime specifiers:

```mix
print(date_format(1700000000, "%Y-%m-%d"))
print(date_format(1700000000, "%A, %d %B %Y"))
print(date_format(1700000000, "%H:%M"))
print(date_format(1700000000, "%I:%M %p"))
print(date_format(1700000000, "%a %j %s"))
```
```text
2023-11-15
Wednesday, 15 November 2023
08:13
08:13 AM
Wed 319 1700000000
```

Common specifiers:

| Token | Meaning | Token | Meaning |
|---|---|---|---|
| `%Y` | 4-digit year | `%H` | hour, 24h (00–23) |
| `%m` | month (01–12) | `%I` | hour, 12h (01–12) |
| `%d` | day of month (01–31) | `%M` | minute (00–59) |
| `%B` / `%b` | month name / abbrev | `%S` | second (00–60) |
| `%A` / `%a` | weekday name / abbrev | `%p` | AM/PM |
| `%j` | day of year (001–366) | `%z` | UTC offset (`+1000`) |
| `%s` | Unix timestamp | `%%` | a literal `%` |

`date_format` needs at least one argument:

```mix
print(date_format())
```
```text
Runtime error at line 1: date_format() expects at least 1 argument(s), got 0
```

### Local vs UTC — read this

`date_format` always renders in the **machine's local timezone** — there is no UTC-output variant and no timezone argument. If you need UTC output, format the offset into the string and reason about it yourself, or include `%z`/`%Z` so the offset is explicit:

```mix
print(date_format(1700000000, "%Y-%m-%d %H:%M:%S %z"))
```
```text
2023-11-15 08:13:20 +1000
```

## date_parse(s) — text → timestamp

`date_parse` converts a date string back into a Unix timestamp (a number). It tries three formats in order and returns on the first that matches:

1. **RFC 3339 / ISO 8601** with an explicit offset or `Z` — e.g. `2023-11-15T08:13:20+10:00`, `...Z`. The offset is honoured.
2. **`%Y-%m-%d %H:%M:%S`** (space-separated) — interpreted as **UTC**.
3. **`%Y-%m-%d`** (date only) — interpreted as **UTC midnight**.

```mix
print("" .. date_parse("2023-11-15T08:13:20+10:00"))
print("" .. date_parse("2023-11-15T08:13:20Z"))
print("" .. date_parse("2023-11-15 08:13:20"))
print("" .. date_parse("2023-11-15"))
```
```text
1700000000
1700036000
1700036000
1700006400
```

Note the difference: the offset form `+10:00` lands on `1700000000`, while the bare `2023-11-15 08:13:20` is read as UTC and lands on `1700036000` (10 hours later). **The space and date-only forms have no timezone, so they are always UTC** — supply an explicit offset when you mean local time.

Two edge cases of the format list:

- **Fractional seconds parse in the RFC 3339 form only**, and are truncated — `date_parse("2023-11-15T08:13:20.5+10:00")` returns `1700000000` (whole seconds; the result is always an integral number).
- **A `T`-separated string *without* an offset does not parse.** `date_parse("2023-11-15T08:13:20")` raises — RFC 3339 requires the offset (or `Z`), and the offset-less fallback format is the *space*-separated `%Y-%m-%d %H:%M:%S` only. Swap the `T` for a space, or append an offset.

Anything that matches none of the three formats raises a catchable runtime error:

```mix
print("" .. date_parse("15/11/2023"))
```
```text
Runtime error at line 1: date_parse: cannot parse '15/11/2023'
```

Wrap it in [`try`/`catch`](errors.md) when the input is untrusted:

```mix
try
  $ts = date_parse($input)
  print(date_format($ts, "%Y-%m-%d"))
catch $e
  print("bad date: " .. ("" .. $e))
end
```

### The round-trip gotcha (parse-then-format)

`date_parse` treats the timezone-less forms as **UTC**, but `date_format` renders in **local** time — so a naive round-trip shifts by your UTC offset:

```mix
$ts = date_parse("2023-11-15 08:13:20")   -- read as 08:13:20 UTC
print(date_format($ts, "%Y-%m-%d %H:%M:%S %z"))
```
```text
2023-11-15 18:13:20 +1000
```

Parsed at `08:13:20` UTC, re-rendered at `18:13:20 +1000`. This is correct behaviour (same instant, two zones), but it surprises people. To round-trip text-identically, either include an explicit offset in the input that matches your locale, or format with the offset printed (`%z`/`%Z`) so the shift is visible.

## duration_format — human-readable spans

`duration_format(secs)` turns a number of seconds (an *elapsed time*, not a timestamp) into a compact `Nd Nh Nm Ns` string. It drops any zero leading units, but always shows at least `0s` for a zero span. A float is truncated to whole seconds.

```mix
print(duration_format(0))
print(duration_format(45))
print(duration_format(90))
print(duration_format(3600))
print(duration_format(3661))
print(duration_format(8100))
print(duration_format(90061))
print(duration_format(125.9))
```
```text
0s
45s
1m 30s
1h
1h 1m 1s
2h 15m
1d 1h 1m 1s
2m 5s
```

The breakdown is days / hours / minutes / seconds (1 day = 86400s); there is no weeks/months unit. Zero-valued **leading and middle** units are omitted (`2h 15m` has no `0s`, `1h` has no minutes), but the result is never empty. A **negative** input clamps to `0s` (the seconds are cast to an unsigned count before splitting) — an elapsed span is never negative, so take `abs()` first if your subtraction can go either way. Pair it with a `time()` difference for an uptime/elapsed readout:

```mix
$elapsed = 5025
print("uptime " .. duration_format($elapsed))
```
```text
uptime 1h 23m 45s
```

## relative_time — "3h ago" / "in 5m"

`relative_time(ts)` compares a timestamp against **now** (`Utc::now()`) and renders the gap as a short relative phrase. Past times read `... ago`; future times read `in ...`.

```mix
print(relative_time(time() - 30))
print(relative_time(time() - 7200))
print(relative_time(time() - 172800))
print(relative_time(time() + 300))
print(relative_time(time() + 90000))
```
```text
30s ago
2h ago
2d ago
in 5m
in 1d
```

The granularity is a single largest unit, truncating: under 60s → `Ns`, under an hour → `Nm`, under a day → `Nh`, otherwise `Nd`. It picks one unit only (no `2h 5m` — `7200`s reads `2h`, and `7800`s also reads `2h`). For a precise span use `duration_format` on the difference instead.

## Patterns

### Stamp a log line

```mix
print(now_iso() .. "  starting job")
```
```text
2026-07-03T14:19:54.080883176+10:00  starting job
```

### "Last seen" in a table row

```mix
$rows = [
  { name: "alpha", last: time() - 45 },
  { name: "node1", last: time() - 9000 }
]
for each $r in $rows
  print($r["name"] .. ": " .. relative_time($r["last"]))
end
```
```text
alpha: 45s ago
node1: 2h ago
```

### Day-bucket a timestamp (group events by date)

```mix
$ts = 1700000000
$day = date_format($ts, "%Y-%m-%d")
print("bucket " .. $day)
```
```text
bucket 2023-11-15
```

### Compute a deadline and show it both ways

```mix
$deadline = time() + 3600
print("due " .. date_format($deadline, "%H:%M") .. " (" .. relative_time($deadline) .. ")")
```
```text
due 15:20 (in 1h)
```

## Gotchas

- **A timestamp is a number, an elapsed span is a number — don't confuse them.** `date_format`/`relative_time` take an absolute timestamp (seconds since epoch); `duration_format` takes a duration (seconds of elapsed time). `duration_format(time())` prints a meaningless `20637d 4h …` (the age of the epoch).
- **`date_format` is always LOCAL; `date_parse` treats offset-less input as UTC.** There is no timezone parameter on either. Naive parse→format round-trips shift by your UTC offset (see above) — include `%z` or an explicit offset to make it visible.
- **Newline or `;` separates statements** — `mix -c '$t = time(); print(date_format($t))'` is valid Mix. A shell-dispatch line still gives `;` its shell command-list meaning (see [shell](shell-mode.md)).
- **Concatenate with `..`, never `+`.** `print("at " .. date_format($t))` — `+` is numeric addition only.
- **`$name` is literal inside `"double quotes"`.** Build strings with `..` concat (`"day " .. $day`); only `${name}` interpolates. See [strings](strings.md).
- **Sub-second precision is dropped on format.** `date_format`/`relative_time` truncate the float to whole seconds; `now_iso()` keeps nanoseconds because it formats `Local::now()` directly.
- **Bad numeric input raises.** A numeric string (`"1700000000"`) works, but a non-numeric string — including `"inf"`/`"nan"`/`"1e999"`, rejected by strict coercion since 0.21 — raises `TYPE_MISMATCH` from `date_format`, `relative_time`, and `duration_format`. A timestamp outside chrono's representable range (for example `1e18`) raises `VALUE_OUT_OF_RANGE`; it never clamps to the epoch.
- **`date_parse` raises on no match** — only the three documented formats parse (and `T`-separated input *must* carry an offset). Wrap untrusted input in `try`/`catch`.
- **No date *math* helpers** (add-month, day-of-week-of, business-day). Do arithmetic on the raw timestamp (`$t + 86400` for +1 day) and read fields back via `date_format` strftime tokens like `%A`/`%j`.

## See also

- [math](math.md) — numeric operators and functions (timestamps are numbers; `abs`, `round`)
- [numbers](numbers.md) — numeric coercion rules, `to_number`/`is_number`
- [strings](strings.md) — `..` concat, interpolation, formatting output
- [errors](errors.md) — `try`/`catch` for `date_parse` failures
- [system](system.md) — `sleep(n)`, `run`/`run_rc` timeouts, process helpers
- [builtins index](builtins.md) — the full builtin catalogue
- [shell](shell-mode.md) — `mix -c` one-liner rules (newlines, classifier)
- chrono strftime reference — <https://docs.rs/chrono/latest/chrono/format/strftime/index.html>
- Run `mix help` for the category listing, or `mix what date_format` (also `time`, `date_parse`, `now_iso`, `duration_format`, `relative_time`) for a one-line description of any builtin.
