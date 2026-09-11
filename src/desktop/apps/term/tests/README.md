# Production Term→Mix binding test (p0i-01)

Use a clean, committed checkout. First build the main workspace's current
Mix with `cargo build --release -p cosmix-mix` from `src`. Then run the desktop
test with `COSMIX_E2E_MIX_BIN` naming that checkout's absolute
`src/target/release/mix` path (or the actual release path if using
`CARGO_TARGET_DIR`):

```mix
print(run_argv_must(["env", "RUSTC_WRAPPER=", "cargo", "build", "--release", "-p", "cosmix-mix"], {cwd: env("COSMIX") .. "/src"}))
print(run_argv_must(["env", "RUSTC_WRAPPER=", "COSMIX_E2E_MIX_BIN=" .. env("COSMIX") .. "/src/target/release/mix", "cargo", "test", "--manifest-path", "desktop/Cargo.toml", "-p", "cosmix-term", "native_session::production_e2e::p0i_01_production_term_spawn_enrols_real_mix_and_exit_revokes", "--", "--exact", "--ignored", "--nocapture"], {cwd: env("COSMIX") .. "/src"}))
```

The test shells out to `git status --porcelain -uno` and compares live HEAD to the
binary's embedded full SHA, rejecting dirty or stale builds. There is no
installed-binary fallback. Ordinary desktop runs report this test **ignored**,
never passed. Explicit runs with `--ignored` fail with instructions when the
variable is absent. Untracked scratch files do not invalidate provenance;
tracked changes (including dependencies) do.

The real broker, NativeSession prepare, sealed LaunchFd, production Terminal
spawn implementation, patched teletypewriter FD mapping and rio Machine all
participate. The test requires attachment generation 1, submits `exit` to the
real Mix prompt and observes revocation while retaining Terminal itself.
This realises p0i-01's binding hop.

## Recipient enforcement (S4)

The `native_session::enforcement_tests` module uses the same clean-HEAD Mix
provenance requirement. Each fixture starts a real isolated broker and the
production NativeSession actor, TabSet and PTY implementation. Requests enter
the real verified service and property adapters. Child fixtures first require
the production Mix attachment, then deliberately resume that child's key on
a verified fixture connection. No caller-provided principal bypasses the broker.

After building current-HEAD Mix, the unprivileged acceptance inventory is:

```mix
print(run_argv_must(["env", "RUSTC_WRAPPER=", "COSMIX_E2E_MIX_BIN=" .. env("COSMIX") .. "/src/target/release/mix", "cargo", "test", "--manifest-path", "desktop/Cargo.toml", "-p", "cosmix-term", "native_session::enforcement_tests::", "--", "--ignored", "--nocapture", "--skip", "p0i_07_other_uid_both_policies"], {cwd: env("COSMIX") .. "/src"}))
```

| Fixture | Real enforcement exercised |
| --- | --- |
| p0i-07 both policies | Ambient owner, pane-bound child, sibling scope, TCP and body spoof denial; verb/property reads, input, selection and unsupported execute |
| p0i-07 owner mutations | Every layout/input mutation, identical retries, operation lookup and expired retention |
| p0i-07 capability separation | Real child granted only read_state in both policies; denied contents/input/layout/terminate/execute; full-grant child close |
| p0i-07 other UID | Verified socket from a different OS UID, both policies, real and nonexistent targets |
| p0i-08 queued input | Actual PTY write boundary blocked; one-writer exclusion, human revocation, two-second deadline during broker pause, local pane revocation |
| p0i-08 lifetime | Child exit, pane close, Term exit and broker restart during queued input; old incarnation cannot regain rights |
| p0i-08 stale cleanup | Authentic child resumption, stale connection cleanup and connection-specific private event delivery |
| p0i-09 payload | Actual protected input and contents, allowlisted observe metadata, legacy tap omission and captured application diagnostics |
| p0i-10 unbound pane | Live graphical fallback pane has no protected verb/property access |
| p0i-10 TCP fallback | Real diagnostic service permits discovery only |
| p0i-10 launch failures | Missing/cancelled handoff, locally expired handoff window, broken sealed FD, wrong grant scope and quota exhaustion; real fallback prompt and denied controls |
| p0i-10 parent outage | Failed native bootstrap leaves diagnostic discovery only and no native control registration |

The cross-UID test is separately ignored and **requires root plus an explicit
`COSMIX_SESSION_TEST_UID` naming a non-root local test account**, in addition to
`COSMIX_E2E_MIX_BIN`. Run its exact test name with `--ignored --exact` under
those conditions. Missing prerequisites fail loudly; ordinary ignored output
is not acceptance evidence. The application-log fixture excludes dependency
raw-wire tracing and asserts that protected payloads do not enter application
diagnostic output.

The queued-write and retention hooks only hold the real write boundary or
age retained records; policy, transport and broker stamps are not mocked.
Existing S0–S3 and legacy handler tests remain part of the regression inventory.
Run both workspace test/clippy gates separately; this inventory does not claim
they passed.

## Stage-D test hooks

Two environment variables exist ONLY to make P0-J stage-D interleavings
deterministic. Both are read **once** — at editor start and at first use
respectively — so nothing later in the process, including evaluated Mix, can
turn them on; both are absent and therefore zero in every ordinary run, costing
one comparison against a captured field.

| Variable | Read by | Widens |
|---|---|---|
| `MIX_ADMIT_DELAY_MS` | the editor thread, before an admission claims its owner token | the queued-envelope window, so a fixture can make the admission owner give up while the envelope is still queued |
| `MIX_RESERVE_HOLD_MS` | the admission owner, between reserve and commit | the reservation window, so a fixture can type into it |
| `MIX_CLAIM_DELAY_MS` | the editor thread, AFTER an admission claims its token | the committed window, the only way to reach the branch where the owner's abandon loses and the outcome is genuinely undetermined |

They are not `#[cfg(test)]` because the PTY fixtures drive the real release
binary, which is built without test cfg by construction — a hook compiled out of
that binary could not be reached by the tests that need it. Neither changes any
decision the shell makes: they only stretch an interval that is otherwise
sub-millisecond, and every assertion around them is about behaviour that holds
at any interval length.

`check-stage-d-gates.mix` sets neither; the fixtures that need them set them per
child.
