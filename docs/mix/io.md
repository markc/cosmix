# io — files & I/O builtins

The **io** builtin category — read, write, walk, and stat the filesystem, plus
stdin, path-string helpers, and the embedded SQLite client. Every example below
was run against **mix 0.21.2** and shows real output, except the sub-second
timestamp section, which was run against **mix 0.44.0** — the release that added
the field it demonstrates. List the category live with
`mix builtins io`; one-line help for any name with `mix what NAME`. (That listing
also shows `print` / `eprint` — those are *statements*, not builtin calls; see
[keywords](keywords.md).)

Mix's filesystem builtins are *native* `std::fs`/`std::os::unix` calls — there is
no `/bin/sh` in the loop. Reach for them instead of shelling out: `stat(p)` not
`run_rc("stat -c …")`, `chmod(p, 0o755)` not `run("chmod 755 …")`. They are
structured-return by design — `stat` hands back a map, `read_lines` a list, `walk`
a flat list of paths — so the result is data you index into, never string soup you
re-parse.

## Common semantics

- **Paths are plain strings.** A relative path resolves against the process CWD. No tilde expansion happens *inside* a builtin — `"~/x"` only expands at lex time in a double-quoted [string](strings.md) literal, so `read_file("~/.mixrc")` works because the lexer rewrites `~` to `$HOME`, but `read_file($p)` where `$p` holds a literal `"~/x"` does not.
- **Errors raise, they don't return a sentinel.** A failed `read_file` / `ls` / `mkdir` raises a catchable runtime error (the message names the call and path). Wrap a must-succeed step in `try/catch`, or pre-check with `exists` / `access` / `is_file` / `is_dir`. The exceptions are the predicates themselves (`exists`/`access`/`is_dir`/`is_file` return a `bool` for ordinary absence or denial) and `glob`, which returns an empty list for "no match", never an error.
- **Mode arguments take the VALUE, not the digits.** `chmod`/`write_new` want an octal *literal* — `0o755` (= 493), `0o600` (= 384) — or an octal *string* (`"0755"`). A bare `755` is the decimal number 755, not the mode (see [numbers](numbers.md) for why `0755` is a lex error and `0o755` is the value). That lex error is a Mix-*expression* hazard only — `0755` inside a command string handed to `run` (`run("install -m 0755 a b")`) is plain text for `/bin/sh` and works fine.
- **`stat`'s `ino`/`dev` are STRINGS.** They are `u64` and Mix numbers are f64, which loses precision above 2^53 — so they come back as text, ready to use verbatim as a dedupe key. Don't `to_number()` them.
- **Bytes vs strings.** `read_file`/`read_lines` require valid UTF-8 (they error on a bad byte); `read_file_bytes` carries the raw buffer through `Value::Bytes`. `write_file`/`append_file`/`write_new` write a `Value::Bytes` argument verbatim and stringify anything else.

## Reading files

```
read_file(path)          -> string   (whole file; errors if not UTF-8)
read_lines(path)         -> list      (lines, trailing \n stripped, trailing empty dropped)
head(path[, n])          -> list      (first n lines, default 10 — stops reading at n)
tail(path[, n])          -> list      (last n lines, default 10 — reads backwards from EOF)
line_count(path)         -> number   (count lines by streaming; works on non-UTF-8 files)
read_file_bytes(path)    -> bytes     (raw; binary-safe)
read_file_bytes(p, max)  -> bytes     (read at most `max` bytes — header-sniff)
```

```mix
write_file("/tmp/io/hello.txt", "line one\nline two\nline three\n")

print(read_file("/tmp/io/hello.txt"))
$lines = read_lines("/tmp/io/hello.txt")
print("count: " .. length($lines))
print("first: " .. $lines[0])
print("line_count: " .. line_count("/tmp/io/hello.txt"))
```

```text
line one
line two
line three

count: 3
first: line one
line_count: 3
```

`read_lines` strips each trailing newline (`\r\n` too — the `\r` is stripped
with it) and **drops the empty last line** a file ending in `\n` would otherwise
produce — so `"x\ny\n"` is two lines, not three. `line_count` streams the file
in blocks and never materializes a list — the right call when you only want the
count. It is byte-oriented (counts `\n`), so unlike the readers it works on
non-UTF-8 files too.

`head` and `tail` (v0.28.1) are the **no-slurp** twins of
`take(read_lines(p), n)` and `take(read_lines(p), -n)`: same line semantics,
same list result, but `head` stops reading after `n` lines and `tail` reads
64 KiB blocks backwards from EOF — the last 10 lines of a multi-GB log cost
kilobytes, not the file's size in RAM. `n` defaults to 10 and must be a
non-negative integer *number*; a numeric string or bool is a loud error, same
discipline as `read_file_bytes`'s cap. Reach for `read_lines`+`take` only when
you need the whole file anyway.

A missing file raises:

```mix
try
  read_file("/tmp/does-not-exist-xyz")
catch $e
  print("caught: " .. ("" .. $e))
end
```

```text
caught: read_file '/tmp/does-not-exist-xyz': No such file or directory (os error 2)
```

### Reading bytes (binary-safe)

`read_file_bytes` is the binary companion. The optional second argument caps the
read at *max* bytes via `File::take`, so you can sniff a header out of a
multi-megabyte file without slurping it all. The cap must be a real `Number`
(a numeric string or bool is rejected — a silent surprise on a read cap would be
a footgun). Pair it with the [bytes helpers](#bytes-helpers):

```mix
-- a file whose first bytes are the JPEG magic \xFF\xD8\xFF
$all = read_file_bytes("/tmp/io/img.bin")
print("full: " .. bytes_len($all))

$head = read_file_bytes("/tmp/io/img.bin", 4)
print("capped: " .. bytes_len($head))
```

```text
full: 34
capped: 4
```

To pull an ASCII header out of a not-quite-UTF-8 block, decode the capped buffer
with the explicit lossy escape hatch — `bytes_to_string($head, {lossy: true})`
(see below). For a strict-data `.mix` file, use `load_data(path)` — the
non-executing twin of [`include`](functions.md) that parses bare-key `key: value`
data without running it.

## Writing files

```
write_file(path, content)        overwrite (or create); follows symlinks
append_file(path, content)       create-if-missing, then append
write_new(path, content, mode)   atomically create; FAILS if path exists
```

```mix
write_file("/tmp/io/log.txt", "first\n")
append_file("/tmp/io/log.txt", "second\n")
print(read_file("/tmp/io/log.txt"))
```

```text
first
second
```

### `write_new` — atomic, mode-at-creation, secret-safe

`write_new` is `O_EXCL` at the syscall level (`create_new(true)`): it **fails if
the path already exists**, with no TOCTOU window between a check and the write.
The mode is applied *at creation* via `OpenOptions::mode()`, so the file is never
briefly umask-permissioned — secret material hits disk at the configured mode
from the very first byte. This is the call for DKIM keys and similar single-shot
secrets; for ordinary mutable writes use `write_file`.

```mix
write_new("/tmp/io/secret.key", "TOPSECRET\n", 0o600)
print("perm: " .. stat("/tmp/io/secret.key")["perm"])    -- 0o600 == 384

try
  write_new("/tmp/io/secret.key", "x", 0o600)            -- already exists
catch $e
  print("second write_new: refused (exists)")
end
```

```text
perm: 384
second write_new: refused (exists)
```

> `0o600` prints as `384` because Mix numbers are decimal f64 — `0o600` is just a
> source spelling of the value 384. `stat`'s `perm` field is that same number;
> feed it straight back into `chmod`.

## Existence & type tests

```
exists(path[, opts])  -> bool   opts: {follow_symlinks: true|false}  (default true)
access(path, mode)     -> bool   mode: non-empty combination of r, w, x, f
is_dir(path)          -> bool
is_file(path)         -> bool   (regular file only — false for a dir or a dangling link)
```

```mix
print("" .. exists("/etc/hosts"))     -- true
print("" .. is_dir("/etc"))           -- true
print("" .. is_file("/etc/hosts"))    -- true
print("" .. is_file("/etc"))          -- false  (it's a dir)
print("" .. is_file("/no/such"))      -- false  (missing — no error)
```

```text
true
true
true
false
false
```

### Kernel permission checks — `access`

`access(path, mode)` asks the kernel whether this process can access a path
*right now*. `mode` is a non-empty string containing any combination of `r`,
`w`, and `x`, in any order. `f` means existence (`F_OK`): it may appear alone,
and is redundant when combined with another letter. A repeated letter, an empty
string, or any other character raises rather than turning a typo into a
permissive check.

```mix
if access(".git/hooks/pre-commit", "x") then
  print("the kernel will not refuse to execute this on permission grounds")
end

print("" .. access("/etc/hosts", "f"))       -- existence
print("" .. access("/no/such/path", "r"))    -- false, no catch needed
```

This is the sound answer to a question that `stat(path)["perm"]` cannot answer.
Mode bits are only part of the kernel's permission calculation: a POSIX ACL can
grant or deny a named user independently of what `0o755` appears to say.
`access` uses `faccessat(AT_FDCWD, path, mask, AT_EACCESS)`, so ACLs are honoured
and the check uses the process's **effective** uid/gid — the identity under which
this process would actually do the thing.

On Linux that is issued as the **`faccessat2(2)` syscall directly**, not through
the C library's `faccessat` wrapper, and the difference is not academic. The
older kernel call takes no flags, so glibc implements `AT_EACCESS` itself — and
where `faccessat2` is unavailable (glibc ≤ 2.32, or a kernel before Linux 5.8)
that emulation decides from `fstatat(2)` mode bits and, in the man page's own
words, *"does not take ACLs into account"*. That is exactly the arithmetic this
builtin exists to replace, so rather than fall back to it silently, `access`
**raises** on such a kernel. The answer is the kernel's or there is no answer. A
caller that genuinely wants mode arithmetic can read `stat(path)["perm"]` and own
that choice out loud.

That is deliberately *not* the same rule as POSIX `access(2)`, which uses the
**real** ids; some programs (git's hook lookup among them) use that older call.
The two agree except under a setuid or setgid binary, where real and effective
diverge. Mix is neither, and neither is a shell, a daemon, or a `sudo`ed
command, so predicting such a program's answer with this builtin is sound
wherever the two ids match — which is everywhere Mix normally runs, but is an
assumption worth naming rather than one the builtin can enforce.

On Linux `x` also accounts for a **`noexec` mount**, which no combination of
mode bits reveals: a 0755 file on a `noexec` filesystem answers `false`, exactly
as `execve` would. On a *directory*, `x` means search permission rather than
execute, so it does not answer "can programs be run from here" — for that, ask
about a file inside it.

Symlinks are followed deliberately: the question is whether the thing they
resolve to can be read, written, or executed. Ordinary negative answers return
`false`, including a missing path, a denied permission, a non-directory path
component, a symlink loop, an overlong path, a read-only filesystem on a write
query, a `w` query against a file currently being executed, and a `w` query
against an immutable file. Malformed input (including an interior NUL in the
path) and an unexpected syscall failure raise.

That last case is the one genuinely ambiguous errno: a seccomp filter or an LSM
that blocks the syscall itself also reports `EPERM`, and is reported here as an
ordinary "no". That is the safe direction — a caller asking "may I?" is better
served by a false than by an exception thrown at code that was only trying to
describe a file — but on a sandboxed system a blanket `false` from `access` is
worth reading as *"the question could not be put"* rather than as a fact about
the file.

The answer is a snapshot, not a lease: a mode change, an ACL change or a remount
between the check and the use invalidates it. For a step that must succeed,
attempt it inside `try`/`catch` and let the failure be authoritative; `access` is
for reporting, for choosing a branch, and for saying *why* something will not
work before trying it.

**`exists` answers two different questions and you have to pick.** By default it
**follows symlinks**, so it means *"can I open something here"* — and a dangling
link reads as **absent**. `exists(path, {follow_symlinks: false})` is the `lstat`
form: *"is there an entry at this name"*, which sees the link itself (v0.39.0;
same option name and default as `stat`).

```mix
symlink("nowhere", "/tmp/io/dangling")
print("" .. exists("/tmp/io/dangling"))                            -- false
print("" .. exists("/tmp/io/dangling", {follow_symlinks: false}))  -- true
```

```text
false
true
```

Use the default for *"is there a file I can read or run"*. Use the `lstat` form
for *"is this name free"*, *"is there something here I must not destroy"*, and
*"is there something here I must not silently ignore"* — those three all have to
see a dangling link, and the default hides it. (The same distinction, one layer
down, is why `write_new` is the right way to claim a pathname: see
[Claim the temp name before you write into it](#atomic-replace--rename).)

`realpath` canonicalises a path — it resolves every symlink and `.`/`..` to the absolute real path (like `readlink -f` / `realpath(3)`). The path **must exist**; a missing component, a symlink loop, or a non-UTF-8 resolved path returns `nil` (never an error), so the caller decides. It is the same canonicalisation `require`/`include` use for their per-path module cache. It is **normalisation only, not a race-free authorization primitive** — canonicalise-then-use is not atomic. For an exec/open safety check, operate on the **returned canonical path** (which has no symlinks left to re-traverse), not the original, so a symlink hop can't be repointed between the check and the use.

```
realpath(path)  -> string | nil   (absolute real path, or nil if unresolvable)
```

```mix
print("" .. realpath("/usr/bin/traceroute"))  -- e.g. /usr/bin/traceroute.db (Debian alt-link)
print("" .. realpath("/etc/../etc/hosts"))    -- /etc/hosts
print("" .. realpath("/no/such/path"))        -- nil  (missing — no error)
```

These never raise — they answer a question, so a missing path is simply `false`.
Use them to guard a read that *would* raise.

## Listing directories — `ls`, `glob`, `walk`

```
ls([dir])                     one level, names only (sorted; default ".")
glob(pattern)                 paths matching a glob (* ? and ** globstar)
walk(dir[, opts])             recursive list of paths under dir (sorted)
```

`ls` returns the **entry names** in a single directory, sorted, with no path
prefix. It raises on a missing or unreadable directory.

```mix
for each $e in ls("/tmp/io")
  print($e)
end
```

```text
a.txt
b.log
sub
```

`glob` matches a shell-style pattern and returns **full matching paths**, sorted.
`*` and `?` match within one path component; `**` is a globstar that descends
through zero or more directory levels. No match → an empty list (not an error).
A relative pattern starts from `.` (with no stray `./` prefix on results); an
absolute pattern starts from `/`.

```mix
-- top level only
for each $g in glob("/tmp/io/*.txt")
  print($g)
end
print("---")
-- recurse with **
for each $g in glob("/tmp/io/**/*.txt")
  print($g)
end
```

```text
/tmp/io/a.txt
---
/tmp/io/a.txt
/tmp/io/sub/c.txt
/tmp/io/sub/deep/d.txt
```

`walk` is the depth-first recursive walk. By default it returns **files only**;
the options map flips the defaults:

- `max_depth` (number, default unlimited) — nesting depth relative to `dir`; `max_depth: 0` returns only the direct children.
- `include_dirs` (bool, default `false`) — include directory entries too.
- `follow_symlinks` (bool, default `false`) — follow symlink dirs (loop-safe; the walker tracks visited inodes).

`max_depth` accepts a number or numeric string. If the option is present but
cannot be parsed as a number, `walk` raises `TYPE_MISMATCH`; it never silently
discards the limit and performs an unlimited traversal.

`walk` errors only on a **missing** top-level `dir`. Everything else is
best-effort: an unreadable directory is silently skipped — even the top-level
one (it exists, so the walk proceeds and simply finds nothing, returning `[]`) —
and so is an individual entry that fails to stat. One bad file deep in a subtree
shouldn't abort the whole walk.

```mix
print("--- files only, full depth ---")
for each $p in walk("/tmp/io")
  print($p)
end
print("--- direct children, include dirs ---")
for each $p in walk("/tmp/io", {max_depth: 0, include_dirs: true})
  print($p)
end
```

```text
--- files only, full depth ---
/tmp/io/a.txt
/tmp/io/b.log
/tmp/io/sub/c.txt
/tmp/io/sub/deep/d.txt
--- direct children, include dirs ---
/tmp/io/a.txt
/tmp/io/b.log
/tmp/io/sub
```

The split: `ls` = one level / names, `glob` = pattern / full paths, `walk` =
recurse / full paths. Reach for `walk` over a hand-rolled `glob("**")` loop when
you need depth limits or directory entries.

## Creating directories — `mkdir`

`mkdir(path)` creates the directory **and any missing parents** (it is
`create_dir_all`, i.e. `mkdir -p`), and is idempotent — calling it on an existing
directory is a no-op, not an error.

```mix
mkdir("/tmp/io/a/b/c")
print("" .. is_dir("/tmp/io/a/b/c"))   -- true
mkdir("/tmp/io/a/b/c")                 -- again: fine, no error
print("idempotent ok")
```

```text
true
idempotent ok
```

`mkdir(path, {parents: false})` creates **only the final component** (plain
`create_dir`) and raises if the parent is missing. Reach for it when the parent
was placed deliberately and its disappearance is news: the recursive default
would quietly re-create it — and every level above it — so a script that checked
`is_dir($TMPDIR)` a moment earlier ends up manufacturing the very directory it
meant to refuse, and its cleanup removes only the leaf it knows about.

```mix
mkdir("/tmp/io/exists")
mkdir("/tmp/io/exists/leaf", {parents: false})     -- fine: the parent is there
print("" .. is_dir("/tmp/io/exists/leaf"))
try
  mkdir("/tmp/io/absent/leaf", {parents: false})
catch $e
  print("refused")
end
print("" .. exists("/tmp/io/absent"))              -- the parent was NOT created
```

```text
true
refused
false
```

The option map is checked strictly, because every way of misspelling a safety
switch would otherwise select the unsafe default in silence. `parents` is the
only key `mkdir` accepts and it must be a boolean; `{parent: false}` (singular)
and `{parents: "false"}` (a string, which is truthy) both raise rather than
recursing.

```mix
try
  mkdir("/tmp/io/x/y", {parent: false})
catch $e
  print("unknown key refused")
end
try
  mkdir("/tmp/io/x/y", {parents: "false"})
catch $e
  print("non-boolean refused")
end
print("" .. exists("/tmp/io/x"))                   -- neither call created it
```

```text
unknown key refused
non-boolean refused
false
```

## Advisory locks — `flock`, `funlock`

```
flock(path)                              exclusive, non-blocking
flock(path, {shared: true})              shared, non-blocking
flock(path, {wait: 2.5})                 wait for up to 2.5 seconds
funlock(path)                            release this process's lock
```

`flock` opens the file read/write, creating it with mode `0644` (subject to the
process umask), then takes a kernel advisory lock with `flock(2)`. The default is
exclusive and non-blocking: `true` means acquired and `false` means another
process currently holds an incompatible lock. Contention is normal control flow,
not an exception. A positive `wait` retries without busy-spinning until the
deadline, then returns `false`; genuine open or locking failures raise.

```mix
$lock = "/tmp/deploy.lock"
if not flock($lock, {wait: 5}) then
  die "another deploy is running"
end

try
  print("deploy while the kernel owns the lock lifetime")
catch $e
  funlock($lock)
  die $e
end
funlock($lock)
```

The fd lives in a process-global registry under the file's canonical path, so it
stays open after `flock` returns and the kernel releases it automatically when
the process exits — including signal death. Calling `flock` again for the same
canonical path in the same process returns `true` without opening a second
retained fd. The first acquisition fixes shared versus exclusive mode until
`funlock`; a repeated call is idempotent, not an upgrade or downgrade.

`funlock` returns `true` when it released a held fd and `false` when this process
held no lock for that path. The not-held case never raises.

These are **advisory** locks: every cooperating process must use `flock` on the
same file. Do not remove, rename, or replace a live lock file. A path can then
name a new inode while an older process still holds the old inode locked, letting
both processes proceed. Leave the lock file in place permanently; only acquire
and release its kernel lock.

## Copying & removing — `copy`, `copy_tree`, `remove`, `remove_dir`

```
copy(src, dst)          copy a single file; overwrites dst, preserves the source mode
copy_tree(src, dst)     recursive dir copy — files (mode kept) + symlinks (as symlinks);
                        creates dst and merges into an existing one
remove(path)            remove a file/symlink; no-op if already gone (rm -f)
remove_dir(path)        recursive remove of a dir + contents; no-op if gone (rm -rf)
```

`copy_tree` mirrors the parts of `cp -a` scripts actually need: it recreates the
tree, copies files with `std::fs::copy` (which carries the permission bits), and
recreates symlinks **as symlinks** (target verbatim — no dereference). `remove` and
`remove_dir` follow `rm -f` / `rm -rf` semantics: an already-missing path is a no-op,
not an error (so a "clear then rebuild" step needs no `exists()` pre-check), while a
real failure (a permission error, or `remove` aimed at a directory) still raises.

```mix
mkdir("/tmp/io/rc/sub")
write_file("/tmp/io/rc/a.mix", "-- alias file\n")
copy_tree("/tmp/io/rc", "/root/.rc")   -- e.g. bundling ~/.rc into an image rootfs
remove_dir("/tmp/io/rc")               -- rm -rf the staging copy
remove_dir("/tmp/io/rc")               -- again: fine, already gone
print("copied + cleaned")
```

```text
copied + cleaned
```

## Atomic replace — `rename`

```
rename(src, dst)        rename(2) — moves src onto dst within one filesystem
```

`rename` exists for the one guarantee `copy` cannot give: replacing an existing
`dst` is **atomic**. A concurrent reader — or a `git` about to `exec` a hook, or a
daemon re-reading its config — sees either the old file or the new one, never a
half-written one. That makes it the second half of the safe in-place update:
**write a temp file beside the target, `chmod` it, then `rename` it over the
target.** `write_file` alone truncates first, so a crash or a permission failure
mid-write leaves the target destroyed.

Errors surface verbatim rather than being papered over. `EXDEV` (a cross-filesystem
move) raises — that is a genuinely different operation with different failure
modes, and silently falling back to copy + remove would hand back the
non-atomicity the caller came here to avoid. A missing `src` raises `ENOENT`;
unlike `remove`, this is never a no-op, because renaming something that isn't
there is always a caller bug.

```mix
$live = "/tmp/io/hooks/pre-commit"
$tmp  = $live .. ".tmp"
write_file($tmp, "#!/bin/sh\nexec my-gate\n")
chmod($tmp, 0o755)
rename($tmp, $live)                    -- the swap is all-or-nothing
print(read_file($live))
```

```text
#!/bin/sh
exec my-gate
```

**Claim the temp name before you write into it.** A *fixed* staging name like
`pre-commit.tmp` is a pathname anyone can get to first, and `write_file` follows
a symlink sitting there — so `pre-commit.tmp -> pre-commit` turns the "safe"
temp write into a write straight through the live target, and the `rename` then
points the target at itself. `write_new` is `O_EXCL`: it fails on *any* existing
entry, including a dangling symlink, and never follows one. Use it to take the
name, and the rest of the sequence is honest.

```mix
write_new($tmp, "", 0o755)             -- refuses if anything is already there
write_file($tmp, $body)
rename($tmp, $live)
```

Also note `rename` reports the *client's* view. On NFS a client can be told the
rename failed after a server crash even though the server performed it, so
"raised" is not proof the old file survived; re-read the target if you must know.

## Symbolic links — `symlink`, `read_link`

```
symlink(target, linkpath)   symlink(2) — creates linkpath pointing at target
read_link(path) -> string   readlink(2) — the target exactly as stored
```

Argument order is `symlink(2)`'s, **not** `ln`'s reading order: the target comes
first, the link to create second. `target` is stored verbatim and is neither
resolved nor validated, so a relative target resolves against the *link's own*
directory, and creating a dangling link is legal — both are usually the point.
`linkpath` must not exist (`EEXIST`); `remove` deletes a symlink itself rather
than what it points at.

`read_link` is not `realpath`. It hands back what the link literally says,
relative or dangling and unresolved; `realpath` answers "where does this land".
Auditing a link — *is this temp file secretly aimed at the file next to it?* —
needs the unresolved answer. It raises `EINVAL` on anything that is not a
symlink, so test first with `stat(path, {follow_symlinks: false})`, which is the
`lstat` form and the only way to `stat` the link rather than its target.

```mix
symlink("../elsewhere/absent", "/tmp/io/link")
print(read_link("/tmp/io/link"))
print(stat("/tmp/io/link", {follow_symlinks: false})["is_symlink"])
print(exists("/tmp/io/link"))          -- follows: a dangling link reads as absent
```

```text
../elsewhere/absent
true
false
```

That last line is the trap worth remembering: plain `exists` follows the link, so
a dangling symlink answers "nothing is here" while very much being something. Ask
the other question — `exists(path, {follow_symlinks: false})` — whenever you mean
"is this name taken" rather than "can I open this".

## Permissions & ownership — `chmod`, `chown`

```
chmod(path, mode)            mode as octal literal 0o755 or string "0755"
chown(path, uid, gid)        numeric uid/gid only; follows symlinks
```

Native syscalls — don't `run("chmod …")`. Both follow symlinks (the mode/owner is
applied to the link's *target*, standard shell semantics). `chmod`'s mode is the
**value** (`0o755`, range `0..=0o7777`); a number out of range or with a fraction
is rejected. `chown` takes numeric uid/gid only — no name resolution — and
requires real `Number` arguments (a bool or numeric string is refused, so
`chown(p, true, false)` can't silently become uid 1 / gid 0).

```mix
write_new("/tmp/io/f", "x\n", 0o600)
print("before: " .. stat("/tmp/io/f")["perm"])   -- 384  (0o600)
chmod("/tmp/io/f", 0o644)
print("after:  " .. stat("/tmp/io/f")["perm"])   -- 420  (0o644)
```

```text
before: 384
after:  420
```

| octal literal | decimal value (what `perm`/`mode` print) |
|---|---|
| `0o600` | 384 |
| `0o644` | 420 |
| `0o755` | 493 |

## Inspecting metadata — `stat`

`stat(path)` returns a map; `stat(path, {follow_symlinks: false})` is `lstat`
(reports the link itself). Fields (a frozen surface — scripts index into them):

| field | type | meaning |
|---|---|---|
| `uid` `gid` `nlink` `size` | number | owner / group / hard-link count / bytes |
| `mode` | number | full `st_mode` (type bits included) |
| `perm` | number | `mode & 0o7777` — round-trips into `chmod` |
| `ino` `dev` | **string** | inode / device (u64 → text, see semantics) |
| `ctime` `mtime` `atime` | number | epoch **seconds** (f64, whole-second) |
| `ctime_nsec` `mtime_nsec` `atime_nsec` | number | sub-second part, `0..=999999999` (v0.44.0) |
| `is_file` `is_dir` `is_symlink` | bool | type flags |

```mix
write_new("/tmp/io/k", "hello\n", 0o600)
$st = stat("/tmp/io/k")
print("size:  " .. $st["size"])
print("perm:  " .. $st["perm"])
print("ino:   " .. $st["ino"])           -- a STRING
print("file?: " .. ("" .. $st["is_file"]))
```

```text
size:  6
perm:  384
ino:   1909
file?: true
```

### "Did this file change?" needs the timestamp *pair*

Whole seconds cannot answer that question. A rewrite that puts *equal* bytes
back inside the same second is invisible to a content comparison (the bytes
match) **and** to an `mtime` comparison (the second matches) — so a tamper check
built on those two reads it as untouched. This is not hypothetical: a hook
installer was rewriting live git hooks on every run and the check stayed green.

Compare `(mtime, mtime_nsec)` together:

```mix
fn changed($path, $was)
  $now = stat($path)
  return $now["mtime"] != $was["mtime"] or $now["mtime_nsec"] != $was["mtime_nsec"]
end
```

The sub-second part is a **separate** field rather than one combined nanosecond
timestamp on purpose. Mix numbers are `f64`, exact for integers only up to
2^53; a nanosecond epoch passed that about 104 days after 1970, so a combined
value would silently round. Split, both halves are exact.

**The pair narrows the blind spot; it does not close it.** `mtime_nsec` reports
what the filesystem recorded, and timestamp granularity is a filesystem
property, not a guarantee — a filesystem that stores whole seconds reports `0`,
and even one that stores nanoseconds is not obliged to give two writes distinct
values.

Which field catches a change depends on *how* the writer writes:

- A writer that **replaces** the file — new file, then `rename()` over the old
  one, the way any careful writer updates a live file — gets a **new inode**.
  `ino`/`dev` catch that on their own, whether or not the timestamps are
  compared. This is the common case, and a check that compares only timestamps
  is doing less work than it looks like it is.
- A writer that rewrites **in place** keeps the inode, so `ino`/`dev` say
  nothing. If the new bytes differ, `size` or the content still show it. The
  case that needs the timestamps is the byte-identical one — same inode, same
  size, same bytes — where `(mtime, mtime_nsec)` is the field pair a caller
  normally compares, and only helps if the sub-second half is compared too when
  the rewrite lands inside one second. `ctime` moves as well; see below for what
  that is worth.

So compare identity *and* the timestamp pair; neither subsumes the other.

Both are still read-side signals, and neither survives a deliberate adversary:
`mtime` is writable — `utimensat` lets a file's owner set any value it likes,
including the previous one — so a change can be made and its evidence put back.

`ctime` is the exception worth knowing. It records when the *inode* last
changed, and no syscall accepts a caller-supplied value for it: a write marks
it, and the `utimensat` used to put `mtime` back marks it again, so the cover-up
signs itself. That is why "no `stat` field sees a byte-identical in-place
rewrite" would be wrong, and why `(ctime, ctime_nsec)` is worth comparing
alongside the `mtime` pair.

It is not proof, in two separate ways. It is a *timestamp*, so the granularity
paragraph above applies to it unchanged — a coarse filesystem, or two events
inside one tick, record the same pair for both, and an unchanged `ctime` is
therefore evidence of nothing having *been recorded*, not of nothing having
happened. And it is the *inode's* clock rather than the content's, so a `chmod`,
a `chown` or a new hard link moves it with no content change at all, while
anyone who can set the system clock or write the raw device moves it wherever
they like. Read it as tamper-*evidence* at best: a changed `ctime` says
something touched the inode, and does not say what.

A check that has to be sound needs evidence from the write side — the
writer announcing its intent before it writes, or an append-only log. A kernel
watch (`inotify`) is better than polling but is not proof either: it can miss
what happened before the watch was established, and its queue can overflow
(`IN_Q_OVERFLOW`) and drop events. Treat `stat` as the cheap first filter, not
the proof.

### The symlink mixed view

`stat` follows symlinks by default, but `is_symlink` is **always** derived from
`lstat`, so it flags the path itself regardless of the follow mode. When
following, the *other* fields describe the target while `is_symlink` flags the
link — a deliberate mixed view matching how callers reason about links:

```mix
-- /tmp/io/link.txt -> /tmp/io/target.txt (a regular file)
$lnk = stat("/tmp/io/link.txt", {follow_symlinks: false})   -- lstat
print("lstat:  is_symlink=" .. ("" .. $lnk["is_symlink"]) .. " is_file=" .. ("" .. $lnk["is_file"]))

$tgt = stat("/tmp/io/link.txt")                             -- follow
print("follow: is_symlink=" .. ("" .. $tgt["is_symlink"]) .. " is_file=" .. ("" .. $tgt["is_file"]))
```

```text
lstat:  is_symlink=true is_file=false
follow: is_symlink=true is_file=true
```

Use `ino`/`dev` straight as a dedupe-set key for a `walk` that should visit each
real file once — never `to_number()` them:

```mix
$seen = {}
for each $p in walk("/srv")
  $st = stat($p)
  $key = $st["ino"]
  if $seen[$key] == nil then
    $seen[$key] = true
    -- ... process $p once ...
  end
end
```

## Path-string helpers (pure, no filesystem)

These operate purely on the path *string* — they never touch disk, so they work
on a path that doesn't exist.

```
basename(path)    filename component                ("/a/b/x.eml" -> "x.eml")
dirname(path)     directory component               ("/a/b/x.eml" -> "/a/b")
extname(path)     extension WITH the leading dot     ("/a/x.eml"   -> ".eml")
path_join(a, b)   join with the native separator     ("/a", "x")   -> "/a/x"
path_parts(path)  -> {dir, base, stem, ext}          (ext has NO dot)
```

```mix
$p = "/srv/mail/inbox/msg.2.eml"
print("basename: " .. basename($p))
print("dirname:  " .. dirname($p))
print("extname:  " .. extname($p))
print("join:     " .. path_join("/srv/mail", "new.eml"))

$pp = path_parts($p)
print("dir=" .. $pp["dir"] .. " base=" .. $pp["base"] .. " stem=" .. $pp["stem"] .. " ext=" .. $pp["ext"])
```

```text
basename: msg.2.eml
dirname:  /srv/mail/inbox
extname:  .eml
join:     /srv/mail/new.eml
dir=/srv/mail/inbox base=msg.2.eml stem=msg.2 ext=eml
```

> **`extname` keeps the dot, `path_parts.ext` drops it** — different consumers, a
> deliberate split. A path with no extension gives `""` from `extname`, and
> `basename` of a trailing-slash path returns the last real component
> (`basename("/srv/mail/")` → `"mail"`).

## Standard input — `readline`, `read_stdin`

```
readline([prompt])   read one line from stdin (trailing \n stripped); optional prompt
read_stdin()         read ALL of stdin to EOF as one string
```

`read_stdin` is the call for a pipe or a hook payload — it slurps the whole stream
to EOF. `readline` reads a single line and, if given a prompt argument, writes it
to stdout (flushed) first — the interactive-input path. Both read UTF-8 *text*
and both **raise** on a stream that will not decode, matching `read_file`;
binary payloads belong in a file read via `read_file_bytes`.

Until 0.33.0 an undecodable stream returned an **empty string** instead. That is
indistinguishable from EOF, and it is silent: a `pre-push` hook reading git's ref
list saw one non-UTF-8 ref name as no refs at all, iterated nothing, and exited 0
having checked nothing. Catch it with `try` if a caller genuinely wants to
continue past undecodable input.

```mix
-- echo 'piped line 1\npiped line 2' | mix script.mix
$in = read_stdin()
print("got " .. length($in) .. " chars")
print("first line: " .. split($in, "\n")[0])
```

Piped in (`printf 'piped input line 1\npiped input line 2\n' | mix -c '…'`):

```text
got 38 chars
first line: piped input line 1
```

For richer stdin handling see [strings](strings.md) (`split`, `trim`) — a common
pattern is `for each $line in split(read_stdin(), "\n")`.

## Bytes helpers

The bytes type (`Value::Bytes`) carries raw 8-bit data without a UTF-8
round-trip. These convert to and from it (they live in the `system` category but
are the natural companions to `read_file_bytes`):

```
bytes_len(b)               byte length of a Value::Bytes buffer
string_to_bytes(s)         UTF-8 encode a string -> bytes
bytes_to_string(b)         decode bytes -> string (strict UTF-8; errors on bad byte)
bytes_to_string(b, {lossy: true})   from_utf8_lossy decode (bad bytes -> U+FFFD)
```

All three are **strict about their argument type** — `string_to_bytes` rejects a
non-string, `bytes_to_string`/`bytes_len` reject a non-bytes — so a `to_mix_string`
placeholder like `<bytes:N>` can never silently leak in.

```mix
$b = string_to_bytes("héllo")
print("byte length: " .. bytes_len($b))     -- 6 (é is 2 UTF-8 bytes)
print("round-trip:  " .. bytes_to_string($b))

-- sniff an ASCII header out of a non-UTF-8 head with the lossy escape hatch
$head = read_file_bytes("/tmp/io/img.bin", 4)
print("lossy: " .. length(bytes_to_string($head, {lossy: true})))
```

```text
byte length: 6
round-trip:  héllo
lossy: 4
```

Note `bytes_len` counts **bytes**, while `length` on a string counts **codepoints**
(`length("héllo")` is 5) — see [strings](strings.md) for the codepoint / byte /
grapheme split.

## SQLite — `sqlopen`, `sqlexec`, `sqlclose`

The io category includes an embedded SQLite client (feature-gated `sqlite`; the
`mix` binary turns it on). Handles are plain numbers:

```
sqlopen(path)              open READ-ONLY               -> numeric handle
sqlopen(path, "rw")        open read-write (WAL mode, 5s busy timeout)
sqlexec(h, sql[, params])  run one statement            -> rows or {affected}
sqlclose(h)                close the handle (errors on an unknown handle)
```

`sqlexec` returns a **list of row maps** for anything that produces columns
(`SELECT`, row-returning `PRAGMA`s) and `{affected: n}` for a write — decided by
SQLite itself (`sqlite3_stmt_readonly`), not by keyword-sniffing the SQL, so
`REPLACE` and `WITH … INSERT` take the write path correctly. Column values map
NULL → `nil`, INTEGER/REAL → number, TEXT → string; a BLOB comes back as the
placeholder string `<blob N bytes>`, not bytes.

`params` binds `?` placeholders — a list binds positionally, a single non-list
value is one bind. Binds are **TYPED** (since 0.21): `nil` → NULL, bool →
INTEGER 0/1, whole number → INTEGER, fractional → REAL, string → TEXT, bytes →
BLOB; a list/map param is a loud error. (Before 0.21 every param was bound as
TEXT, so `nil` arrived as the 3-char string `"nil"`.) Always bind data — never
splice values into the SQL string.

```mix
$db = sqlopen("/tmp/io/t.db", "rw")
sqlexec($db, "CREATE TABLE t (id INTEGER, name TEXT, score REAL)")
print("insert: " .. json_encode(sqlexec($db, "INSERT INTO t VALUES (?, ?, ?)", [1, "alice", 2.5])))
print("rows:   " .. json_encode(sqlexec($db, "SELECT * FROM t WHERE id = ?", 1)))
sqlclose($db)
```

```text
insert: {"affected":1}
rows:   [{"id":1,"name":"alice","score":2.5}]
```

A write against a read-only (default) handle raises `attempt to write a readonly
database` — open with `"rw"` when you mean to mutate.

## When to shell out instead

The native builtins cover read/write/list/stat/perms. For an operation Mix has no
builtin for — `cp -a`, `rsync`, `tar`, `mv` across filesystems — call the real
tool via [`run`/`run_rc`](system.md) (both take an optional `{timeout: seconds}`
opts map since 0.21, so a hung copy can't wedge the script), remembering the
PATH is minimal inside `/bin/sh`, so use a full binary path:

```mix
$r = run_rc("/usr/bin/install -m 0755 /tmp/io/a.txt /tmp/io/b.txt")
if $r.rc != 0 then
  print("install failed: " .. $r.stderr)
end
```

## See also

```
mix builtins io       list every io builtin with its one-line description
mix what NAME         one-line description of a single builtin (e.g. mix what stat)
mix help              the full categorized builtin reference
```

- [strings](strings.md) — codepoint vs byte vs grapheme; `split`, `trim`, `~` expansion
- [numbers](numbers.md) — why `0o755` is the mode value and `0755` is a lex error
- [running-commands](system.md) — `run`/`run_rc`/`run_stream` for shelling out
- [data](data.md) — `json_parse`, `load_data`, `data_encode` for structured files
- [modules](functions.md) — `source` / `include` for loading `.mix` code (vs `load_data` for inert data)
- [capabilities](capabilities.md) — the FsRead/FsWrite gating an embedder can apply to these builtins
- [builtins index](builtins.md) · [mix repo](https://github.com/markc/cosmix)
