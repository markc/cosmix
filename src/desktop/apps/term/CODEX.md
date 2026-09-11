# Native-session tests

`tests/support` embeds noded production modules with `#[path]` rather than a
mock command engine. Changes to those modules can break or change Term's
integration tests even when Term's own source is untouched. Gate both the
daemon and desktop workspaces after changing the session/Unix substrate or
the typed client API. Keep the embedding tests-only; it is not a supported
noded library API, and daemon test-only modules are not included.

Mix's `tests/native_session_pty.rs` also depends on this support crate and
uses Term's production `src/session_fd.rs` through the support library, without
compiling its desktop-only teletypewriter tests. Mix uses libc `openpty`, as its
job-control fixtures do. Handoff or support changes therefore require
the Mix native-session tests as well as Term's gates; do not duplicate the
memfd writer or replace the real broker with simulated replies.

The desktop `mix_child_bootstrap_proves_end_to_end` test explicitly builds and
runs that main-workspace target. It uses `src/target/term-native-e2e` to avoid
the enclosing Cargo test's build lock. The fixture requires a clean committed
build with embedded SHA matching HEAD; no installed/stale executable fallback
is allowed. A desktop-only lifecycle pass is not Mix enrolment evidence.

The fixture's pause control stalls the actual broker runtime and UDS sockets.
It does not fabricate replies. Ordering probes at metadata removal and PTY
cleanup must stay at those boundaries so reversing revoke/cleanup order fails.
