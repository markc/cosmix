# Term native control

The global TCP `term` diagnostic registration serves discovery (`INFO` and
`HELP`) and sends completion notifications. It refuses all protected reads,
mutations and property requests with `FORBIDDEN`, including when native-session
bootstrap fails. It is not an alternative control route.

Protected controls belong on the broker-allocated, verified Unix Term identity.
BROKER-023 defines their policy; a diagnostic service name is never proof of
authority.

## Recipient gate

The native identity actor receives protected requests on its own verified Unix
connection. It uses the broker-stamped principal, its own current Term record,
and the live pane model. It never takes identity or policy from request bodies,
service names, PIDs or ordinary TCP headers.

`COSMIX_TERM_POLICY=restricted` selects the restricted policy at allocation;
the default is `default-open`. An invalid value disables native bootstrap.
The allocated record's policy remains authoritative on resumption.

Under default-open, an independent verified same-UID owner connection has all
six owner capabilities without confirmation. A bound pane-shell connection has
only its recorded capabilities and pane scope, under either policy. Opening
an independent connection is ambient owner access under default-open, including
when the process happens to be a child. Restricted policy removes that ambient
access. Other UIDs, TCP callers, names-only impostors and undelegated principals
receive the same `FORBIDDEN` refusal without target-existence details.

Every request names a `target` object:

```json
{
  "target": {
    "instance_id": "00000000000000000000000000000001",
    "incarnation": "00000000000000000000000000000002",
    "pane_id": "1",
    "pane_generation": "1"
  }
}
```

IDs above are illustrative. Discover current records through the native session
API. Instance/incarnation use BUS-016 hex encodings; generations, pane IDs and
request IDs use decimal strings. A graphics-only pane has no protected control
authority: delivering a launch descriptor alone is insufficient. The actor must
first observe or reconcile a successful child attachment. There is no active-pane
fallback for mutations.

| Verb | Required capability | Additional fields / result |
| --- | --- | --- |
| `term.session`, `term.list`, `term.tabs`, `term.panes` | `read_state` | Scoped pane/tab metadata, binding diagnostic, input generation, retry epoch/high-water and limits |
| `term.snapshot` | `read_state` | Metadata only by default |
| `term.snapshot` with `contents:true` | `read_contents` | Snapshot text, bounded by the response limit |
| `term.type` | `input` | `text`, `foreground_generation`, mutation ID/epoch |
| `term.tab.new` | `manage_layout` | Create a tab; requires owner/owning Term authority |
| `term.pane.split` | `manage_layout` | `dir`: `h`, `horizontal`, `v` or `vertical`; requires owner/owning Term authority |
| `term.tab.select`, `term.pane.select` | `manage_layout` | Select the explicitly targeted pane and its tab |
| `term.tab.close`, `term.pane.close` | `terminate`, plus affected layout authority | Close the explicit target |
| `term.execute` | `execute` | Always `UNSUPPORTED`; no Stage-D execution API is enabled |
| `term.operation` | `read_state` | `operation_id`: retrieve the caller's retained operation outcome |

Layout requests supply `affected`, an array of additional explicit targets,
when source/destination tabs, sibling geometry or replacement focus affect more
than the primary target. The gate computes the affected set under the model
lock and checks every member. Bound children cannot create panes outside their
grant. Close requires termination authority for the panes being removed; layout
authority does not substitute for termination authority.

## Live properties

`term.props.get` supports `property:"state"` and `property:"contents"` with the
same explicit target and separate read capabilities. `term.props.set` supports
`property:"selected", value:true` and `property:"input", value:"..."`; selection
uses layout authority and input uses input authority. Mutating properties carry
the same generations, affected set, request ID and epoch as their corresponding
verbs. Input properties also need `foreground_generation`.

These are live pane properties, not a second persisted copy of terminal state.
The adapter constructs `PeerIdentity` only from verified context and resolves
the props-store `AuthPolicy`/capability set with the same Term-owned evaluator.
Fresh lease checks complete before synchronous resolution; commitment rechecks
scope and local lifetime under the model lock. Legacy property identities retain
an absent optional native context. Broad property-write strings cannot bypass
the pane gate. Private watches, audit subscriptions and retained screen replay
are not installed by this adapter; unsupported property operations are refused.

## Input and revocation

Each accepted input request claims a two-second, one-writer lease for the pane.
Another actor receives `BUSY` while that lease is live. Input requires the current
foreground generation; human keys, focus/layout changes and observed PTY foreground
process-group changes invalidate old generations. The first human key revokes
agent ownership before its own admission. It never waits for lease expiry.

The existing ASCII key encoder supports ordinary ASCII, newline/Enter, tab,
backspace and Ctrl+C/D. This is bounded keyboard input, not the future paste or
execution pump. Unsupported characters are rejected before queue admission.
The request envelope is limited to 8,192 bytes; responses to 256 KiB. Queue
admission is atomic and actual nonblocking PTY writes recheck the permit. Expired
or revoked, unwritten agent bytes are discarded without counting them as PTY
writes. Already written bytes cannot be recalled.

Term establishes conservative `Deadline`s with real `lease.check` calls outside
the model/property locks. Queued permits retain those deadlines and a read-only
connection-liveness probe. Lifecycle revocation/suspension, successor binding
generations, gaps, connection loss, pane close and child exit invalidate permits.
No missed notice can extend a deadline. The verified client inbox is bounded at
256 deliveries and retires its reader on local overflow, invalidating connection
liveness; broker-side lifecycle queue/gap behaviour is unchanged.

`term.input.revoked` is a private event carrying target, request ID and a
`partial_or_unknown` outcome with a delivered-byte lower bound. Its
`recipient_connection` routing constraint contains the original verified broker
epoch and connection ID. Noded accepts this direct event only from a currently
attached same-UID Term, checks the exact destination connection under its session
lock, and stamps its source. This also serves unnamed ambient owners without
inventing service names. Old names or successor attachments cannot collect an
old connection's event. Event queues are bounded and best-effort; retained
operation queries remain available within the retry window. A completed input
outcome means bytes reached the PTY, never that a shell command completed.

Protected requests, replies, refusal bodies, property contents and private input
events are omitted before tap enqueue. Observe receives only BROKER-024 metadata;
general application diagnostics do not log payloads. Unsupported VT-event logs
record only the event discriminant, not clipboard/title/text fields.

## Retries and errors

Mutations require a nonzero, monotonically increasing `request_id` and
`request_epoch` equal to the authenticated connection's ID, available from
`session.hello` or `term.session`. Preserve the exact epoch and JSON payload when
retrying. The epoch constrains the ID; it never supplies the authenticated actor.
`term.session` reports the current actor's request high-water mark.

Entries bind the authenticated actor, target incarnation/generation, verb and
payload hash. Repeated accepted requests return the same operation ID and
submission result; conflicting arguments produce `CONFLICT`. `term.operation`
can report a later PTY-write outcome. Successful close retries resolve before
live-pane lookup, so removal does not cause a duplicate action.

Retention is 15 minutes or the last 1,024 accepted mutations per actor, with an
instance cap of 4,096 retained entries and 256 actor histories. Actor high-water
marks remain until Term exits; exhausting the actor/entry cap returns
`RESOURCE_LIMIT`. Retired IDs, stale instance incarnations and old connection
epochs produce `UNKNOWN_OUTCOME`, never automatic re-execution. A bound actor
can recover retained results after authenticated resumption. There is no
exactly-once guarantee across crashes or a new authenticated actor.

Success uses ABP RC 0; typed application refusals use RC 10. The error vocabulary
includes `INVALID_ARGUMENT`, `NOT_FOUND`, `STALE_GENERATION`, `CONFLICT`, `BUSY`,
`FORBIDDEN`, `UNSUPPORTED`, `RESOURCE_LIMIT`, `DISCONNECTED`, `EXPIRED`, `CANCELLED`
and `UNKNOWN_OUTCOME`. Authority failures remain uniform `FORBIDDEN`; a stale
foreground generation is `STALE_GENERATION`. Transport failure and caller-side
timeout remain client transport outcomes, separate from an application reply.

The S4 fixture inventory and explicit gate prerequisites are in
`src/desktop/apps/term/tests/README.md`. Test/clippy acceptance is supplied by the
orchestrator; this page describes implementation, not a claim of passing gates.
