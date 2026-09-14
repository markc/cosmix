# Daemon command discovery

Send `HELP` to a daemon's registered Bus service to list its accepted verbs,
argument names, short descriptions and `read_only` flags. The shared client
answers HELP from the daemon's verb manifest, with HELP itself listed first.
Argument names are discovery hints; structured requests still use the JSON
schema accepted by the corresponding verb. Existing authorisation applies.

Alongside dnsd, this surface is available on indexd, maild, filesd, inputd,
interactd, nspawnd and webd in the main Cargo workspace. Manifests are installed
on every connection, including reconnects. Filesd advertises corpus verbs in
corpus mode and `fs.*` verbs in filesystem mode. Nspawnd advertises executor
verbs in executor mode and `nspawnd.ct.*` plus props verbs in controller mode.

Indexd and filesd also list their accepted unqualified aliases. Broker control
frames such as `noded.admit.challenge`, which indexd ignores, are not request
verbs and are not advertised. Props reads and watches are read-only; props
set/delete and other mutations are marked writable. A read-only flag describes
the command's operation, not an authorisation grant or a promise that diagnostic
counters will remain unchanged.
