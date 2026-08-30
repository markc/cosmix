# Remote execution (SSH)

Mix runs commands on other machines through four builtins that wrap the system `ssh` binary: **`ssh_run`** (structured result, never raises), **`ssh_must`** (returns stdout or dies), **`ssh_mix`** (runs Mix *source* on a remote node over the ssh stdin channel — zero quoting pain), and **`ssh_exec`** (runs an argv list on a remote node with truthful argv semantics via the remote run_argv, v0.30.0). The first two are the remote-host analogue of [`run_rc`/`run`](system.md) — same result-as-data philosophy, same "non-zero exit is data, not an exception" stance — extended with ssh-specific fields and a built-in wall-clock timeout.

All four wrap the real `ssh` client via `std::process::Command`. There is **no** native SSH client inside Mix: host alias resolution, `~/.ssh/config`, identity files, ports, `ProxyJump`, agents, hardware keys, and certificates are all `ssh`'s job. Mix passes the host string straight through (after validating it — see §"Host validation") and lets `ssh` resolve it. By convention pass an ssh **alias** (a bare host name from `~/.ssh/config`) rather than a raw IP.

The far-end Mix process records its ordinary `stdin`, `c`, or `script` mode;
there is no special remote tag. Set `MIX_STATS=off` in the remote environment
when fleet execution must perform no collection or stats I/O. See [usage
statistics](stats.md).

> All examples here use placeholder hosts (`alpha`, `node1`, or `192.0.2.x`
> from RFC 5737). In real use, substitute an alias from your `~/.ssh/config`.

## Headline idiom: `ssh_mix` + heredoc

For multi-line remote Mix, pass a heredoc directly as the `source` argument to
`ssh_mix`. The bytes travel over ssh stdin into `/opt/cosmix/bin/mix -`; no
shell parses the source on the way.

```mix
$r = ssh_mix("alpha", <<EOF
$h = run_argv_must(["hostname"])
print("remote host: " .. trim($h))
EOF
)
print($r["stdout"])
```

Actual output:

```text
remote host: alpha
```

### Heredoc interpolation: bare is remote, braced is local

This is the safety rule that makes the form useful. A bare `$name` in a
heredoc passes through **literally**, so the remote Mix interpreter resolves
it. Only `${name}` interpolates locally before the source is sent.

```mix
$local = "LOCAL"
$r = ssh_mix("alpha", <<EOF
$who = "REMOTE"
print("bare=" .. $who)
print("braced=${local}")
EOF
)
print($r["stdout"])
```

Actual output:

```text
bare=REMOTE
braced=LOCAL
```

### Pass local values with `bindings`

Use the `bindings` option when local data belongs in the remote program.
`ssh_mix(host, source, {bindings: {path: $target}})` strict-data-encodes each
value and prepends a real `$name = value` assignment. The value remains data;
it cannot break out into shipped source.

```mix
$target = "/srv/app dir; still-data"
$r = ssh_mix("alpha", <<EOF
print("path=" .. $path)
EOF
, {bindings: {path: $target}})
print($r["stdout"])
```

Actual output:

```text
path=/srv/app dir; still-data
```

The result map always has these nine keys:

```text
stdout  stderr  exit_code  ok  duration_ms  host
timed_out  interrupted  utf8_lossy
```

The exit-status key is **`exit_code`**, not `rc`. Reading `$r["rc"]` yields
`nil` silently, so branch on `$r["ok"]` or inspect `$r["exit_code"]`.

### Why not `ssh_run` for Mix source?

`ssh_run` sends a command string for the remote shell to parse. Nesting Mix in
that string creates escape ladders:

```mix
ssh_run("alpha", "print(run(\"/opt/cosmix/bin/mix -c 'print(which(\\\"sh\\\"))'\"))")
```

The heredoc form ships the intended program directly:

```mix
ssh_mix("alpha", <<EOF
print(which("sh"))
EOF
)
```

Use `ssh_run` for an actual remote-shell snippet; use `ssh_mix` + heredoc for
remote Mix source.

```mix
$r = ssh_run("node1", "uptime")
if $r.ok then
  print($r.stdout)
else
  print("failed (" .. ("" .. $r.exit_code) .. "): " .. $r.stderr)
end
```

---

## `ssh_run(host, command[, opts])` → map

Runs `command` on `host` and returns a result map. **Never raises** on a remote failure — branch on `.ok` (or `.exit_code`). The only errors it raises are *local argument* errors (bad host, bad opts), surfaced before any ssh is spawned.

### Signature

```mix
ssh_run(host, command)            -- command: a string (a remote-shell snippet)
ssh_run(host, commands)           -- commands: a list of strings, joined with " && "
ssh_run(host, command, opts)      -- opts: a map (see below)
```

| Arg | Type | Notes |
|---|---|---|
| `host` | string | An ssh alias (`node1`), `user@host`, or `user@host` with a `Port` from `~/.ssh/config`. Validated (§"Host validation"), then resolved by `ssh`, never parsed by Mix. |
| `command` / `commands` | string \| list of strings | Each element is a **remote-shell snippet** (it reaches the remote shell verbatim — §"Quoting"). A list is joined with literal ` && `; an empty list is an error. |
| `opts` | map | Optional. Unknown keys are rejected. See §"Options". |

### Host validation

The host string must be non-empty, contain no NUL bytes, and **must not begin with `-`**. A dash-leading host would let `ssh` parse it as an option — `-oProxyCommand=<cmd>` executes an arbitrary *local* command before any connection (a local RCE whenever the host value is config- or agent-derived). Defense in depth: the builtin rejects a leading-dash host up front, *and* the constructed argv always places a literal `--` (end of options) immediately before the host so ssh can never read it as an option. `ssh_must` and `ssh_mix` funnel through the same guards.

```mix
ssh_run("-oProxyCommand=touch /tmp/pwned", "echo hi")
-- Runtime error: ssh_run: host must not begin with '-' (got "-oProxyCommand=touch /tmp/pwned")
```

### Return map

```mix
{
  stdout:      "...",   -- trimmed (trailing newline stripped); never nil
  stderr:      "...",   -- trimmed; never nil
  exit_code:   0,       -- integer; 0 = success
  ok:          true,    -- convenience: exit_code == 0 (a real bool)
  duration_ms: 142,     -- wall-clock from spawn to exit
  host:        "node1", -- echoed back, handy for logging
  timed_out:   false,   -- true if killed by opts.timeout
  interrupted: false,   -- true if killed by Ctrl-C
  utf8_lossy:  false    -- true if a pipe held invalid UTF-8 (replaced with U+FFFD)
}
```

`exit_code` carries sentinels for local outcomes that aren't a real remote exit status:

| `exit_code` | Meaning |
|---|---|
| `0` | success (`ok == true`) |
| `255` | ssh-level failure (couldn't connect, auth failed, host-key refused) |
| `-1` | local **timeout** fired (`timed_out == true`) |
| `-2` | **interrupted** by Ctrl-C (`interrupted == true`) |
| `-3` | exited with no code and no signal (non-Unix fallback / unclassifiable edge) |
| `128 + signo` | remote command killed by a signal |
| other | the remote command's own exit status |

Note the field is named `exit_code`, **not** `rc` (which is what [`run_rc`](system.md) uses) — so the two result maps are deliberately not interchangeable, and a remote `exit_code` doesn't get confused with a local `rc` when both appear in the same script.

### Verified: failure is data, not an exception

A connection failure comes back as an ordinary map — the script keeps running.

```mix
$r = ssh_run("192.0.2.1", "echo hi", { connect_timeout: 2, timeout: 4 })
print("ok="        .. ("" .. $r.ok))
print("exit_code=" .. ("" .. $r.exit_code))
print("timed_out=" .. ("" .. $r.timed_out))
print("host="      .. $r.host)
print("stdout=["   .. $r.stdout .. "]")
print("stderr=["   .. $r.stderr .. "]")
```

```text
ok=false
exit_code=255
timed_out=false
host=192.0.2.1
stdout=[]
stderr=[ssh: connect to host 192.0.2.1 port 22: Connection timed out]
```

The full key set (every call returns all nine):

```text
stdout
stderr
exit_code
ok
duration_ms
host
timed_out
interrupted
utf8_lossy
```

### The `.ok` predicate — there is no `ssh_ok` / `ssh_try`

Mix ships exactly four SSH builtins: `ssh_run`, `ssh_must`, `ssh_mix`, and `ssh_exec`. Don't reach for `ssh_ok` or `ssh_try` — they don't exist. For a boolean test, use `ssh_run(...).ok`:

```mix
if ssh_run("node1", "systemctl is-active nginx").ok then
  print("nginx is up")
end
```

For fail-fast, use `ssh_must` (below).

---

## `ssh_must(host, command[, opts])` → stdout string (or dies)

Same arguments as `ssh_run`. On success (`ok == true`) it returns the command's **stdout string**. On *any* non-success — non-zero exit, timeout, interrupt, or signal-kill — it raises a catchable `RuntimeError` whose message names the host, the disposition, the `exit_code`, and the first 512 bytes of stderr.

```mix
$kernel = ssh_must("node1", "uname -r")
print("remote kernel: " .. $kernel)
```

Verified failure message (unreachable host):

```mix
$out = ssh_must("192.0.2.1", "echo hi", { connect_timeout: 2, timeout: 4 })
print($out)
```

```text
Runtime error at line 1: ssh_must: failed on 192.0.2.1 (exit_code=255): ssh: connect to host 192.0.2.1 port 22: Connection timed out
```

The disposition word is `failed`, `timed out`, or `interrupted` depending on `timed_out`/`interrupted`. Catch it like any other raising builtin:

```mix
try
  $v = ssh_must("node1", "test -f /etc/myapp.conf")
  print("config present")
catch $e
  print("config missing or host down: " .. ("" .. $e))
end
```

**Pick the right one:** `ssh_must` when failure is genuinely fatal and a `try`/`catch` would just be noise; `ssh_run` whenever you need finer control than "succeed or abort" — e.g. a `test -f` that returns 1 for "absent" is a perfectly valid answer, not an error.

---

## `ssh_mix(host, source[, opts])` → map

Runs Mix **source** on a remote node with zero quoting pain. It ships `source` over ssh's **stdin** byte-channel into `/opt/cosmix/bin/mix -`, so the source never becomes an argv word and bypasses *every* shell-quoting layer — the local shell, ssh, and the remote login-shell classifier. Arbitrary quotes, `$`, backslashes, and newlines survive intact. It is the discoverable, first-class form of the `ssh_run(host, "mix -", { stdin: $source })` idiom.

Start with the [headline `ssh_mix` + heredoc pattern](#headline-idiom-ssh_mix--heredoc)
above. This section supplies the remaining option and decoding contract.

```mix
$r = ssh_mix("node1",
             "print(data_encode({host: hostname(), kernel: trim(run(\"uname -r\"))}))",
             { decode: "data", timeout: 5 })
if $r.ok then print($r.value.kernel) end
```

For source stored in a file, pass caller values separately with `bindings`:

```mix
$job = {action: "inspect", paths: ["/srv/a", "/srv/b"]}
$source = read_file("remote-job.mix")
$r = ssh_mix("example.com", $source, {
  bindings: {job: $job, dry_run: true},
  decode: "data"
})
```

- **Returns the same map as `ssh_run`** (all nine keys). With `decode`, a tenth key `value` is added on success.
- **`decode: "data"` or `decode: "json"`** parses the trimmed stdout into `.value` (via `data_parse` / `json_parse`) — the common "get a structured value back from the remote" case. Pair it with [`data_encode`](data.md) on the remote side, as above. `decode: "json"` needs the `json` feature (the `mix` binary has it; a bare library build without it errors — use `decode: "data"`). On failure (`ok == false`) no `value` key is added — check `.ok` first.
- **`bindings` is a typed data channel:** it is a map from names to strict-data-encodable Mix values. Each key must match `[A-Za-z_][A-Za-z0-9_]*`; each value is strict-data-encoded locally and prepended to the shipped source as a `$name = value` assignment. Strings, numbers, bools, `nil`, lists, and maps retain their types, including nested values. Bytes and buffers have no strict-data representation and raise `OPTION_INVALID` locally; encode binary data explicitly (for example with base64) and decode it remotely.
- **No binding name is reserved.** Names such as `args` and `argv` are legal. Bindings run first, so the caller source may legally rebind any of them; this is convenience and injection safety, not isolation.
- **Order of the prepended prefix is `env` exports, then `bindings`, then your source.** A binding of the same name therefore shadows the exported **variable** but not the exported **environment**: with `{env: {FOO: "x"}, bindings: {FOO: "y"}}` the remote sees `$FOO == "y"` while `env("FOO") == "x"` and any child process still inherits `FOO=x`. Your source can rebind either. Give them distinct names unless you mean exactly that.
- **Accepts every `ssh_run` opt except two:** `stdin` (the source *is* the stdin — passing it is an error) and `env_transport` (not applicable — see below).
- **`env` works and is secret-safe:** the values are translated into hidden `export KEY = "value"` lines (strict-data-escaped) *prepended to the shipped source*, so they travel on stdin and never touch `ps` argv on either end.
- **The remote binary path is hardcoded** to `/opt/cosmix/bin/mix` — a full path, so it resolves inside the remote `/bin/sh` even on nodes where a bare `mix` is not on the non-interactive PATH. The remote must have mix installed there; `ssh_mix` is for cosmix-managed nodes, not arbitrary POSIX hosts.
- **Inherits every `ssh_run` host guard** (empty, NUL, leading-dash — verified: a `-oProxyCommand=` host is rejected through `ssh_mix` too, before any spawn).
- **`timeout` is in SECONDS**, like every ssh/run timeout opt (§"Options"). The Bus `send … timeout=2000` form is the only millisecond surface in Mix — `timeout: 5000` here is 83 minutes, not 5 seconds.

For source maintained as a separate program, use [`read_file`](io.md). For
inline multi-line work, prefer the headline heredoc form. Note the stdin
channel is *consumed by the program itself*: `ssh_mix` ships source fine but
cannot also feed stdin to a `run()` that the remote script calls (see §"stdin
caveat").

Verified argument errors (raised locally, before any ssh):

```mix
ssh_mix("node1")
-- Runtime error: ssh_mix: expected 2 or 3 args (host, source, [opts]), got 1

ssh_mix("node1", "print(1)", { stdin: "x" })
-- Runtime error: ssh_mix: `stdin` opt is not allowed — the source argument is the stdin

ssh_mix("node1", "print(1)", { env_transport: "mix" })
-- Runtime error: ssh_mix: `env_transport` is not applicable — ssh_mix always ships env
--   as hidden `export` lines inside the stdin source

ssh_mix("node1", "print(1)", { decode: "xml" })
-- Runtime error: ssh_mix: decode must be "data" or "json", got String("xml")

ssh_mix("example.com", "print(1)", { bindings: {"bad-name": 1} })
-- OPTION_INVALID: ssh_mix: invalid bindings key "bad-name"
--   (must match [A-Za-z_][A-Za-z0-9_]*)
```

---

## `ssh_exec(host, argv[, opts])` → map (v0.30.0)

The remote analogue of [`run_argv`](system.md): run an **argv list** on a remote
node with truthful, injection-inert argv semantics. OpenSSH only accepts a remote
command *string*, so a naive `ssh host <argv joined>` can't promise argv-inertness
across the wire. `ssh_exec` instead ships a strict-data Mix **driver** over the ssh
stdin channel that reconstructs the argv + options as data and invokes the remote
`run_argv` — no shell, no quoting, on either end.

```mix
$r = ssh_exec("pve3", ["pct", "start", "" .. $vmid], {timeout: 60})
if not $r.ok then
  eprint("start failed on " .. $r.host .. ": " .. $r.stderr)
end
```

The result is the full [`run_argv` process_result](system.md) map (`ok`,
`exit_code`, `stdout`, `stderr`, `timed_out`, `signal`, `duration_ms`,
truncation/lossy flags, …) plus **`host`**. Options split into two groups:

- **Remote `run_argv` options** — `timeout` (the remote command deadline,
  default 30 s, `0` = unbounded which also lifts the auto transport ceiling),
  `cwd`, `env`, `clear_env`, `max_output`, plus the protocol-safe stdio routes
  below — are encoded into the driver and applied to the remote `run_argv`
  call. `env`/`stdin` never appear in argv on either end.
- **SSH transport options** — `connect_timeout`, `transport_timeout` (the ssh wall-clock deadline, default = remote `timeout` + `connect_timeout` + 5 s), `batch`, `strict_host_key`, `multiplex`, `extra_ssh_args`, and `remote_mix` (the remote binary path, default `/opt/cosmix/bin/mix`).

The remote stdio allowlist is deliberately narrower than local `run_argv`:

- `stdout`: `"capture"`, `"null"`, or `{file: path, append?, mode?}`;
- `stderr`: `"capture"`, `"null"`, `"stdout"`, or the same file map;
- `stdin`: `nil`, a string, `{file: path}`, or `{null: true}` — exactly the local
  grammar minus binary. Bytes/buffer cannot cross the strict-data driver and
  raise `OPTION_INVALID`.

`stdout: "inherit"`, `stderr: "inherit"`, and `stream: true` raise
`OPTION_INVALID` **locally, before ssh is opened**. Inherited remote stdout would
interleave child bytes with ssh_exec's one strict-data result envelope; inherited
remote stderr lands in the outer ssh stderr that the success decoder discards;
and live teeing has the same envelope problem.

There is **no** `stdin: "inherit"` route to reject, here or locally: a stdin
string is always data, including the seven bytes `inherit`, and `nil` always
means closed stdin. The ssh stdin channel carries the driver, and the driver
carries whatever stdin you asked for as data — so the same value means the same
thing on both sides of the hop. A path inside any `{file: ...}` route is
resolved and opened on the **remote host**, not the machine calling `ssh_exec`.
The remote command deadline starts before that open. A FIFO input with no
writer reaches the deadline as remote `PROCESS_STDIO`; a FIFO output with no
reader fails immediately as remote `PROCESS_STDIO`. Neither case spawns the
requested command.

Two limits the allowlist cannot enforce for you:

- **A remote-version floor.** Routing options are forwarded to the remote
  `run_argv`; a remote old enough to have `run_argv` but not stdio routing
  rejects them inside the driver, which comes back as `SSH_PROTOCOL` with the
  remote's message. That is truthful but generic — deploy a current `mix` to a
  host before routing remote stdio on it.
- **An explicit path can still reach the envelope.** `{file: "/dev/stdout"}` or
  `{file: "/proc/self/fd/1"}` names the driver's own stdout, so the child's
  bytes are interleaved with the strict-data reply and the call comes back
  `SSH_PROTOCOL` instead of a result. Nothing *option-shaped* slips past the
  allowlist; a path that deliberately aliases the channel is your choice, and
  it fails loudly rather than silently.

Failure modes are encoded in the result, never raised (argv/option *validation*
does raise, locally, before any ssh):

- transport failure → `error_code` `SSH_TIMEOUT` / `SSH_INTERRUPTED` / `SSH_TRANSPORT`, `exit_code: nil`;
- an undecodable remote reply → `SSH_PROTOCOL`;
- a remote binary **older than 0.29** (no `run_argv`) → `ok:false`, `error_code: "SSH_REMOTE_UNSUPPORTED"`, and the command is **not run** — `ssh_exec` refuses to silently change injection/capture/timeout guarantees rather than fall back to a shell.

`ssh_exec` inherits every `ssh_run` host guard (empty / NUL / leading-dash
rejected). Use it for structured remote steps where you'd reach for `run_argv`
locally; `ssh_mix` remains the path for arbitrary multi-step remote Mix programs.

---

## Options map

Every key is optional; unknown keys raise an error. `ssh_mix` takes the same map minus `stdin`/`env_transport`, plus its own `decode` and `bindings`; see its section above for the typed binding contract.

| Key | Type | Default | Effect |
|---|---|---|---|
| `timeout` | int (**seconds**) | `30` | Hard wall-clock deadline. `0` disables it. On expiry: SIGKILL the process group, `timed_out = true`, `exit_code = -1`. |
| `connect_timeout` | int (seconds) | `10` | Passed as ssh's `-o ConnectTimeout=`. Bounds the connect phase only. |
| `multiplex` | bool | `false` | Connection reuse via `ControlMaster=auto` / `ControlPath=$HOME/.ssh/cm-%C` / `ControlPersist=60s`. See the caveat below. |
| `batch` | bool | `true` | `-o BatchMode=yes` — never prompt for a password/passphrase; fail instead. Keep it on for unattended scripts. |
| `strict_host_key` | string | `"accept-new"` | `-o StrictHostKeyChecking=`. One of `yes`, `no`, `accept-new`, `ask`. |
| `env` | map | `{}` | Remote env vars. Keys must match `[A-Za-z_][A-Za-z0-9_]*`; values may be string/number/bool (whole numbers inside i64 render as integers; since 0.59.0 a whole number at or above 2^63, or below −2^63, renders as its own shortest round-trip decimal instead of saturating to an i64 extreme, which is a different number; −2^63 itself is exactly representable and renders as the integer). **How the values travel is `env_transport`'s job** — by default they go over ssh stdin inside a driver, never in `ps` argv. See Â§"Remote env". |
| `env_transport` | string | `"mix"` | `"mix"` \| `"sh"` \| `"argv"` — the transport for `env` values. Inert when `env` is empty. See §"Remote env". |
| `cwd` | string | — | Prepends `cd '<cwd>' && ` to the remote command (quoted for you). |
| `stdin` | string | — | Fed to the remote command's stdin over the local ssh pipe. May contain NULs (binary payloads). Conflicts with `env` under the secure transports (§"Remote env"). |
| `extra_ssh_args` | list of strings | `[]` | Extra argv inserted before the host: `["-p", "2222"]`, `["-J", "jump"]`, `["-o", "IdentitiesOnly=yes"]`. Always followed by the `--` host guard. |

### Verified option errors (raised locally, before any ssh)

```mix
ssh_run("node1", "echo hi", { foo: 1 })
-- Runtime error: ssh_run: unknown opts key "foo" (allowed: timeout, connect_timeout,
--   multiplex, batch, strict_host_key, env, env_transport, cwd, stdin, extra_ssh_args)

ssh_run("node1", "echo hi", { timeout: "soon" })
-- Runtime error: ssh_run: timeout must be a non-negative integer

ssh_run("node1", "echo hi", { strict_host_key: "maybe" })
-- Runtime error: ssh_run: strict_host_key "maybe" not in ["yes", "no", "accept-new", "ask"]

ssh_run("node1", "echo hi", { env_transport: "ftp" })
-- Runtime error: ssh_run: env_transport "ftp" not in ["mix", "sh", "argv"]

ssh_run("node1", "echo hi", { env: { K: "v" }, stdin: "x" })
-- Runtime error: ssh_run: `env` and `stdin` conflict — the secure env driver owns stdin;
--   pass env_transport: "argv" to combine them (env values then appear in ps argv on both ends)

ssh_run("", "echo hi")
-- Runtime error: ssh_run: host must not be empty

ssh_run("node1", [])
-- Runtime error: ssh_run: commands list is empty

ssh_run("node1")
-- Runtime error: ssh_run: expected 2 or 3 args (host, command, [opts]), got 1
```

### The built-in timeout — don't wrap ssh in your own

`ssh_run` enforces a wall-clock deadline itself, so you never need `timeout 5 ssh ...`. On expiry it SIGKILLs the whole ssh process group (so descendant helpers can't keep the output pipes open and drag the wall clock toward the *remote* command's lifetime). Ctrl-C is gentler: SIGTERM to the group, up to 2 s of cooperative grace, then SIGKILL — reported as `interrupted = true`, `exit_code = -2`.

**Every ssh/run `timeout` opt is in SECONDS** (so are `run`/`run_rc`/`http_*` — see [system.md](system.md)). The Bus `send … timeout=2000` form is the only millisecond surface in Mix.

```mix
$r = ssh_run("192.0.2.2", "sleep 30", { connect_timeout: 30, timeout: 1 })
print("ok="          .. ("" .. $r.ok))
print("exit_code="   .. ("" .. $r.exit_code))
print("timed_out="   .. ("" .. $r.timed_out))
print("under 3s? "   .. ("" .. ($r.duration_ms < 3000)))
```

```text
ok=false
exit_code=-1
timed_out=true
under 3s? true
```

> **`multiplex: true` weakens cancellation, not the local wall-clock bound.** A persisted mux master from a previous call lives in a different process group, so Mix cannot kill it or prove that the remote command stopped. Mix still returns at the local deadline: after a short drain window it abandons capture descriptors retained by the master and reports the timeout. The payoff is speed, at the cost of possible remote work continuing after the result. The `multiplex: false` default passes `ControlPath=none`, so the fresh ssh client and its helpers normally die with the process group.

---

## Remote env without the ps leak (`env` + `env_transport`)

Naively, remote env means prefixing `export K='v'; ` onto the command string — which makes the value one argv element of the local `ssh` *and* of the remote shell, so a secret is visible in `ps` output **on both ends**. That is now the opt-in legacy path, not the default. `env_transport` selects how `env` values travel (it is inert when `env` is empty):

| Transport | Mechanism | Visible in `ps`? | For |
|---|---|---|---|
| `"mix"` (default) | A generated Mix driver ships over ssh **stdin** into `/opt/cosmix/bin/mix -`: strict-data-escaped `export` lines, then `exit(run_stream(["sh", "-c", <command>]))`. | **No** — local `ps` shows only `ssh … /opt/cosmix/bin/mix -`; remote `ps` shows `sh -c <command>` (the command, never the env). | Every cosmix-managed node, regardless of login shell. Requires `/opt/cosmix/bin/mix` on the remote. |
| `"sh"` | A POSIX driver (`export K='v'` lines + the command) ships over stdin into `sh -s`. | **No** — the script travels on stdin. | Arbitrary **non-cosmix** POSIX hosts. **Broken on mix-login-shell nodes** — the remote `sh -s` misroutes through the Mix classifier. |
| `"argv"` | The legacy `export K='v'; ` command-string prefix. | **Yes, on both ends.** | Compatibility only. Never for secrets. |

```mix
ssh_run("node1", "deploy.sh", { env: { API_TOKEN: $secret } })
-- the token travels inside the stdin driver; `ps` on either end never shows it
```

Three consequences to know:

1. **The secure transports own stdin.** `env` + `stdin` together conflict loudly (verified error above) unless you explicitly pass `env_transport: "argv"`, which restores the combinable-but-visible legacy shape.
2. **With `env` set under a secure transport, the command runs under POSIX `sh`, not the remote login shell.** On a mix-login-shell node, a no-env snippet is evaluated by the remote *Mix* classifier; the same snippet with `env` set runs via `sh -c`. Write POSIX shell in the snippet when you pass `env`.
3. **`ssh_mix` handles env its own way** — always as hidden `export KEY = "value"` lines prepended to the shipped source (stdin, never argv), which is why it rejects `env_transport` as not applicable.

The legacy `"argv"` shape, for reference (values `shell_quote`-d, but still argv-visible):

```mix
ssh_run("node1", "make build", {
  cwd: "/srv/project with spaces",
  env: { CI: true, BUILD_TAG: "v1.2.3" },
  env_transport: "argv"
})
-- remote command becomes:
--   export CI='true'; export BUILD_TAG='v1.2.3'; cd '/srv/project with spaces' && make build
```

(`export` rather than the `K=v cmd` one-shot syntax, so every command in a ` && `-joined chain sees the variable, not just the first.)

---

## Quoting — the footgun this design removes

Each `command` element reaches the **remote shell verbatim** — Mix does **not** wrap it in quotes. That's deliberate: you write normal shell, including pipes, redirects, and globs, and they run remotely.

```mix
ssh_run("node1", "grep -c error /var/log/app.log | tr -d '\\n'")
```

The danger is *interpolated data*. If you splice an untrusted value straight into the snippet, you've built a remote-shell-injection hole. Use [`shell_quote`](builtins.md) on every interpolated value:

```mix
$name = "weird; rm -rf /"            -- hostile input
$cmd  = "ls -la " .. shell_quote($name)
$r    = ssh_run("node1", $cmd)
-- the snippet becomes:  ls -la 'weird; rm -rf /'   → one literal argument, no injection
```

When you pass a **list**, the elements are joined with ` && ` *unquoted* — same rule, you `shell_quote` interpolated pieces inside each element:

```mix
ssh_run("node1", [
  "mkdir -p " .. shell_quote($dir),
  "cd " .. shell_quote($dir),
  "tar xzf -"
], { stdin: $archive_bytes })
```

The `cwd` option *is* quoted for you (via `shell_quote` internally), and `env` values never need caller quoting on any transport — they're the safe path for a remote working dir or environment without hand-rolling shell:

```mix
ssh_run("node1", "make build", { cwd: "/srv/project with spaces" })
-- remote command becomes:  cd '/srv/project with spaces' && make build
```

For threading a *Mix value* (a map/list, not a shell string) to a remote Mix interpreter safely, see [`data_encode`](data.md) — it produces inert Mix source the remote lexer rebuilds, never shell commands. And for shipping whole Mix *programs*, `ssh_mix` sidesteps quoting entirely (the source travels as stdin bytes, not argv).

---

## When the remote login shell is Mix

If a host runs `/opt/cosmix/bin/mix` as the login shell, then the snippet you send is evaluated by **Mix**, not by a POSIX shell. Two distinct modes:

- **Send Mix source for Mix behaviour.** The remote evaluates it as a Mix program.

```mix
ssh_run("node1", "print(run(\"hostname\"))")          -- runs Mix on the far side
```

- **Send a bare command for native dispatch.** Mix's shell-first classifier dispatches it like a normal command.

```mix
ssh_run("node1", "hostname")                            -- dispatches natively
ssh_run("node1", "mix status")                          -- bare `mix` self-resolves remotely
```

Things to remember on a Mix-shell host:

1. **For anything with quoting, prefer `ssh_mix`.** A one-liner as above is fine inline; the moment the payload has nested quotes, `$`, or newlines, ship it as source over stdin instead of fighting three quoting layers.
2. **No aliases on the non-interactive path.** `~/.mixrc` (aliases, PATH tweaks, prompt) loads only for *interactive* logins. `ssh_run` is non-interactive, so the remote sees no `~/.mixrc` — call binaries by full path (`/opt/cosmix/bin/...`), don't rely on a remote alias.
3. **Watch your local quoting.** The snippet is a Mix *string* on your side first. Prefer `'single quotes'` for the snippet so a `${...}` or `..` inside it isn't eaten by *local* interpolation, then escape only the inner double quotes you need.
4. **POSIX subshell grouping `( … )` doesn't survive the remote classifier** — a remote `(cmd; cmd) | sort` hits the same shell-dispatch limits as a local one (see [shell-mode.md](shell-mode.md)). Wrap it: `ssh_run("node1", "run_rc(\"(cmd; cmd) | sort\")")`.

See [the Bus page](bus.md) for the usually-better alternative on a meshed host: a static `.bus` address (`send noded.node1.bus noded.info`) routes to the remote broker directly — no ssh, no shell at all.

---

## Transferring files: scp via internal-sftp

`ssh_run` runs commands; it does not move files. For that, shell out to `scp`/`sftp` (the genuinely-right external tool — these aren't primitives that belong in Mix):

```mix
$rc = run_rc("scp /local/path/build.tar.gz node1:/srv/app/")
if $rc.rc != 0 then
  print("upload failed: " .. $rc.stderr)
end
```

> On a host whose login shell is Mix, plain scp/sftp needs the sshd config to enable the SFTP subsystem explicitly (`Subsystem sftp internal-sftp`) — otherwise the transfer has no server to talk to even though `ssh_run` works fine. That's a server-side setup detail, not a Mix one.

### stdin caveat

Stdin you write into a Mix-shell host's session does **not** reach a child spawned by the remote's `run`/`run_rc`. So for "push these bytes into a remote process", prefer one of:

- the **`stdin` option** on `ssh_run` (feeds the remote command's own stdin over the ssh pipe), or
- having the remote **pull** the data, or
- **scp** the file then act on it.

`ssh_mix` is the other side of this coin: there, stdin becomes the remote mix **program** itself — it ships *source* intact, but that same channel can't also feed stdin to a `run()` the remote script calls.

```mix
-- stdin option: pipe a payload to a remote filter
$r = ssh_run("node1", "wc -l", { stdin: $multiline_text })
print("remote line count: " .. $r.stdout)
```

---

## Patterns

### Fan out across a host list, collect results

A multi-statement lambda must be **bound to a var** before it's passed to a HOF (it won't parse inline) — see [functions](functions.md). For a large sweep, add `multiplex: true` to reuse connections per host — accepting the softer timeout bound.

```mix
$hosts = ["node1", "node2", "node3"]
$check = function($h)
  $r = ssh_run($h, "systemctl is-active nginx", { timeout: 5 })
  return { host: $h, up: $r.ok, detail: $r.stdout }
end
$status = map($hosts, $check)
for each $s in $status
  print($s.host .. ": " .. (if $s.up then "up" else "DOWN (" .. $s.detail .. ")" end))
end
```

### Structured value back from a remote node

```mix
$r = ssh_mix("node1", "print(data_encode({disk: trim(run(\"df -h / | tail -1\"))}))",
             { decode: "data", timeout: 5 })
if $r.ok then print($r.value.disk) end
```

### Deploy step that must succeed

```mix
ssh_must("node1", "install -m 0755 /tmp/app /opt/app/bin/app")
ssh_must("node1", "systemctl restart app")
print("deployed")
-- either line raises (aborting the script) the moment the remote step fails
```

### Custom port / jump host without touching ~/.ssh/config

```mix
ssh_run("user@example.com", "uptime", { extra_ssh_args: ["-p", "2222"] })
ssh_run("backend1",         "uptime", { extra_ssh_args: ["-J", "bastion.example.com"] })
```

### Detect non-UTF-8 output

```mix
$r = ssh_run("node1", "cat /some/binary")
if $r.utf8_lossy then
  print("warning: remote output had invalid UTF-8 (replaced with U+FFFD)")
end
```

---

## Local vs remote — which builtin

| You want… | Use |
|---|---|
| run a command **locally**, branch on exit code | [`run_rc`](system.md) → `{rc, stdout, stderr, timed_out, interrupted}` |
| run **locally**, raise on failure | [`run`](system.md) → stdout string |
| run **locally** with live/inherited stdio (interactive, `ssh -t`) | [`run_stream`](system.md) → exit code |
| run on a **remote** host, branch on outcome | **`ssh_run`** → result map |
| run on a **remote** host, raise on failure | **`ssh_must`** → stdout string |
| run Mix **source** on a remote node, no quoting | **`ssh_mix`** → result map (+ `.value` with `decode:`) |
| message a **meshed** node's broker (no shell) | [`send` / `address`](bus.md) |

---

## See also

- [system.md](system.md) — local `run` / `run_rc` / `run_stream`, the structured-return model `ssh_run` extends, and their (seconds-based) timeouts
- [builtins.md](builtins.md) — full builtins index, including `shell_quote`, `data_encode`
- [strings.md](strings.md) — single- vs double-quote rules (matters for snippet quoting)
- [functions.md](functions.md) — lambdas + HOFs for fan-out across hosts
- [shell-mode.md](shell-mode.md) — the shell-dispatch classifier a mix-login-shell node applies to your snippet
- [bus.md](bus.md) — the no-ssh alternative on a meshed host: `send`/`emit`/`address`
- [data.md](data.md) — `data_encode`/`data_parse` for safely shipping Mix *values* to and from a remote interpreter
- [OpenSSH `ssh_config(5)`](https://man.openbsd.org/ssh_config) — host aliases, `ProxyJump`, identities that `ssh_run` inherits
- `mix help` · `mix what ssh_run` · `mix what ssh_must` · `mix what ssh_mix` — terminal quick reference
