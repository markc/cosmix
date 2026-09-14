# Service verb introspection

Native Bus services can supply a `Vec<cosmix_bus::VerbDescriptor>` to
`NodedClient::with_verbs`. The shared client reader then answers exact uppercase
`HELP` with rc `0` and a JSON array as the response body:

```json
[{"name":"HELP","args":[],"description":"List all commands this service accepts","read_only":true},{"name":"dnsd.stats","args":[],"description":"Return DNS response counters by rcode","read_only":true}]
```

`name`, `args` and `description` match the SPEC 02 HELP shape used by Mix.
`read_only` is additive metadata and defaults to `false` when deserialising
older descriptors. It is not an authorisation grant. Argument entries name
JSON body fields; individual handlers still validate required fields, optional
fields, types and mutually exclusive choices.

The manifest is returned exactly as supplied, including an empty array. Include
a descriptor for `HELP` when listing every accepted verb. The reader consumes
HELP before delivery, so an application HELP handler cannot answer it twice.
Clients without a manifest retain their existing dispatch, including Mix's
reserved HELP handling. INFO and service registration schemas are unchanged.

```rust
use cosmix_bus::VerbDescriptor;

let client = client.with_verbs(vec![
    VerbDescriptor::new("HELP", &[], "List commands", true),
    VerbDescriptor::new("example.status", &[], "Read service status", true),
]);
```

`set_verbs` replaces a live client's manifest through a shared reference,
including `VerifiedConnection::client()`. Install it before publishing the
identity where possible. For supervised clients use
`SupervisedClient::connect_options(...).with_verbs(manifest).connect()`;
the manifest is installed before registration and retained across reconnects.

CTK's `register_app_verb_described(descriptor, handler)` attaches metadata to a
typed handler. The original `register_app_verb(command, handler)` remains a
compatibility shorthand with no arguments, a generic description and
`read_only: false`. The app control connection captures the registry at
startup, including plugin-provided lifecycle, widget and action verbs.
`app.describe` now returns descriptor objects in its `verbs` array, matching
HELP, instead of strings. Consumers reading that array should use each
entry's `name` field.

Term's CTK app port lists its lifecycle and action verbs. Its separate native
session identity lists the `term.*` control verbs, including pane, tab,
snapshot, execution and task operations. HELP exposes static metadata without
requiring a target; invoking those verbs still uses existing native-session
policy and target/generation checks. dnsd supplies its two read-only verbs.
Other Rust daemons have not yet opted in.
