# CosmixOS Guiding Principles

## Goal 1: Self-Hosting — CosmixOS builds CosmixOS

The primary technical goal. CosmixOS and OpenBrane must be capable of
developing, maintaining, and evolving themselves. The AI companion, the
scripting layer, the mesh network, the web dashboard, the desktop
environment — all of it used daily to build more of itself.

This is the bootstrapping test: if the system isn't good enough to build
itself with, it isn't good enough to ship. Every missing feature, every
rough edge, every workflow gap is felt immediately because the developers
are the users.

Self-hosting means:
- Writing cosmix code inside a CosmixOS container
- Using OpenBrane's AI agents to research, plan, and review changes
- Using cosmix-port scripts to automate builds, tests, and deployments
- Using the mesh to coordinate across dev/staging/production nodes
- Using cosmix-web to monitor and manage the infrastructure
- Dogfooding every component, every day

## Goal 2: Human Companion — AI that helps people plan their lives

The primary human goal, and the reason the project exists beyond
technical curiosity.

CosmixOS + OpenBrane should enable any person — especially those facing
later life, illness, or complex personal circumstances — to sit down
with an AI companion and organise the things that matter:

- **Medical preferences** — advance care directives, pain management
  wishes, medication history, allergies, GP and specialist contacts
- **Personal wishes** — funeral preferences, messages to family,
  distribution of possessions, stories worth preserving
- **Living documents** — not static PDFs filed once and forgotten, but
  structured data that can be queried, updated, and shared with the
  people who need it when they need it
- **Gentle prompting** — regular check-ins via OpenBrane routines:
  "Has anything changed?" "Anything new to record?" The AI removes
  the barrier of not knowing where to start
- **Portable and private** — the entire environment runs locally,
  can be snapshotted, and handed to a family member or executor as
  a self-contained unit. No cloud dependency. No subscription that
  expires when you can't renew it.

Most people don't write these things down. It's overwhelming, it's
confronting, and there's no obvious structure. An AI companion that
interviews you conversationally, organises your answers into searchable
records, and nudges you to keep them current — that changes the equation.

The medical professional who needs to know your wishes at 2am shouldn't
have to call a family member who may not remember. The executor who
handles your affairs shouldn't have to guess. The grandchild who wants
to know your story shouldn't find silence.

**OpenBrane is the interface** — shared data surfaces where the human
door and the agent door meet. Clips for quick notes, Memory for
structured records, Queues for pending tasks, Routines for ongoing
check-ins. The AI sees what you've shared. You see what the AI has
organised. Both sides contribute.

## The Relationship Between the Goals

Goal 1 serves Goal 2. The self-hosting discipline ensures that
CosmixOS is actually usable, reliable, and polished enough to hand
to someone who isn't a developer. If the people building it can't
use it comfortably, the people it's meant to help certainly can't.

Goal 2 grounds Goal 1. Without a human purpose beyond "cool tech,"
the project becomes an exercise in architecture for architecture's
sake. The human companion use case demands simplicity, reliability,
privacy, and empathy in the design — qualities that make every other
use case better too.

---

*Document created: 2026-03-14*
*Status: Foundational — these principles govern all design decisions*
