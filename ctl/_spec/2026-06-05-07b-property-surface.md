---
title: Cosmix Self-Aware Layer — The Uniform Property Surface
chapter: 7b
version: 0.1.0
status: stable
date: 2026-06-05
substrate_layer: aware
amends: _spec/2026-04-27-07-self-aware.md (the property read model; was SPEC 07 §2)
companion: _spec/2026-05-11-12-property-substrate.md (mutation/audit/collections extend this read surface)
---

# Cosmix Self-Aware Layer — The Uniform Property Surface

> **Split out of SPEC 07 §2 (2026-06-05).** This is the property *read model* — what
> a property is, the path grammar, `props.get`, and `props.describe`. It is the
> foundational, code-backed read surface (`cosmix-lib-props-core`) that the SPEC 07
> conformance + event-emission contract builds on, and that SPEC 12 (mutation,
> audit, managed collections) extends. Section numbers are preserved as **§2.x** so
> existing "SPEC 07 §2.x" cross-references resolve here.

## 2. The Uniform Property Surface

Every cosmix daemon (any process registered as a broker service) MUST implement
the following commands at conformance level L1 or higher:

| Command | Args | Returns | Purpose |
|---------|------|---------|---------|
| `<svc>.props.get` | `{path?: string}` | property tree (JSON body) | Snapshot at the path (root if absent) |
| `<svc>.props.list` | none | `[string]` (JSON body) | All defined property paths |
| `<svc>.props.describe` | `{path: string}` | schema entry (JSON body) | Type, mutability, sensitivity, description |
| `<svc>.props.watch` | `{path?: string}` | `rc: 0`, then change-event stream | Subscribe to changes (L2+) |

### 2.1 What is a property?

A property is a named, schema-described slot in a daemon's observable state.
Properties cover:

- **Configuration** — values loaded from `~/.config/cosmix/<svc>.toml`,
  command-line flags, environment variables.
- **Lifecycle state** — uptime, start time, current operating mode, health
  classification.
- **Registered resources** — connections, accounts, mailboxes, panels, peers
  — anything the daemon owns and can enumerate.
- **Derived state** — counters, queue depths, cache hit rates, *only* when
  durable enough to be worth observing (see §7.1 cardinality).

A property is **not**:

- An ephemeral metric sampled per-request (use a metrics topic instead).
- A high-frequency render value (frame timing, cursor position, GPU
  framebuffer state).
- A secret in plaintext (use `sensitive: true` in `describe`; values are
  redacted by default — see §7.2).

### 2.2 Property paths

Property paths use dotted notation: `config.bind`, `lifecycle.uptime_s`,
`mailboxes.inbox.unread`. The syntax matches Mix `$var.field` access (Ch 04)
and ABP command naming (Ch 02 §1).

A path identifies either:

- A **leaf** — a single value (string, number, bool, list, object).
- A **subtree** — an interior node whose children are themselves paths.

`props.get` with no `path:` returns the whole tree. With a path, it returns
that subtree (or leaf). `props.list` returns all leaves; subtree paths are
implied by their constituent leaves. `props.describe` accepts both leaf and
subtree paths.

Path segments are case-sensitive, lowercase, alphanumeric + `_`. The
wildcard `*` is reserved for future use (e.g., subscribe to all children of
a subtree) and MUST NOT appear in concrete paths.

### 2.3 Example: `noded.props.get`

Request:

```
---
amp: 1
type: request
to: noded
command: noded.props.get
---
```

Response:

```
---
amp: 1
type: response
from: noded
command: noded.props.get
rc: 0
---
{
  "config": {
    "bind": "192.0.2.5:4200",
    "node_name": "alpha",
    "log_level": "info"
  },
  "lifecycle": {
    "started_at": "2026-04-25T08:14:22Z",
    "uptime_s": 38291,
    "health": "ok"
  },
  "services": {
    "registered": ["maild", "indexd", "display", "mix-shell"],
    "count": 4
  },
  "topics": {
    "active": 12,
    "snapshot_bytes": 81920
  }
}
```

### 2.4 `props.describe`

Response shape (JSON body):

```json
{
  "path": "config.bind",
  "type": "string",
  "format": "host:port",
  "mutable": false,
  "sensitive": false,
  "description": "WireGuard interface address and port the broker binds to."
}
```

Optional fields: `format`, `enum`, `min`, `max`, `default`, `since`,
`deprecated`, `transient`. Type values: `string`, `number`, `bool`, `list`,
`object`, `null`. Subtree descriptions return `type: "object"` and an
additional `children: [path]` array enumerating direct child paths.

The schema language is defined in §6.4. It is intentionally minimal — a
subset of fields agents need, not a full JSON-Schema port.

---

