# Native-session tests

`tests/support` embeds noded production modules with `#[path]` rather than a
mock command engine. Changes to those modules can break or change Term's
integration tests even when Term's own source is untouched. Gate both the
daemon and desktop workspaces after changing the session/Unix substrate or
the typed client API. Keep the embedding tests-only; it is not a supported
noded library API, and daemon test-only modules are not included.

The fixture's pause control stalls the actual broker runtime and UDS sockets.
It does not fabricate replies. Ordering probes at metadata removal and PTY
cleanup must stay at those boundaries so reversing revoke/cleanup order fails.
