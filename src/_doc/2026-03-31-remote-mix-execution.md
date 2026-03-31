# Remote Mix Execution — Scripts as Messages

**Date:** 2026-03-31
**Status:** Design

## The Idea

Every cosmix node can receive a Mix script over AMP, execute it locally with access to that node's services, and return the result. The script travels to the data instead of the data traveling to the script.

This is what ARexx did on the Amiga in 1990 — any application could receive and execute scripts in its own context. Cosmix extends this from a single machine to a WireGuard mesh.

## Why This Matters

### Problem: chatty mesh orchestration

Without remote execution, a monitoring script on your workstation does this:

```mix
-- 5 round-trips per node × 4 nodes = 20 AMP messages
for each $node in ["cachyos", "mko", "gcwg", "mmc"]
    send "maild.cosmix.${node}.amp" email.query filter="unread"
    $unread = len($result.emails)
    send "mond.cosmix.${node}.amp" system.load
    $load = $result.load_1m
    send "mond.cosmix.${node}.amp" system.disk
    $disk = $result
    send "mond.cosmix.${node}.amp" system.memory
    $mem = $result
    -- ... process all this data locally
end
```

Each `send` is a WireGuard round-trip. Latency compounds. The workstation pulls raw data it only needs summaries of.

### Solution: send the script, get the answer

```mix
-- 1 round-trip per node × 4 nodes = 4 AMP messages
$report = <<MX
    send "maild" email.query filter="unread"
    $unread = len($result.emails)
    send "mond" system.load
    $load = $result.load_1m
    send "mond" system.disk
    $disk_max = 0
    for each $d in $result.disks
        if $d.percent > $disk_max then
            $disk_max = $d.percent
        end
    next
    return {unread: $unread, load: $load, disk_peak: $disk_max}
MX

for each $node in ["cachyos", "mko", "gcwg", "mmc"]
    send "mixd.cosmix.${node}.amp" mix.eval source=$report
    print "${node}: ${result.unread} unread, load ${result.load}, disk ${result.disk_peak}%"
end
```

The script runs *on the node*, talks to local services with zero network latency, does the aggregation *there*, and returns a small summary. The mesh carries 4 small messages instead of 20 large ones.

### The key principles

**Scripts run where the data is.** A query that touches 10,000 emails should run on the node that has the mailbox, not on a workstation pulling data over WireGuard.

**The script IS the API.** Node A doesn't need to know Node B's service inventory. It sends a script that probes, adapts, and returns what it finds. If a service doesn't exist on that node, the script handles it (`if port_exists("maild") then ...`).

**Composition without deployment.** Write a monitoring script on your workstation, test it locally, then send it to all nodes. No build step, no binary copy, no service restart. The same `.mx` file works locally and remotely.

**Live patching.** Fix a bug in an orchestration script, re-run it — every node gets the fixed version immediately because the script is the message.

## Architecture

### The mix.eval command

A single AMP command handler, either in a dedicated `cosmix-mixd` daemon or as a module in a consolidated node daemon:

```
---
command: mix.eval
to: mixd
from: shell.cosmix.cachyos.amp
id: abc123
---
{"source": "send \"mond\" system.load\nreturn $result", "timeout": 10}
```

Response:

```
---
command: mix.eval
type: response
rc: 0
id: abc123
---
{"load_1m": 0.42, "load_5m": 0.31, "load_15m": 0.28}
```

### Implementation

The handler is trivially small — it's a wrapper around the existing `execute_mix()` function from `cosmix-lib-script/mix_runtime.rs`:

```rust
"mix.eval" => {
    let source = cmd.args["source"].as_str().unwrap();
    let timeout = cmd.args.get("timeout")
        .and_then(|v| v.as_u64())
        .unwrap_or(30);

    let result = tokio::time::timeout(
        Duration::from_secs(timeout),
        execute_mix(source, hub.clone(), "mixd", &vars),
    ).await;

    match result {
        Ok(script_result) => /* return script_result as AMP response */,
        Err(_) => /* return rc: 10, error: "script timed out" */,
    }
}
```

The evaluator is already:
- Async (yields properly to tokio)
- AMP-wired (send/emit/port_exists work via HubAmpHandler)
- JSON-capable (results serialize back to AMP automatically)
- Sandboxable (the evaluator has no fs access unless builtins are registered)

### What the remote evaluator has access to

- **All local AMP services** — via the hub connection. `send "maild" ...`, `send "mond" ...`, etc.
- **Context variables** — `$NODE` (hostname), `$MESH` (mesh name), `$SERVICE` ("mixd")
- **The cosmix prelude** — `ensure_running()`, `wait_for_port()`, etc.
- **Mix builtins** — string functions, json_parse/encode, math, type coercion
- **NOT the filesystem** — unless explicitly enabled. Remote scripts should talk to services, not read arbitrary files. File access goes through `cosmix-files` via AMP if needed.

### Timeout and resource limits

Every `mix.eval` call has a timeout (default 30 seconds). The evaluator already runs in a tokio task, so timeout is a simple `tokio::time::timeout` wrapper. For resource limits:

- **No infinite loops** — the timeout catches these
- **No fork bombs** — Mix has no process spawning unless `sh` is enabled (disable `sh` for remote eval)
- **Memory** — Mix values are heap-allocated Rust objects. A malicious script could allocate large lists, but mesh membership implies trust, so this is not a priority concern.

### Security model

Same as all AMP: **WireGuard mesh membership = trust**.

- Inside the /24: any node can send `mix.eval` to any other node
- At the mesh bridge: `mix.eval` from foreign meshes is rejected by default
- Single operator, single mesh — no multi-tenant threat model
- `sh` command disabled for remote eval (optional, can be enabled per-node in config)
- The `from:` field on every AMP message identifies the sender — audit trail is automatic

This is the ARexx security model: if you can reach the port, you're trusted.

## Use Cases

### 1. Mesh-wide monitoring

```mix
-- health-check.mx — run from any node, queries all nodes
$nodes = ["cachyos", "mko", "gcwg", "mmc"]
$check = <<MX
    $report = {}
    if port_exists("mond") then
        send "mond" system.load
        $report.load = $result.load_1m
        send "mond" system.memory
        $report.mem_percent = $result.used_percent
        send "mond" system.disk
        $report.disk_max = 0
        for each $d in $result.disks
            if $d.percent > $report.disk_max then
                $report.disk_max = $d.percent
            end
        end
    end
    if port_exists("maild") then
        send "maild" email.stats
        $report.emails = $result.total
    end
    return $report
MX

for each $node in $nodes
    send "mixd.cosmix.${node}.amp" mix.eval source=$check
    if $rc == 0 then
        print "${node}: load=${result.load} mem=${result.mem_percent}% disk=${result.disk_max}%"
        if $result.disk_max > 80 then
            print "  WARNING: disk usage high on ${node}!"
        end
    else
        print "${node}: UNREACHABLE"
    end
end
```

### 2. Distributed search

```mix
-- search-all-nodes.mx — fan-out search across the mesh
$query = args(1) ?? die "usage: search-all-nodes.mx <query>"

$search_script = <<MX
    $results = []
    if port_exists("maild") then
        send "maild" email.query search="${query}" limit=10
        if $rc == 0 then
            $results = $result.emails
        end
    end
    return {node: env("HOSTNAME"), results: $results}
MX

-- Replace $query in the heredoc before sending
$search_script = replace($search_script, "${query}", $query)

$all_results = []
for each $node in ["mko", "gcwg", "mmc"]
    send "mixd.cosmix.${node}.amp" mix.eval source=$search_script
    if $rc == 0 then
        for each $email in $result.results
            push $all_results, {node: $result.node, subject: $email.subject}
        end
    end
end

print "Found " .. len($all_results) .. " results across mesh:"
for each $r in $all_results
    print "  [${r.node}] ${r.subject}"
end
```

### 3. Rolling deployment

```mix
-- deploy.mx — build locally, deploy to all nodes
sh "cd ~/.cosmix/src && cargo build -p cosmix-maild --release"

$deploy_script = <<MX
    -- On the remote node: stop the service, report ready for copy
    sh "systemctl --user stop cosmix-maild"
    return {status: "stopped", ready: true}
MX

for each $node in ["mko", "gcwg"]
    print "Deploying to ${node}..."
    send "mixd.cosmix.${node}.amp" mix.eval source=$deploy_script
    if $result.ready then
        -- scp the binary (this part is still shell)
        sh "scp ~/.cosmix/src/target/release/cosmix-maild ${node}:~/.local/bin/"
        -- Restart remotely
        $restart = 'sh "systemctl --user start cosmix-maild"'
        send "mixd.cosmix.${node}.amp" mix.eval source=$restart
        print "  ${node}: deployed and restarted"
    end
end
```

### 4. Interactive remote debugging

From the Mix REPL on your workstation:

```mix
-- Ad-hoc queries against a remote node
send "mixd.cosmix.mko.amp" mix.eval source='send "maild" account.list; return $result'
print $result

-- Check what services are running on gcwg
send "mixd.cosmix.gcwg.amp" mix.eval source='return hub_services()'
print $result
```

### 5. Self-healing automation

```mix
-- watchdog.mx — run on a schedule (cron or mix timer)
$check = <<MX
    $health = {ok: true, issues: []}
    if not port_exists("maild") then
        sh "systemctl --user start cosmix-maild"
        sleep 2
        if port_exists("maild") then
            push $health.issues, "maild was down, restarted"
        else
            $health.ok = false
            push $health.issues, "maild failed to restart"
        end
    end
    if not port_exists("hubd") then
        $health.ok = false
        push $health.issues, "hubd is down (cannot self-heal)"
    end
    return $health
MX

for each $node in ["mko", "gcwg", "mmc"]
    send "mixd.cosmix.${node}.amp" mix.eval source=$check timeout=15
    if $rc != 0 then
        print "ALERT: ${node} unreachable"
    else if not $result.ok then
        print "ALERT: ${node} has issues:"
        for each $issue in $result.issues
            print "  - ${issue}"
        end
    end
end
```

## Relationship to cosmix-claude

The `cosmix-claude` daemon is a Claude Code agent. Remote Mix execution is complementary:

- **Mix scripts** handle deterministic orchestration (monitoring, deployment, queries)
- **Claude agents** handle fuzzy tasks (code review, bug diagnosis, natural language queries)
- A Mix script could delegate to Claude: `send "claude" analyze source=$error_log`
- Claude could emit Mix scripts: generate a monitoring script based on a natural language request

## Future Extensions

### mix.eval with streaming

Instead of waiting for the entire script to complete, stream results back as they're produced:

```mix
send "mixd.cosmix.mko.amp" mix.stream source=$long_running_script
-- Receive partial results as the script runs
```

This would need AMP to support streaming responses (multiple response frames per request ID).

### Script caching

Frequently-sent scripts could be cached on the receiving node by content hash:

```
command: mix.eval
args: {"hash": "abc123", "source": "..."}
```

If the node already has the script cached, it skips parsing. The content hash doubles as a cache key and an integrity check.

### Signed scripts

For mesh bridges that accept foreign scripts (future multi-mesh peering):

```
command: mix.eval
args: {"source": "...", "signature": "...", "signer": "mc@cosmix.dev"}
```

Ed25519 signature over the source. The receiving node checks the signer against an allow-list. Not needed for single-operator meshes but essential for federation.

## What This Is NOT

- **Not a general-purpose RPC framework.** This is scripting — small, readable, human-authored orchestration logic. Not a transport for generated code or binary payloads.
- **Not a replacement for AMP commands.** Apps still expose command vocabularies. Mix scripts compose those commands. The two layers are complementary.
- **Not Kubernetes.** There's no declarative state reconciliation, no controller loops, no resource objects. It's imperative scripting with human-readable scripts. The operator is the control loop.
