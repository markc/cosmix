# Expression evaluation mode

Sometimes a host doesn't want to *run a Mix program* — it wants to
*evaluate one Mix expression*: a scene host resolving a binding like
`$model.a * 2`, a config layer computing a default, a template filling a
field from preset values. For that, `cosmix-lib-mix` exposes
`eval_expr_string` — a synchronous entry point that evaluates **exactly
one expression** under an optional capability policy and the standard
eval limits, rejecting everything that isn't a pure expression shape
**before execution**. It landed in lib-mix 0.89.0 together with the
`send`/`emit` capability-gate fix.

`expr_mode_check(source) -> MixResult<()>` (lib-mix 0.89.1) performs the same
single-expression, static deny and depth checks without evaluation. Hosts
can reject invalid bindings at compile time without constructing an evaluator
or running builtins. Runtime errors such as division by zero remain evaluation
errors; this check does not predict values or enforce a host capability policy.

```rust
use cosmix_lib_mix::{
    eval_expr_string, CategoryAllowList, EvalLimits, IndexMap, Value,
};

let mut model = IndexMap::new();
model.insert("a".to_string(), Value::Number(21.0));

let v = eval_expr_string(
    "$model.a * 2",
    &[("model", Value::map(model))],
    Some(Rc::new(CategoryAllowList::deny_all())),
    EvalLimits::default(),
)?;   // Value::Number(42.0)
```

The evaluator is built fresh per call: `policy` installs via
`set_capability_policy`, `limits` via `set_limits`, and each
`(name, value)` in `globals` via `set_global` — the same seam handler
dispatch uses for `$event`. Nothing about the caller's own evaluator (if
any) is touched.

## The single-expression rule

`source` must parse to **exactly one expression statement**. Anything
else is rejected with an error naming the offending construct:

```text
eval_expr_string: not an expression: assignment
eval_expr_string: not an expression: for loop
eval_expr_string: not an expression: emit statement
eval_expr_string: expected exactly one expression statement, found 2 statements
```

That rule alone rules out assignment, control flow, `print`, `send`,
`emit`, `sh`, `on`, `source`, and multiple statements. A `try`/`catch`
around the call is the caller's job — the errors are ordinary
`MixError`s.

## The static deny walk

Past the single-expression rule, a static walk over the expression tree
rejects the constructs that carry authority or first-class functions —
**before execution, untaken branches included** (deterministic compile
semantics: a denied construct inside a ternary's false arm still
rejects, because the walk never evaluates anything):

| Construct | Rejected as |
|---|---|
| `send …` expression / statement | `send expression` / `send statement` |
| `sh "…"` (expression or nested statement) | `sh expression` / `sh statement` |
| `$(…)` command substitution | `command substitution $()` |
| `function (…) …` lambda literal | `function literal` |
| `$f(x)` — call on a function-valued expression | `function-value call` |
| `$x.name(…)` where `name` is not a builtin | `method call` |
| `on` / `source` / `include` / `… | cmd` nested in an if-expression | named per construct |
| `for` / `for … in` / `while` / `loop` nested in an if-expression branch | `for loop` / `for-each loop` / `while loop` / `loop statement` |
| `select … end` / `address "…" … end` nested in an if-expression branch | `select statement` / `address block` |
| `export` (mutates the HOST process environment; its runtime gate is permissive with no policy installed) | `export statement` |
| `print` statement nested in an if-expression (writes to the evaluator's output sink — the host daemon's real stdout/stderr) | `print statement` |
| a coalesce default in an interpolation (`"${x ?? …}"` and heredoc bodies): the payload is parsed and executed as a full program AT RUNTIME, so the walk statically analyses it here — it must be a single expression and passes the same deny rules recursively (nested coalesces included) | `coalesce default must be a single expression…`, or the inner construct's name |
| `sleep`, `readline`, `read_stdin`, `read_stdin_bytes`, `printf`, `eprintf`, `write_stdout`, `write_stderr`, `print_raw`, `eprint_raw` — by NAME: each is Pure- or Env-classed so no installed policy stops the Pure ones, and a binding has no business blocking on time/input or writing to the host's output stream; the static deny makes those premises unconditional, whatever policy the host chose | `<name> builtin` |
| string interpolation beyond literals and Mix variables | `environment-variable interpolation in string`, `command substitution in string` |

What stays **allowed**: literals, variables, arithmetic and comparison,
`..` concat, ternary `?:`, `if … then … else … end` as an expression,
field access, indexing, list/map literals, and bareword builtin calls
(`upper(…)`, `length(…)`). Method syntax on a builtin (`$s.upper()`)
desugars to a bareword `FunctionCall` at parse time, so it is allowed —
in a **nested** position (`'X-' .. $s.upper()`). One parser quirk to
know: as the *whole* statement, `$s.upper()` is the variable-led
first-accessor form, which folds to a field access and parses the `(`
as a first-class call on the map member — a `ValueCall`, denied like
every first-class call.

Interpolation details: `"${NAME}"` is a Mix **variable** part (scope
first, process env fallback) and stays allowed; a **leading `~`** in a
double-quoted string (`"~/x"`) is an env-var part and is denied;
`$(…)` inside a *heredoc* body is a command-sub part and is denied.

The walk is depth-capped at `MAX_EXPR_DEPTH` (**256**): a deeper tree is
a clean error, never a stack overflow in the walk. (The parser's own
nesting cap bounds *parse recursion*; a left-associative operator chain
parses iteratively but still builds a deep tree, so the walk carries
its own cap.)

```text
eval_expr_string: expression nesting exceeds MAX_EXPR_DEPTH (256)
```

Ordinary builtins are **not** statically denied — they run, gated by
the installed `policy` at dispatch exactly as in a full program. With
`CategoryAllowList::deny_all()` (the documented name for
`CategoryAllowList::new(&[])` — Pure only, nothing else) every non-pure
builtin fails with `capability denied`, and — since the 0.89.0 gate —
so do the bare `send`/`emit` broker forms and all shell syntax, by
class.

## Limits: the fuel story

`EvalLimits` applies in full: `max_list_len` / `max_map_len` /
`max_string_len` are enforced as values are built (an over-cap `..`
concat is a clean `string length N exceeds limit M` error), and
`recursion_limit` bounds what recursion can even be expressed (none, in
one expression without lambdas). **Loops cannot be expressed at all**:
the loop statements are denied everywhere the walk reaches, including
inside if-expression branch bodies — the one place a `for`/`while` could
otherwise hide — and neither can the constructs that *wait*: `sleep`,
`readline` and the stdin readers are denied by name (sleep is `Pure`, so
no installed policy stops it; the readers are `Env`, stoppable by a
policy but not by `policy: None` — the static deny covers both),
`select`/`address`/`send`/`emit`/`sh`/`export` are denied by construct,
and a fresh expression evaluator has no Bus/Db/Jmap handler to pend on
(those seam builtins raise "not available"). Evaluation cost is bounded
by the size caps, never by iteration count or by *unbounded* waiting
(`flock`'s argument-bounded wait remains reachable only when the host
installs a permissive policy — its class is `FsWrite`, denied under
`deny_all()`).

The **time limit** is armed at this entry point itself — there is no
statement loop here to carry the evaluator's usual per-statement poll,
so the budget is enforced by wrapping the evaluation future: on expiry
the future is dropped at its next yield and the caller gets a clean
`time limit exceeded` error. A single non-yielding CPU-bound builtin can
still overshoot until its next yield point; with the blocking four
denied statically, that residual is a slow computation, not a hang. See
[capabilities & embedding](capabilities.md) for the same caveat in full
programs.

`eval_expr_string` is **synchronous**: it drives the evaluator's async
path on a fresh current-thread runtime inside the call. Calling it from
within an async execution context **panics** (tokio refuses to block a
runtime thread) — hop to a blocking task first. A fresh runtime per
call is fine at config/scene-load rates; a host re-evaluating bindings
per frame should measure it.

## The honest boundary

The same wording as the `CapabilityPolicy` trait, because it is the
same gate: an in-process gate is a **robustness** boundary for trusted
embedded expressions, **not a containment** boundary for untrusted
code — a compromise owns the address space the gate runs in. The
single-expression rule and the deny walk are deterministic *fuel and
footgun* bounds (no runaway construction, no blocking on time or input,
no output to the host's streams, no env or shell reach — the deny table
above is the whole story), not a sandbox. What remains within a
conforming expression is pure computation over its inputs and the size
caps. Untrusted or multi-tenant code runs out-of-process under OS
isolation; see [capabilities & embedding](capabilities.md) for the
trust-split model.

## See also

- [capabilities & embedding](capabilities.md) — the classes, `CategoryAllowList`,
  the limits knobs, the trusted/untrusted split
- [Bus messaging](bus.md) — `send`/`emit` semantics and the `$rc` bands
- [shell dispatch](shell-mode.md) — `sh`, `$(…)`, pipes (all `Process`-gated)
- Source of truth: `eval_expr_string` / `MAX_EXPR_DEPTH` in
  [`evaluator.rs`](https://github.com/markc/cosmix/blob/main/src/crates/cosmix-lib-mix/src/evaluator.rs),
  `CategoryAllowList::deny_all` in
  [`builtins.rs`](https://github.com/markc/cosmix/blob/main/src/crates/cosmix-lib-mix/src/builtins.rs)
