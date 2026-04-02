# Dioxus State Management Patterns Reference

Extracted from Dioxus 0.7.4 examples on 2026-04-02.

## 1. Global Signals (app-wide state)

For state that any component needs to read/write without prop drilling:

```rust
static COUNT: GlobalSignal<i32> = Signal::global(|| 0);
static DOUBLED: GlobalMemo<i32> = Memo::global(|| COUNT() * 2);

// Read from anywhere
rsx! { p { "Count: {COUNT}" } }

// Write from anywhere
*COUNT.write() += 1;

// When you need a mutable signal reference (e.g. for .set())
let mut sig = use_hook(|| COUNT.resolve());
sig.set(0);
```

Best for: app-wide settings, counters, feature flags.

Source: `examples/04-managing-state/global.rs`

## 2. Context API (hierarchical state)

For state scoped to a component subtree:

```rust
// Provider (root or any ancestor)
use_context_provider(|| Signal::new(Theme::Light));

// Consumer (any descendant)
fn use_theme() -> Signal<Theme> {
    try_use_context::<Signal<Theme>>()
        .expect("Theme context missing")
}

let mut theme = use_theme();
theme.set(Theme::Dark);
```

Best for: theme, user session, locale — things that scope to a UI subtree.

Source: `examples/04-managing-state/context_api.rs`

## 3. Local Signals (component-level state)

```rust
fn Counter() -> Element {
    let mut count = use_signal(|| 0);
    rsx! {
        button { onclick: move |_| count += 1, "Count: {count}" }
    }
}
```

Best for: UI state local to one component (toggle, input value, etc.).

## 4. Reducer Pattern (complex state)

Encapsulates state transitions behind an action enum:

```rust
fn app() -> Element {
    let mut state = use_signal(|| AppState::default());
    // Dispatch actions that the state knows how to handle
    state.write().apply(Action::Increment);
}
```

Best for: complex state with many mutation types.

Source: `examples/04-managing-state/reducer.rs`

## 5. Memo Chains (derived state)

Chain memos for computed values that depend on other signals:

```rust
let count = use_signal(|| 0);
let doubled = use_memo(move || count() * 2);
let message = use_memo(move || format!("Value: {}", doubled()));
```

Memos only recompute when their dependencies change.

Source: `examples/04-managing-state/memo_chain.rs`

## 6. Error Boundaries

Catch panics in child components:

```rust
rsx! {
    ErrorBoundary {
        handle_error: |error| rsx! { p { "Error: {error}" } },
        MayFailComponent {}
    }
}
```

Source: `examples/04-managing-state/error_handling.rs`

## 7. Async State (use_resource / use_future)

```rust
// For data that returns a value
let data = use_resource(move || async move {
    fetch_data().await
});

// For fire-and-forget tasks
use_future(move || async move {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        count += 1;
    }
});
```

Source: `examples/05-using-async/future.rs`, `suspense.rs`

## Pattern Selection Guide

| Need | Pattern | Example |
|------|---------|---------|
| App-wide singleton | `Signal::global()` | Theme toggle, user auth |
| Subtree-scoped | `use_context_provider()` | Component library theme |
| Single component | `use_signal()` | Button toggle, form input |
| Complex mutations | Reducer | Multi-action state machine |
| Derived values | `use_memo()` / `Memo::global()` | Filtered lists, computed totals |
| Async data | `use_resource()` | API calls, file loading |
| Background task | `use_future()` | Polling, timers |
