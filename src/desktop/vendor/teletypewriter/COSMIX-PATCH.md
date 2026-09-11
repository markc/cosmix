# Term launch FD hook

Source: raphamorim/rio, commit `932c1a7d9e07b4db5924f7a0dd689e823c3a1442`,
`teletypewriter/`, MIT (see LICENSE). Workspace-inherited dependency metadata
is made explicit; corcovado retains the same upstream revision.

The only runtime change adds `create_pty_with_spawn_fd`. The existing entry
point delegates with no mapping. An optional reserved source/target pair is
duplicated in the child after PTY setup, before exec. Both descriptors remain
CLOEXEC in the parent. Term owns and closes them after the spawn attempt.
