# Native MCP acceptance

Version 0.7 extends the existing Rust server. MCP remains the agent interface; control stays on ABP through noded. The reusable pieces are internal Rust modules for results, native connections, semantic tools and owned Mix workers. Extracting a library or adding a second server would add lifecycle and compatibility costs without a second consumer.

## Tested native paths

The Rust harness in `src/crates/cosmix-mcp/tests/native_mcp.rs` starts a fresh noded and a fresh stdio MCP process. Every fixture receives explicit temporary node configuration, a separate socket, an ephemeral loopback port, isolated logs and a temporary home. Startup, requests and teardown have bounds. RAII owns children and tasks; failure output includes MCP logs and captured stderr.

`native_mcp_acceptance` checks the real handshake, tools/list, annotations, readable results and JSON Schema validation of successful structured results. Native protocol fixtures check malformed-input rejection before delivery, terminal incarnation refusal, application return codes, explicit semantic arguments, bounded waits, cancellation without mutation replay, lazy broker reconnection and actual connection liveness. Mix checks cover cwd isolation, inherited child stdout, output bounds, native Bus sends and cancellation of worker descendants. Observed `ERROR:` log lines remain successful data.

`owning_ctk_mcp_acceptance` starts the real CTK `mcp_control_probe` example with its Bus bridge and widget control systems. This path is:

```text
agent/client → MCP app_control_set → Bus client → isolated noded
             → CTK app.controls.set → ordinary final ControlChange
agent/client → MCP app_control_wait → CTK app.controls.get → structured observed value
```

The test sets a knob, observes its new domain value and independently queries the ordinary change-event count. Invalid types and unknown targets produce tool errors while leaving the value and count unchanged. The probe runs headless. It proves native application behaviour; it does not prove compositor rendering, input coordinates or pixels.

Run the shipped Mix driver from a checkout, under the site's normal memory/build guard:

```text
/opt/cosmix/bin/mix /path/to/build-guard.mix /opt/cosmix/bin/mix /path/to/cosmix/src/crates/cosmix-mcp/tests/native-acceptance.mix /path/to/cosmix
```

The driver builds noded, MCP and the CTK probe, runs the normal MCP unit tests and then explicitly runs both isolated acceptance tests. An optional second argument supplies an existing desktop Cargo target directory. It does not run the older ignored test that requires an operator's live broker. Individual tests accept `COSMIX_MCP_TEST_NODED` and `COSMIX_MCP_TEST_CTK_PROBE` paths to freshly built binaries. Use the pinned checkout compiler. No install or desktop deployment is required.

## Patterns retained and facilities deferred

The Python project supplied useful interface and lifecycle patterns rather than a native control backend. KWin discovery, AT-SPI, XKB/uinput workers and clipboard helper processes cannot be transplanted into the Cosmix stack.

| Pattern | Rust/native adaptation | Status / owner | Acceptance |
|---|---|---|---|
| Schemas plus readable text | Dedicated status, terminal-list, Mix and semantic-result types; compatibility envelopes for other tools | Implemented / MCP | Actual tools/list schema validates every successful result exercised by the harness. |
| Honest errors and conservative hints | `isError`, malformed JSON rejection, retained application rc, mutation hints | Implemented / MCP | Invalid requests never reach the native service; owner refusals stay readable tool errors. |
| Separate transport and automation lifecycle | Parent owns MCP; one Rust worker process per Mix call with lazy ABP connection and bounded capture | Implemented / MCP | Changing worker cwd leaves later calls unchanged; cancelled descendant cannot write its completion marker. |
| Reconnect without replay | Refresh a dead client only for a new request; show actual cached-client liveness | Implemented / MCP | Kill isolated noded, observe disconnected status, restart it, then complete a new observation. |
| Semantic targeting and waits | Exact service/control/action ids; compositor id/generation; owner admission and observed-state checks | Implemented adapter / CTK and compositor retain their existing contracts | Actual CTK change path passes; compositor wrapper translation uses a protocol fixture. |
| Screenshot image attachments | Native compositor capture result with correlation, bounded image payload and explicit coordinate metadata; MCP attaches that result | Deferred / compositor capture and Bus payload contract, then MCP | Nested compositor over isolated noded returns an identified PNG through ABP; MCP image dimensions and output/scale metadata agree with a known rendered probe. Capture failure, timeout and cleanup are observable. No file path chosen by an agent and no external screenshot command. |
| Complete accessibility observations | Export CTK/AccessKit semantics with parent ids, role/state, revision and explicit supported actions | Deferred / CTK | A real CTK probe exports hierarchy and state through ABP; stale revision and ambiguous action are refused without a change event. Do not claim control metadata is a complete tree. |
| Unicode text and clipboard ownership | Extend the owning compositor/CTK text-input and seat clipboard contracts where current semantic controls are insufficient | Deferred / compositor input and clipboard, CTK editable text | A nested session receives unsupported-XKB Unicode exactly once through the native text path. A clipboard write survives the request lifetime and loses ownership cleanly when replaced; no paste retry after an unknown outcome. |
| Restart-safe target identity and authorisation | Broker-attested principal/route policy and owner process incarnation fences | Existing limits / noded, mesh trust and owning service | A restarted owner rejects an old target even if local ids repeat; denied callers cannot observe or mutate protected controls. Named registration, generation and tool annotations must not be accepted as authority. |

## Further native stages

1. Keep the protocol and CTK gates above as the foundation. Add transactional/idempotency contracts in indexd before interrupting its multi-step pipelines or replaying feedback updates. Acceptance: cancel after commit but before reply and prove exactly one update.
2. Add an isolated nested-compositor fixture driven by Mix over ABP. Admission must require an explicit readiness observation and teardown must confirm every owned child exits. Acceptance: launch one CTK window, observe its compositor id/generation, wait for presentation, act semantically and verify the displayed value through the actual MCP interface.
3. Extend compositor capture with a request/result lifecycle before exposing MCP screenshots. Apply the image and coordinate acceptance above, including output replacement, capture cancellation and stale identity. Existing file capture completion logs are insufficient as an RPC result.
4. Add CTK accessibility/text/clipboard facilities in their owners, retaining semantic actions wherever possible. Apply the hierarchy, Unicode and clipboard tests above before adding agent tools.
5. Introduce stricter identity and authorisation at the native boundary, then pass those contracts through MCP. Existing compatibility lanes need explicit migration; a tool description cannot enforce access.

Do not copy a global Python-style automation engine, external input injection, a Python sidecar, SSH control or an extra broker. Do not turn an admission acknowledgement into proof of completion. Defer library extraction until there is a second Rust consumer and a stable contract.

## Upstream contracts

The implementation uses pinned `rmcp` 3.1.2 source and actual wire tests. `Result<T, String>` becomes a completed error tool result through `IntoCallToolResult`; `ErrorData` is the distinct protocol-error path. Client request options return a request handle whose response must be awaited. The SDK generates the progress token, so tests compare with that handle's token. See the [official Rust SDK](https://github.com/modelcontextprotocol/rust-sdk), [pinned request lifecycle source](https://docs.rs/rmcp/3.1.2/src/rmcp/service.rs.html), [MCP tool result contract](https://modelcontextprotocol.io/specification/2026-07-28/server/tools) and [cancellation contract](https://modelcontextprotocol.io/specification/2025-06-18/basic/utilities/cancellation).
