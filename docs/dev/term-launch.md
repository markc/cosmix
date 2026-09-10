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

Completion notifications use separate tracked Bus tasks so a blocked sink write
does not hold up the verb loop. On shutdown, queued and in-flight notifications
share a two-second drain budget, followed by bounded client close. Delivery is
best effort. `TERM_NOTIFY=0` disables these notifications. Dedupe keys identify
individual panes and do not coalesce exits from different panes.
