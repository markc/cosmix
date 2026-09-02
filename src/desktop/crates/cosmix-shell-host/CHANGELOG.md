# Changelog

## 0.4.0 — 2026-09-02

- Add `LayerHostWake`: a thread-safe, coalescing wake handle producers outside
  Bevy (the ctk Bus worker) use to make a blocked calloop dispatcher runnable.
  Capacity one collapses bursts into a single pending update; a wake arriving
  after runner teardown is an inert no-op.
