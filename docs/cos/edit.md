# cosmix-editd — the `edit` citizen

**`cosmix-editd` registers the Bus service `edit`: shared text buffers that
humans and agents edit together.** A buffer has a revision number, a log of
who changed what, a separate undo lane for each origin, named anchors that
follow the text, atomic saves, and a change feed a mirror can follow exactly.
It is the core of **ced**, the Cosmix editor. The ced app (E1) is one
frontend of this daemon. An agent that sends `edit.*` verbs is another, and
both have the same standing.

The text engine is `cosmix-edit-core`. That crate is pure Rust with no Bus
code and no async runtime. It builds on a vendored subset of Microsoft's
[msedit](https://github.com/microsoft/edit) (MIT): the gap buffer and the SIMD
line scanner. See [libraries](libraries.md).

> **Volatile.** Buffers live in memory only. If the daemon stops, crashes or
> restarts, every unsaved edit is lost. `edit.info` says so
> (`"volatile": true`), as does the `lifecycle.volatile` prop. On SIGTERM the
> daemon logs each dirty buffer (id, path, rev) and exits within 10 s. Crash
> recovery files are planned for E1. Save often, or `edit.save` from your
> script.

## Running it

```sh
/opt/cosmix/bin/cosmix-editd serve        # registers `edit`, READY=1 once registered
/opt/cosmix/bin/cosmix-editd --version    # cosmix-editd <semver> (<sha12>, built …)
```

The public `src/_etc/systemd-user/cosmix-editd.service` is a user unit with
`Type=notify` and `WantedBy=default.target`. It is not tied to a graphical
session, because agents use the daemon headless. The daemon runs as the user
whose files it edits: `~/…` paths expand against the daemon's `HOME`.

## Conventions

- **Arguments** are a JSON object, which is the body a Mix `send` builds from
  `key=value` args. Nested maps and lists are written directly:
  `ops=[{op: "insert", at: "end", text: "x"}]`. **Never add `body=`** to an
  `edit` send, because it switches Mix to header routing and the other args
  stop reaching the daemon as a JSON body.
- **Success** is `rc 0`. A **refusal** is `rc 10` with a body shaped
  `{"error_code","message","reason"?,"buffer"?,"rev"?,…}`. On a plain kv send,
  Mix puts that body's raw JSON text in `$result`, so read the fields from
  `$reply`, for example `$reply.error_code` and `$reply.rev`.
- **Codes:** `INVALID_ARGUMENT NOT_FOUND CONFLICT RESOURCE_LIMIT IO_ERROR
  UNKNOWN_VERB FORBIDDEN INTERNAL`. `CONFLICT` reasons: `stale_rev overlap
  history_trimmed undo_conflict dirty disk_modified path_open exists`. A
  `CONFLICT` always carries the buffer's current `rev`.
- **Refusal precedence.** When several checks would fail, the first in this
  order is reported: unknown verb → argument shape → unstamped mutation →
  mesh lock → buffer/epoch lookup → op_id duplicate (returns the original
  reply) → CAS → position resolution → limits → I/O.
- **Buffer ids** look like `b3_9f2c41a7`. The suffix is the daemon **epoch**,
  8 hex digits chosen fresh at each start. Every reply that names a buffer
  also carries `epoch`. After a restart, an old id gets `NOT_FOUND`
  `epoch_mismatch`.
- **Mesh-open.** Local and mesh callers reach every verb. With
  `COSMIX_MESH_OPEN=0` in the daemon's environment (read once at start),
  mutating verbs from a non-local broker are refused with `FORBIDDEN`
  `mesh_locked`. A mutating request that did not come through a broker
  (no `broker_origin`) is refused with `INVALID_ARGUMENT` `unstamped`.

## Wire forms

```text
BUFFER := "b<N>_<epoch>"
POS    := <byte offset> | {"line":L,"col":C?} | {"anchor":"name"} | "start" | "end"
RANGE  := [s, e] | {"start":POS,"end":POS} | {"lines":[a,b]} | {"anchor":"name"} | "all"
SEL    := RANGE | {"anchor":POS,"head":POS}                 (explicit direction)
POINT  := {"offset":N,"line":L,"col":C}                     (every position in a reply)
OP     := {"op":"insert","at":POS,"text":S}
        | {"op":"delete","range":RANGE}
        | {"op":"replace","range":RANGE,"text":S}
EDIT   := {"offset":N,"delete":N,"insert":S}                (application order, sequential coordinates)
```

- **Offsets** are UTF-8 byte offsets and must fall on a char boundary
  (otherwise `not_char_boundary`).
- **`line` and `col` start at 1.** `col` counts every Unicode scalar value from
  the start of the line, `\r` included. A line ends at `\n` alone, so every
  char boundary of a CRLF file is addressable. `{line}` alone means col 1.
- **`{"lines":[a,b]}`** runs from the start of line a to the start of line
  b+1, so it includes b's `\n`. When b is the last line it runs to the end of
  the buffer.
- `bytes` always means text bytes, with any BOM excluded.

## Origins, lanes and the kind rule

Every edit records an **origin** `kind:label`, where kind is `human`, `agent`
or `tool` and label matches `^[A-Za-z0-9._@/+-]{1,64}$`.

- A mutating verb takes an optional `origin` claim. For `edit.undo` and
  `edit.redo` the claim is `as`, because there `origin` names the lane to undo.
  Without a claim the origin is derived from the attested caller:
  `agent:<from>` for a registered local service,
  `agent:<service>@<peer>` for a mesh caller, and `agent:anon` for an
  anonymous one-shot.
- **The kind is attested; the label is free.** `human:` is honoured only for
  a *local registered* caller, which is what a frontend is. `tool:` is never
  honoured, since `tool:disk` (external reload) and `tool:editd` are the
  daemon's own. Any other claim keeps its label with kind `agent`, and the
  reply says `"origin_downgraded": true`. So an anonymous `origin="human:x"`
  edits as `agent:x`. A human frontend on another node edits as `agent:` until
  attested remote identities exist.
- **Lanes.** Each origin has its own undo and redo stacks. `edit.undo` undoes
  **your own lane** by default. `origin="agent:foo"` undoes that lane, and
  `origin="*"` undoes whichever lane holds the newest group. Mark's Ctrl+Z
  never eats an agent's edit, and an agent's undo never eats his.
- Undo is checked in full before anything changes. If another origin has
  since edited the same text, the undo is refused with `CONFLICT`
  `undo_conflict`, which names the intervening rev. In that case nothing
  changes: no rev, no event, and the stacks stay as they were.
- `coalesce: true` on a single-edit request merges it into your top undo
  group when it directly continues your previous edit. This is how keystrokes
  are grouped. Each keystroke still gets its own rev.
- **Anonymous callers share `agent:anon`**, including its lane and its op_id
  space. Pass `origin=agent:<name>` to get a lane of your own.

## Concurrency: CAS and `base_rev`

Every text mutation (`insert delete replace apply`) takes at most one of these.
Passing both is refused with `INVALID_ARGUMENT` `both_cas`:

- **Neither**: the request applies to the current text. The last writer wins,
  by the caller's choice.
- **`expect_rev: N`**: the request applies only if the buffer is at rev N.
  Otherwise it is refused with `CONFLICT` `stale_rev`, carrying the current
  `rev`. This is the agent idiom: `find` or `get`, then edit with the rev you
  read.
- **`base_rev: N`** (for optimistic frontends): the positions are **byte
  offsets in the text at rev N**. Line/col, anchor and line-range positions
  are refused with `base_rev_needs_offsets`. editd transforms the ranges
  through every edit since N and applies them. If an intervening edit
  overlapped them, the request is refused with `CONFLICT` `overlap`. If rev N
  has already been trimmed from the log, it is refused with
  `history_trimmed`. The reply says `"rebased": true` when a transform
  happened.
- **Tie priority is server order.** When two inserts land at the same point,
  the one the server applied first comes first.

**Transactions** (`edit.apply`). Every op refers to the same base text. Ranges
that overlap within one request are refused with `overlap_in_txn`, and
nothing applies. Two non-empty ranges with the same start always count as
overlapping. An insert at a range's start or end is allowed. Inserts at the
same offset read in request order. An insert at a range's start reads
**before** that range's replacement text.

**op_id retries.** A mutating request may carry
`op_id` (`^[A-Za-z0-9._:-]{1,64}$`). A retry with the same caller, origin,
verb and op_id returns the original reply marked `"duplicate": true`, and
applies nothing. Only successful replies are cached, 1,024 per buffer, so a
refused request can be retried with its op_id.

## Verbs

`read_only` in the manifest: `ping info list get find anchor.get history props.*`.
`HELP` lists the manifest.

| Verb | Args | Reply |
|---|---|---|
| `edit.ping` | — | `{pong, service:"edit", schema:"edit.v1", epoch}` |
| `edit.info` | — | name, schema, epoch, `props_level:"L2"`, build provenance, `buffers`, `dirty`, `volatile:true`, `mesh_open`, `event_seq`, `publisher_loss`, `limits` |
| `edit.list` | — | `{epoch, buffers:[{buffer, path, opened_as, name, language, rev, saved_rev, dirty, disk, lines, bytes, holders}]}` |
| `edit.open` | `path?` (absolute or `~/…`), `create?`, `language?`, `origin?` | `{buffer, epoch, path, opened_as, name, language, rev:0, lines, bytes, eol, bom, disk, reopened, created}` |
| `edit.close` | `buffer`, `force?` | `{buffer, closed, holders}` |
| `edit.save` | `buffer`, `path?` (save-as), `expect_rev?`, `force?` | `{buffer, epoch, path, rev, saved_rev, file_bytes, disk:"clean", durable, warning}` |
| `edit.reload` | `buffer`, `force?`, `expect_rev?` | a mutation reply, or `{buffer, rev, unchanged:true}` |
| `edit.get` | `buffer`, `range?`=`"all"`, `numbered?`, `expect_rev?`, `snapshot?` | `{buffer, epoch, rev, text \| lines, start, end, bytes_total, lines_total, truncated, next, snapshot}` |
| `edit.insert` | `buffer`, `at`, `text` + common | mutation reply |
| `edit.delete` | `buffer`, `range` + common | mutation reply |
| `edit.replace` | `buffer`, `range`, `text` + common | mutation reply |
| `edit.apply` | `buffer`, `ops:[OP…]` + common | mutation reply |
| `edit.find` | `buffer`, `pattern`, `regex?`, `case?`=true, `range?`, `groups?`, `limit?`=1000, `from?` | `{buffer, rev, matches:[{start, end, text, text_truncated, groups, groups_truncated}], truncated, next}` |
| `edit.select` | `buffer`, `ranges:[SEL…]` | `{buffer, rev, origin, origin_downgraded, selections:[{anchor, head}]}` |
| `edit.cursor` | `buffer`, `at` | same as `select` |
| `edit.anchor.set` | `buffer`, `name`, `at?` \| `range?`, `bias?` | `{buffer, rev, anchors:[…]}` |
| `edit.anchor.get` | `buffer`, `name?` | `{buffer, rev, anchors:[{name, start, end, bias, collapsed_rev}]}` |
| `edit.anchor.clear` | `buffer`, `name` | `{buffer, cleared}` |
| `edit.undo` / `edit.redo` | `buffer`, `origin?` (lane: own / `"*"` / `kind:label`), `as?`, `expect_rev?`, `op_id?` | mutation reply + `lane`, `undid` / `redid` = `[first_rev, last_rev]` |
| `edit.history` | `buffer`, `since_rev?`=0, `limit?`=100 | `{buffer, rev, oldest_rev, entries:[{rev, origin, lane, kind, of, op_id, time, via, edits, edits_elided, text_bytes}], truncated, next}` |
| `edit.props.get\|list\|describe\|watch` | SPEC-07 props | see [Props](#props) |

"Common" means `expect_rev | base_rev`, `coalesce`, `cursor` (post-edit
coordinates), `op_id`, `origin`. The mutating verbs are `open close save
reload insert delete replace apply select cursor anchor.set anchor.clear undo
redo`, and all of them take `origin` and `op_id`. `select`, `cursor` and
anchors do not bump the rev.

**Mutation reply.** The reply is compact and bounded, whatever the size of the
transaction. It never includes the edit list, which lives in the `edit` event
and in `edit.history`:

```json
{"buffer":"b3_9f2c41a7","epoch":"9f2c41a7","rev":43,"base_rev":42,"origin":"agent:ctl-90",
 "origin_downgraded":false,"op_id":"k-118","duplicate":false,"rebased":false,
 "edit_count":1,"inserted_bytes":5,"deleted_bytes":3,
 "changed_span":{"start":{"offset":120,"line":12,"col":1},"end":{"offset":125,"line":12,"col":6}},
 "changed":[{"start":{"offset":120,"line":12,"col":1},"end":{"offset":125,"line":12,"col":6}}],
 "changed_truncated":false,
 "cursor":{"offset":125,"line":12,"col":6},"dirty":true,"lines":204,"bytes":8123,
 "history_trimmed_to":null}
```

- `changed` lists at most 64 inserted spans, in the new text's coordinates.
  Past that, `changed_truncated` is `true`, and `changed_span` still gives the
  envelope of every change.
- When an edit trims the log, `history_trimmed_to` is set to the oldest rev
  still kept. Undo reach is the retained log: 100,000 entries or 32 MiB of
  edit text.

**Open, holders and close.** Opening a path that is already open returns the
same buffer (`reopened: true`) and adds your caller key to `holders`. A buffer
binds to the canonical path resolved at open, so a later retarget of a symlink
is not followed. `edit.open` without a path creates a scratch buffer.
`create: true` opens a missing file as an empty buffer with `disk:"none"`.
`close` removes your key. The buffer is freed when no holders remain and it is
clean. Closing the last holder of a dirty buffer is refused with `CONFLICT`
`dirty`. `force: true` frees the buffer **and discards unsaved text**. In E0,
holders are never reaped when an agent crashes.

**Paging.** `get`, `find` and `history` stop at a 4 MiB encoded reply and
return `truncated` plus `next`; resume from there. To read a consistent
multi-page copy, pass `snapshot: true` on the first `get`. editd freezes that
rev and returns `"snapshot":"s7"`, and later pages pass `snapshot:"s7"`. The
snapshot is released after the final page, on close, or by LRU eviction (at
most 2 per buffer). A released token gets `NOT_FOUND` `snapshot_expired`.
`numbered: true` returns `lines:[{line, text, cont}]`, where a line split
across pages continues with `cont: true`.

**Replace-all** is `find` plus one `apply` with `expect_rev`, a Mix idiom
rather than a verb.

## Anchors

A named anchor (`^[A-Za-z0-9._-]{1,64}$`, at most 1,024 per buffer, shared by
all origins) is a point or a range that follows the text as it changes. An
edit `(p, delete dd, insert ii)` maps a point `a` like this:

| Case | Result |
|---|---|
| pure insert at `p < a` | `a` moves right by `ii` |
| pure insert exactly at `a` | `bias:"before"` (default): stays · `bias:"after"`: moves right by `ii` |
| a delete starting at or after `a` | unchanged |
| a delete that contains `a` (`p < a < p+dd`) | `a` collapses to `p`, and `collapsed_rev` records when |
| a delete or replace wholly before `a` | `a` moves by `ii − dd` |

Range anchors do not expand: text typed exactly at either end stays outside
the range. Selections follow the same rules. The editing origin's carets use
`after` and every other origin's carets use `before`, so your caret moves
with your typing and not with someone else's.

## Props

The daemon has an SPEC-07 read surface, `edit.props.get|list|describe|watch`.
`props_level` is `L2`.

```
lifecycle.props_level  lifecycle.epoch  lifecycle.volatile
lifecycle.event_seq  lifecycle.publisher_loss                (transient)
buffer_count
buffers.<bid>.path | opened_as | name | language | eol | bom | dirty | saved_rev | disk | holders
buffers.<bid>.rev | lines | bytes | origin_last              (transient)
```

`disk` is one of `clean modified deleted none unwatched`. Cursors and anchors
are not props; use the verbs. `edit.props.watch` replies with the
`edit.props.changed` topic, the domain topic `edit.changed`, and the bootstrap
recipe below.

## The change feed

| Topic | Inner command | Body |
|---|---|---|
| `edit.props.changed` | `props.changed` | SPEC-07 `{path, old, new, ts, cause}` |
| `edit.changed` | `edit.changed` | one event, below |

```json
{"event":"edit","epoch":"9f2c41a7","buffer":"b3_9f2c41a7","rev":43,"base_rev":42,"origin":"agent:ctl-90","lane":"agent:ctl-90","kind":"edit","of":null,"op_id":"k-118","edits":[{"offset":120,"delete":3,"insert":"hello"}],"event_seq":1207}
{"event":"cursor","epoch":"…","buffer":"…","rev":43,"origin":"human:ced","selections":[{"anchor":130,"head":130}],"event_seq":1208}
{"event":"anchor","epoch":"…","buffer":"…","rev":43,"name":"fn_render","start":98,"end":240,"event_seq":1209}
{"event":"disk","epoch":"…","buffer":"…","rev":43,"disk":"modified","event_seq":1210}
{"event":"open","epoch":"…","buffer":"…","path":"/home/u/x.mix","rev":0,"event_seq":1211}
{"event":"close","epoch":"…","buffer":"…","event_seq":1212}
{"event":"resync","epoch":"…","buffers":["b3_9f2c41a7"],"reason":"oversized","rev":44,"event_seq":1213}
```

`kind` on an `edit` event is `edit`, `undo`, `redo` or `reload`. `event_seq`
increases by one per event for the life of the daemon. An anchor event with
`start: null` means the anchor was cleared.

**Mirror rule.**

1. Subscribe to `edit.changed`.
2. `edit.get snapshot:true` to fetch the text and its rev.
3. Apply the `edit` events whose `base_rev` equals your rev, applying each
   event's `edits` in list order.
4. Refetch and resume from the new rev if any of these happens:
   - an event's `base_rev` does not match your rev,
   - `event_seq` skips a number,
   - a `resync` event names your buffer (or `"all"`),
   - `epoch` changes.

Loss is always announced:

- An event larger than 256 KiB (for example a big paste, or undoing a big
  delete) is replaced by `resync {reason:"oversized"}`.
- An event dropped from the publish queue or lost to a send failure marks its
  buffer `resync_pending`. The resync is retried with backoff (250 ms,
  doubling, capped at 30 s) until delivered, and goes out ahead of later
  events.
- Every Bus reconnect publishes `resync {buffers:"all", reason:"reconnect"}`.

In Mix: `subscribe("edit.changed")` plus `on edit.changed … end`.
`$event.args` is the event.

## Files

- **Load.** Files are UTF-8. Anything else is refused with `INVALID_ARGUMENT`
  `not_utf8`. A BOM and the line-ending style are detected and preserved.
  The size is checked before the file is read. The language is detected from
  the name (`*.mix` → `mix`, `*.conf.mix` → `mix-data`, `scene.mix` → `scene`,
  `rs` → `rust`, and so on); `edit.open language=` overrides it.
- **Save** writes a temp file beside the target, fsyncs it, then **rechecks
  the destination immediately before replacing it**:
  - For a plain save, the file on disk must still be the one this buffer last
    loaded or saved (same dev/inode/size/mtime), or be absent if it was never
    saved. Anything else is refused with `CONFLICT` `disk_modified`, and
    `force: true` overwrites anyway.
  - For save-as (`path=`), a target that exists is refused with `CONFLICT`
    `exists` unless you pass `force`. A target open in another buffer is
    refused with `path_open`.

  The save then renames the temp file over the target and fsyncs the
  directory. If that directory fsync fails after the rename, the reply says
  `durable: false` and carries a `warning`.
- **The honest guarantee.** A save is an atomic replacement guarded by a
  precondition, not a filesystem compare-and-swap. A writer that lands between
  the final check and the rename is overwritten, which is the same window
  every mainstream editor has. Writing the canonical target keeps a symlink
  intact. Hard links to the old inode are detached.
- **External changes.** editd watches each open file's parent directory
  through inotify; it does not poll. When the file changes on disk:
  - A **clean** buffer reloads with a minimal edit (the common prefix and
    suffix are kept). The reload is logged as one `reload` entry in lane
    `tool:disk` and can be undone.
  - A **dirty** buffer becomes `disk:"modified"` and keeps your text. Use
    `edit.reload force=true` to take the disk version, or `edit.save
    force=true` to overwrite it.
  - A deleted file becomes `disk:"deleted"`.

  A replaced or removed parent directory is re-watched by path. If the watch
  fails (inotify limit, network filesystem), `disk` becomes `unwatched` and
  the save-time check still protects the file.

## Limits

| Limit | Value |
|---|---|
| Buffer text / lines | 64 MiB / 2,000,000 |
| Inserted text per request / ops per `apply` | 1 MiB / 10,000 |
| Undo log per buffer | 100,000 entries or 32 MiB of edit text |
| Open buffers | 256 |
| All buffers + logs + snapshots | 1 GiB |
| Reply / event (encoded) | 4 MiB / 256 KiB |
| `find` matches | 1,000 default, 10,000 max; 4 KiB text per match |
| Anchors / selection origins / selections per origin | 1,024 / 64 / 16 |
| Queued commands per buffer | 256, and a full queue is refused at once with `RESOURCE_LIMIT` `busy` |

Exceeding a limit is refused with `RESOURCE_LIMIT`, and the buffer is left
untouched. Each buffer runs in its own task, so a slow regex on one buffer
never delays another.

## A Mix session

```mix
send edit edit.open path="~/notes/todo.md"
$b = $result.buffer
send edit edit.find buffer=$b pattern="alpha"
$m = $result.matches[0]
send edit edit.replace buffer=$b range={start: $m.start.offset, end: $m.end.offset} text="beta" expect_rev=$result.rev
if $rc == 10 and $reply.error_code == "CONFLICT" then
  print("someone edited first; buffer is now at rev " .. to_string($reply.rev))
end
send edit edit.insert buffer=$b at={line: 2, col: 1} text="-- note\n" origin="agent:tidy" op_id="t1"
send edit edit.apply buffer=$b ops=[{op: "insert", at: "end", text: "tail\n"}, {op: "delete", range: {lines: [1, 1]}}]
send edit edit.undo buffer=$b origin="agent:tidy"          -- only tidy's edit is undone
send edit edit.anchor.set buffer=$b name="mark1" at={line: 3}
send edit edit.get buffer=$b numbered=true
send edit edit.save buffer=$b
```

The same session, with the change feed and an external write checked, is the
end-to-end test `src/crates/cosmix-editd/tests/edit-bus-test.mix`. It runs a
private broker, the real daemon and one-shot Mix clients:

```sh
COSMIX=$PWD mix src/crates/cosmix-editd/tests/edit-bus-test.mix --bin src/target/release/cosmix-editd
```

## Install

```sh
cp src/_etc/systemd-user/cosmix-editd.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now cosmix-editd     # returns once READY=1
mix -c 'send edit edit.info
print($result.version)'
```

## Not in E0

The following arrive in E1 or later, each with its first consumer:

- diagnostics
- syntax highlighting
- Unicode width and word navigation
- `edit.run`
- crash-recovery files
- reaping the holders of crashed agents
- lossy loading of non-UTF-8 files
- attested remote human frontends
- merging overlapping edits instead of refusing them
- per-buffer topics
