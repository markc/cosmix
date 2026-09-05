---
title: Composition and installation
chapter: 2
version: 0.1.0
status: draft
date: 2026-09-05
---

# Composition and installation

## Baseline layout

The audited monorepo has two Cargo workspaces, not a single kernel crate tree:

| Area | Responsibility | Evidence |
| --- | --- | --- |
| `src/crates/` | Bus, Mix, daemon and substrate libraries | [main workspace manifest](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/Cargo.toml) |
| `src/desktop/` | Compositor, shell, toolkit and desktop apps | [desktop manifest](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/desktop/Cargo.toml) |
| `docs/` | Public manuals and website sources | [repository guide](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/AGENTS.md) |
| `bootstrap`, `setup.mix` | Bootstrap and build/install orchestration | [setup](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/setup.mix) |

**COMP-001 — Preserve dependency direction.** The intended family layering is
Bus foundations → Mix integration → daemon/application composition. A cycle-free
Cargo graph alone does not prove every architectural dependency is appropriate.
Do not make pure property types depend on their storage or daemon adapters.

`cosmix-lib-props-core` supplies read-side pure types, with optional Bus integration
and revisioned-write support. `cosmix-lib-props-store` owns namespace registration,
storage, mutation routing, hooks, authorisation and audit integration. These are
separate responsibility boundaries, not duplicate implementations.

**COMP-002 — Keep daemon adapters thin.** Reusable domain invariants belong in
libraries. Daemon adapters supply authenticated context, persistence and transport.
Mix supplies external orchestration. This is a target architecture; the directory
layout by itself is not a completed extraction audit of every daemon.

## Paths and install modes

**COMP-003 — Resolve paths centrally.** Use the common path contract rather than
embedding operator paths in crates. At baseline daemon and Mix path implementations
are separate and must be kept consistent:
[daemon paths](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-lib-config/src/paths.rs),
[Mix paths](https://github.com/markc/cosmix/blob/96d12fdf3fa3dfb2bf86b5bdc02d8ec4f9a415be/src/crates/cosmix-mix/src/cosmix_paths.rs).

`COSMIX` identifies a development root. `COSMIX_SRC`, `COSMIX_ETC`, `COSMIX_VAR`,
`COSMIX_BIN`, `COSMIX_RUN`, `COSMIX_LOG` and `COSMIX_TMP` override individual
directories. System binaries install under `/opt/cosmix/bin`; system operation
without a surrounding checkout uses the resolver's FHS/XDG paths. Do not conflate
the installed binary tree with the development workspace or a private overlay.

**COMP-004 — Keep install and source evidence distinct.** A source version or
successful build does not identify the running executable. Capture binary path,
version, configuration root and enabled features when claiming runtime conformance.
No deployment is implied by publishing specifications.

## Build and compatibility gates

**COMP-005 — Verify both workspaces when affected.** The main workspace's tests
do not cover the separate desktop workspace. Use the relevant pinned toolchains,
feature sets and supported targets; record exact commands and results.

**COMP-006 — Distinguish compatibility dimensions.** Track crate API, wire/schema,
storage and deployment compatibility separately. A private overlay must not alter
public protocol meaning invisibly. Tightening validation can reject previously
accepted stored values; inspect real ingestion paths before labelling it risk-free.

Existing main-workspace dependencies include ordinary Cargo version requirements;
the old constitution's blanket exact-version rule is not demonstrated by that
manifest. Resolve that policy discrepancy rather than describing it as enforced.
