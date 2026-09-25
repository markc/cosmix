# Mirror fixtures (ced E1 plan §3, Stage S)

Each `*.json` file is one scripted scenario. Stage S's runner
(`tests/fixture_server_side.rs`) plays every **server-side** step against the
fake editd (`src/fake.rs`, which wraps the real `cosmix_edit_core::Buffer`) and
checks every declared server expectation. That way a contradictory fixture
fails in Stage S, not in E1d. E1d's mirror test (`tests/mirror_convergence.rs`)
then drives a `Mirror` through the whole script and checks the client-side
expectations.

## Shape

```json
{
  "name": "…", "about": "…",
  "server_side": true,
  "fake": {"max_event_insert": 16, "history_elide_over": 8},
  "path": "/fixture/doc.txt",
  "initial": "hello world\n",
  "run_id": 1,
  "steps": [ {"<kind>": …}, … ],
  "expect": {"server_text": "…", "server_rev": 2, "converged": true, "conflicts": 0,
             "detached": false, "notices": [], "op_ids_once": true}
}
```

- **The buffer** is created by the fake as `b1_0000e1e1`, epoch `0000e1e1`,
  holding `initial`, clean at rev 0, bound to `path` when one is given. After
  an `epoch_change` it is `b2_<new epoch>`.
- **Op ids.** The mirror's `OpIdGen::new(run_id)` hands out one id per
  `local` / `server_op` step, in step order: `c00000001-000001`, `-000002`, ….
- **`server_side: false`** marks a mirror-only scenario. Its steps are
  shape-checked only.

## Steps

Each step is exactly one key.

| Step | Side | Meaning |
|---|---|---|
| `local` `{id, edit:{items:[[s,e,"text"],…], coalesce}, intent?}` | client | `Mirror::local_edit` (view coordinates, request order). `intent` defaults to `"ui"` (origin `human:ced`). |
| `server_op` `{id, op, intent?}` | client | `Mirror::server_op`. `op` is one of `{"undo":{"lane":"own"\|"*"\|"kind:label"}}`, `{"redo":…}`, `{"save":{}}`, `{"reload":{"force":bool}}`. |
| `send` `{id, verb, caller, args, lost?, resend?, truncate_reply?, server_args?, expect}` | both | The request the mirror emits for `id`. `args` is **exactly** its body, which E1d compares. The server receives it at this point in the script unless `lost` is set. `server_args` (a test of the desync path) makes the fake apply different args. `expect` gives `{rc, rev?, reason?, duplicate?, reply_truncated?}`. |
| `arrive` `{id}` | server | A `lost` send reaches the server now, late. |
| `agent` `{caller, verb, args, expect}` | server | Another client's request. |
| `evict_dedup` `{}` | server | The fake forgets cached replies, as if 1,024 other ops had evicted them. |
| `set_disk` `{text}` | server | What the next `edit.reload` reads. |
| `epoch_change` `{new_epoch, text?}` | both | The daemon restarts with recovery (`text` overrides the restored text). The client is told through its epoch detection. |
| `deliver_event` `{seq}` / `drop_event` `{seq}` | client | Deliver or lose the event with that `event_seq`. |
| `deliver_reply` `id` / `drop_reply` `id` | client | Deliver or lose the reply to `id`. |
| `deadline` `id` | client | The request `id` timed out with no reply. |
| `echo_timeout` `id` | client | The echo timer the mirror handed out for `id` (an `rc 0` reply whose effect has not arrived) fires. The harness never fires it unasked. |
| `action` `{keep_mine:{}}` / … | client | A controller-level action. |
| `deliver_all` `{}` | client | Deliver everything still pending, in server order. |

**Reads.** Read-only requests the mirror emits (`edit.get`, `edit.history`,
`edit.list`, `edit.open`) are answered by the harness immediately, from the
fake's state at that moment. They are never scripted.

**Frozen request bodies.** These are the ones `send` compares. Keys are
always present.

- insert: `{buffer, at, text, base_rev, coalesce, origin, op_id}`
- delete: `{buffer, range:[s,e], base_rev, coalesce, origin, op_id}`
- replace: `{buffer, range:[s,e], text, base_rev, coalesce, origin, op_id}`
- apply: `{buffer, ops:[{op,at|range,text?}…in request order], base_rev, coalesce:false, origin, op_id}`
- undo/redo: `{buffer, as, op_id}`, plus `origin` (the lane) when it is not own
- save: `{buffer, origin, op_id}`
- reload: `{buffer, force, origin, op_id}`
- keep-mine replace: `{buffer, range, text, expect_rev, origin, op_id}`

A one-item edit is an insert when its range is empty, a delete when its text
is empty, and a replace otherwise.
