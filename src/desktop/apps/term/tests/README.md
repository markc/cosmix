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
This realises p0i-01's binding hop; protected mutation admission remains S4.
