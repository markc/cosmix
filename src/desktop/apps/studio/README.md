# CosMix Studio

CosMix Studio is the recording-studio/DAW north star for CosMix Desktop. It is
a native CTK/Bevy application with the `musicd` mixer host fused into the
process for a direct interaction path, while its Bus app port exposes semantic
control and drives the `musicd` domain.

Its stable component slug is `studio`; the binary is `cosmix-studio`, the
native app id is `dev.cosmix.studio`, and runtime state lives below
`cosmix/apps/studio`. See [the desktop registry](../../APPS.md).

Run it from the shared workspace:

```sh
cd ~/.cos/desktop
cargo run -p cosmix-studio -- --stems <stem-session.v1-manifest>
```

Useful launch options include `--autoplay`, `--multitone`, `--smoke-stream`,
`--smoke-write`, and `--view mixer|waves|roll`.

Keyboard shortcuts come from the packaged `cosmix-actions` Studio keymap. A
strict-data `keymap.conf.mix` in Studio's resolved `config/` directory can set
`chord_timeout_ms` and custom bindings. Studio reloads that overlay when its
window regains focus.
