# Terminal launch and completion behaviour

`mix --gui` stamps the invoking directory as `TERM_CWD` unless the caller
already supplied it. Term selects an existing, searchable directory, otherwise
HOME. The pinned PTY API accepts UTF-8 strings only; a non-UTF-8 `TERM_CWD`
also falls back to HOME. A spawn failure retries HOME once when it differs.
Term removes `TERM_CWD` from the child environment before executing Mix, so
a later `mix --gui` uses that shell's current directory.

`src/desktop/scripts/term-desktop.mix` supports `open [DIR]`, `focus [DIR]`
and `layout NAME [DIR]`. Reusing an instance cannot apply an explicit directory
through the diagnostic verbs; the script reports that limitation. Carrying cwd
in requests belongs to P3a identity. Failed mutations stop the script with an
error; a partially created layout is retained.

It **resolves** which frontend to drive rather than naming one (TODO-term D1,
2026-09-21): it probes `INFO` on `term`, then `bterm`, and builds every verb
from whichever answers — a frontend refuses a verb in the other's namespace,
so the prefix has to follow the name. After launching it re-probes, because
which frontend `mix --gui` found is not knowable until one answers; the
bounded 20 s wait then fails with `TERM_NOREG` naming both candidates. The
binary search itself stays in `mix --gui` (`COSMIX_TERM_BIN`, then `term`,
then `bterm`, at each tier) and is deliberately not duplicated in the script.

Completion notifications use separate tracked Bus tasks so a blocked sink write
does not hold up the verb loop. On shutdown, queued and in-flight notifications
share a two-second drain budget, followed by bounded client close. Delivery is
best effort. `TERM_NOTIFY=0` disables these notifications. Dedupe keys identify
individual panes and do not coalesce exits from different panes.

The File and Help menus show shortcuts in CTK's native right-aligned
accelerator column: New Tab (`Ctrl+Shift+T`), Close Tab (`Ctrl+Shift+W`),
Quit (`Ctrl+Shift+Q`) and About (`F1`). The `MenuKeymap` supplies display hints;
Term's keyboard handler and `MenuActivated` observer dispatch the actions.
