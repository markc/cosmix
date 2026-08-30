# CosMix Tray Daemon

`cosmix-trayd` is the headless session authority behind CosMix tray skins. It
owns application and service discovery, local-noded reachability checks, and
allowlisted application, systemd and journal actions. It publishes that state on
the session bus as `dev.cosmix.trayd`; clients use `GetSnapshot` to read one
revision-consistent view.

The SSH plane reads the filesystem-authoritative host fragments under
`~/.ssh/hosts/` and public keys under `~/.ssh/keys/`. It verifies that
`~/.ssh/config` includes `~/.ssh/hosts/*`, requires each host file's single
literal `Host` alias to match its filename, and reports malformed entries
without hiding the rest of the catalogue. Non-0600 modes are visible warnings
but do not disable otherwise valid hosts. File access is descriptor-pinned and
symlink-safe: every open resolves beneath `~/.ssh` with symlinks refused, so a
symlinked `~/.ssh` (or symlinked entries beneath it — common with
symlink-managed dotfiles) degrades visibly instead of being followed; the real
directory must be in place. Host and key directory changes are followed with
inotify; missing directories recover from watches on `~/.ssh`, while a missing
`~/.ssh` root can be re-kicked through `Refresh`. No catalogue timer or polling
loop is used. Key fingerprints — including a failed `ssh-keygen` reading — are
cached by the key file's identity and metadata, so a transient failure persists
until the key file's metadata changes or the daemon restarts.

`ConnectSshHost` opens an actionable alias in Konsole through a transient user
unit, using an absolute `ssh` path resolved by trayd from trusted system binary
directories. `ProbeSshHosts` queues up
to 256 identity-only requests across four workers and returns immediately.
Probes use the real OpenSSH configuration with forwarding, agent loading,
local-command, TTY and multiplexing side effects disabled. They are
connect-predictive rather than side-effect-free: `accept-new` deliberately
enrols a first-contact host key in `known_hosts`, matching an interactive
connection, while a changed key fails closed. Results are discarded when the
host file is deleted, trashed or edited before completion.

SSH mutations remain identity-only. `CreateSshHost` validates each field,
requires a catalogued key and the effective hosts Include, then exclusively
creates the five-line sshm fragment with mode 0600. `EditSshHost` uses the fixed
desktop opener as a same-user escape hatch. Trash and restore are atomic
`RENAME_NOREPLACE` moves between `<id>` and `.trashed-<id>`; collisions never
overwrite either side, trashed rows remain visible, and `PurgeSshHost` removes
only an authority-validated trashed regular file.

Service discovery queries both the local system and user systemd managers. The
manager is part of each service identity and is required for controls and logs,
so identically named system and user units remain distinct. Failure to query one
manager leaves units from the other manager available and reports the partial
error in the snapshot.

Refreshes are event-driven. One pass runs at a time; a request arriving during a
pass schedules one coalesced follow-up. There are no recurring timers or
sampling loops. If another request observes that the current pass has run for at
least 30 seconds, `RefreshError` records how long it has been running and clients
can show that warning.

A filesystem operation can still block the refresh worker indefinitely, for
example on a stalled FUSE or SSHFS home. The daemon deliberately does not move
discovery into a killable helper subprocess: that would add process lifecycle,
serialization and deployment complexity to mitigate a low-probability local
failure. The residual limitation is therefore observable but not recoverable
inside the running daemon; restart `cosmix-trayd` after fixing or unmounting the
stalled filesystem.

The stable component slug is `trayd`, and the package, binary, activation
metadata and systemd user unit derive the corresponding `cosmix-trayd` and
`dev.cosmix.trayd` identities. This crate has no renderer and does not depend on
CTK, Bevy or a direct Tokio runtime.
