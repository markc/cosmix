# Native off-site storage pilot

`cosmix-store` stores and restores selected regular files through the existing
webd Bus service. Files stay local while an immutable SHA-256 snapshot is stored
on a remote node. This is an opt-in pilot, not a mounted filesystem or a live
database backup system. Empty directories, permissions and extended attributes
are not archived in schema version 1.

## Server configuration

Provision a private daemon-owned directory, outside every public document root,
and an operator-owned JSON configuration. Set `COSMIX_STORE_CONFIG` to that
configuration's absolute path in the webd service environment. Restart webd to
apply changes. Without the variable, all `webd.store.*` calls are disabled.
An invalid configuration is logged and storage stays disabled; HTTP remains up.

```json
{
  "root": "/srv/cosmix-store",
  "max_bytes": 15032385536,
  "max_staging_bytes": 2147483648,
  "collections": {
    "coast": {
      "writers": ["node:alpha"],
      "readers": ["node:beta"]
    }
  }
}
```

`max_bytes` counts committed objects/manifests plus unfinished reservations,
globally across collections; `max_staging_bytes` additionally bounds unfinished
uploads. Actual disk free space must also be monitored. One filesystem writer
holds an exclusive lock for the store lifetime. The directory must not be
writable by untrusted local accounts or reconfigured as a public web root.

Access is deliberately **node-scoped**. Any registered client on an authorised
source node may use its granted collections. The service requires receiving-
broker-stamped `broker_origin=mesh` and `broker_peer`; it never trusts a node
name supplied in arguments or the `from` service name. This requires noded's
authenticated direct-mesh delivery support (0.15.0). Anonymous, local and
unproven reverse-bridge delivery fail closed. Admission and revocation remain
the broker's responsibility. Readers cannot write or abort uploads; writers
can also read. There is no public HTTP upload route.

## Client

Install the `cosmix-store` binary built by the `cosmix-webd` package alongside
the other Cosmix binaries. Run it through Mix using `run_argv` or the thin
`ctl/_bin/store.mix` wrapper. The default connection uses the existing native
node configuration; `--noded-url` overrides the local broker endpoint.
The pilot registers as `store-client`: run one CLI operation at a time on each
source node. A second concurrent invocation is rejected by the broker.

```mix
print(run_argv_must(["cosmix-store", "--service", "webd.archive.bus",
  "--collection", "coast", "push", "/home/user/media/coast"]))
```

Push outputs the committed snapshot `sha256`. Retain that trusted ID with the
local project metadata. Rerun push after a network interruption to resume:
completed objects are reused and incomplete objects continue at their stored
offset. If source content poisoned an unfinished upload, use `abort HASH` then
retry; abort never deletes a committed object or snapshot.

```mix
print(run_argv_must(["cosmix-store", "--service", "webd.archive.bus",
  "--collection", "coast", "restore", "<snapshot-sha256>",
  "/home/user/media/coast-restored"]))
```

Restore requires a new destination directory whose parent exists. It verifies
the snapshot and each file hash, then installs verified files without replacing
existing ones. A failed restore can leave verified partial files; use a new
destination for a retry. `list` returns the first bounded page of snapshots;
the Bus API also exposes a pagination cursor. Local deletion never deletes a
remote snapshot. Snapshot identity proves content against a trusted ID, not
against a malicious replacement ID obtained from the same server.

## Protocol and limits

Commands are `webd.store.object.{begin,chunk,commit,read,abort}` and
`webd.store.snapshot.{commit,get,list}`. Every call requires `collection`.
Object identity and snapshot-get identity use `sha256`; begin also requires
`size`; chunk uses `offset` and `data_base64`; read uses `offset` and `max`.
Snapshot commit takes `manifest` with `schema_version:1` and a `files` array
of `{path,sha256,size}`. Get returns `{sha256,manifest}`.

Binary data is encoded only per bounded 256-KiB chunk within ABP's current
text/JSON body. It is never buffered as a whole file. Snapshot manifests are
bounded at 128 KiB and 1,000 files. Paths must be safe relative file paths;
symlinks and special files are excluded. Snapshots are compact canonical JSON
with lexical object-key ordering; array order is significant. Upload commits
check length and SHA-256; snapshot commits require all objects to be durable.
Exact chunk retries and immutable commits are idempotent. Disk operations run
on a bounded blocking worker while unrelated Bus commands remain responsive.
Concurrent storage requests may return `store_busy`; retry once the active
request has completed. There is no unbounded pending transfer queue.

The pilot has no pruning, automatic scheduling, signatures, encryption-at-rest,
public publishing or desktop catalogue installer. Use it initially for approved
unrestricted media/project files. Preserve independent backups and prove a
restore before relying on the service. Confidential backup acceptance requires
a reviewed encryption and independent recovery-key design first.
