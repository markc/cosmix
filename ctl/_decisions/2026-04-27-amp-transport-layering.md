---
title: ABP / Transport Layering and the iroh Question
date: 2026-04-26
status: directional
next_review: 2026-07-26
draws_from: ["_spec/README.md", "_spec/2026-04-20-00-constitution.md", "_spec/2026-03-24-01-bus-wire-protocol.md", "CLAUDE.md"]
tags: ["amp", "transport", "iroh", "iroh-blobs", "iroh-net", "iroh-gossip", "postcard", "webrtc", "decision-record"]
---

# ABP / Transport Layering and the iroh Question

Companion memo to the 2026-04-26 amendment of `_spec/2026-03-24-01-bus-wire-protocol.md`
and `_spec/README.md`. Captures the reasoning that motivated the amendment
so the spec can cite a single rationale source instead of re-deriving the
argument every time. Not canon — directional.

## What changed in the spec

Previously, the spec phrasing conflated **ABP** (the wire format — markdown
frontmatter + body, RC codes, command vocabulary) with **"ABP over WebSocket
over WireGuard"** (one specific transport stack). The amendment separates
the two: ABP is the application protocol; transports are pluggable per
workload. The amendment lists currently working transports (WebSocket,
postcard, planned WebRTC) and names `iroh-blobs`, `iroh-net`, and
`iroh-gossip` as candidates under evaluation.

The amendment is *clarifying*, not directional — it formalises a separation
the architecture had always admitted in principle but obscured in phrasing.
No technology has been adopted; no commitment has been made.

## Why the conflation was a problem

The previous phrasing produced two concrete drifts:

1. **It ruled out by phrasing what was admitted in principle.** The hot-path
   postcard plan (compositor↔shell at 165Hz) and the WebRTC media plan
   already required transports other than WebSocket-over-WG. The spec listed
   them as exceptions to ABP rather than as transports underneath ABP. This
   muddled the question every time a new workload appeared: *is this an
   exception to the protocol, or a transport choice?* (Almost always the
   latter.)

2. **It obscured candidates that deserve explicit evaluation.** The "native
   sync engine using postcard over raw TCP on WireGuard" plan
   (`_spec/README.md` §Encryption Layering, pre-amendment) would reinvent
   most of `iroh-blobs`: content-addressed (BLAKE3) chunking, parallel
   transfer, resume on interrupt, dedup. With the spec naming WebSocket-over-WG
   as "the" ABP transport, `iroh-blobs` had no natural place to be evaluated.
   The amendment gives it one (§8.3).

## What ABP is good at, what it is bad at

**Good at (keep):**

- Cat-debuggable. Markdown frontmatter + body. An agent or human can `tail`
  a port log and read the messages without a decoder. Directly serves the
  mandate's legibility criterion (`CLAUDE.md` §"Three design criteria").
- Three-reader principle. Same wire format works for the application, for
  ad-hoc tooling, and for the LLM. Binary protocols force a tooling layer
  between the agent and the bytes.
- Conversational volume. App-to-app commands (`ui.window`, `file.list`,
  `noded.ping`) are tens-to-thousands per second across a whole node. Latency
  tolerance is human-perceptible. ABP fits this comfortably.

**Bad at (don't try):**

- Hot-path events. Markdown parsing + JSON body deserialization is ~10µs
  minimum, plus a broker-relay hop adds a process boundary. At 165Hz pointer
  events × multiple surfaces, the parse cost alone becomes meaningful. The
  postcard plan correctly takes this off ABP.
- Bulk binary transfer. Stuffing file blocks into ABP frames is silly.
  Postcard-over-TCP-on-WG was the planned answer. This is the strongest
  case to revisit (see `iroh-blobs` below).
- Broker bottleneck (theoretical). Today the broker relays everything. At very
  high message rates the single relay process is the ceiling. Cosmix is
  nowhere near that ceiling and may never reach it for ABP traffic.

## What iroh actually is

`iroh` is a Rust networking stack from n0, evolved from the IPFS ecosystem.
Three relevant pieces:

- **`iroh-net`** — QUIC-based peer-to-peer connectivity. NAT traversal via
  DERP relays. Cryptographic node identity (Ed25519). Sub-millisecond
  direct-connect RTT when peers can reach each other directly.
- **`iroh-blobs`** — content-addressed blob transfer. BLAKE3 hashing,
  parallel chunking, resumable streams, built-in dedup. Built on
  `iroh-net`.
- **`iroh-gossip`** — epidemic pub/sub. O(log N) per-node fan-out cost,
  higher total message count than a broker-mediated topic.

All pure Rust, all by a team that has shipped this for years.

## Where iroh fits in the Cosmix stack

### `iroh-blobs` — the strongest case

The current "native sync engine over postcard-over-TCP-on-WG" plan would
build a chunked, resumable, deduplicated bulk transfer engine. That engine
already exists as `iroh-blobs`. Reasonable evaluation criteria for the
spike:

- **Throughput** on the WG link, vs custom postcard chunking. Probably
  comparable or better; iroh's parallel chunking helps for large files.
- **Peak RSS** during transfer. Iroh has more machinery (DERP, QUIC
  state); the cost depends on configuration.
- **Dependency footprint.** `iroh-blobs` pulls in QUIC + iroh-net + crypto.
  Not trivial. Worth knowing the transitive size.
- **API composability.** ABP-level orchestration (which file, where to
  put it, who's syncing) stays ABP. iroh-blobs only moves the bytes. The
  question is how cleanly the seam draws.
- **Operational complexity.** Iroh nodes have identity. Identity
  management on top of WG-mesh-membership is duplication if iroh
  is purely an internal transport. Configurable away (use a known node
  ID per WG peer), but worth checking.

The spike answer is binary: either iroh-blobs replaces the planned custom
engine, or it doesn't. Either outcome is fine. The wrong outcome is
shipping a custom engine without the comparison.

### `iroh-net` — the deferred / constitutional one

`iroh-net` brings two things WireGuard doesn't:

1. **NAT traversal across the open internet** — peers behind separate NATs
   can still find each other via DERP relays. WG requires preshared config
   per peer.
2. **Per-peer node-identity trust** — instead of "trust everyone on the
   subnet," peers carry Ed25519 identities and authorise per-peer.

These are *trust-model* differences, not just transport differences. Article
III.2 of the constitution commits to "WG /24 is the trust domain, no
per-message auth." Adopting iroh-net as a *transport inside WG* is a tech
choice, no constitutional change. Adopting iroh-net *as the primary trust
mechanism* — letting peers connect across WG domains using only Ed25519 IDs
— would amend III.2.

The two should not be conflated. The spec amendment makes this explicit
(SPEC 01 §8.5). For now, iroh-net's primary appeal is *future* — when
cross-mesh peering becomes a real need beyond the operator-managed WG /24.
Today: not needed. The candidate is named so the question doesn't have to
be re-discovered later.

### `iroh-gossip` — the deferred one

The broker topic extension is O(N) per publish at the broker. Gossip is
O(log N) per node but more total messages overall. For Cosmix's current
scale (a handful of nodes, modest topic traffic) the broker-mediated model
is fine and simpler.
If federation grows the mesh past dozens of nodes, gossip becomes
interesting. **Premature today.** The candidate is named so the option
doesn't get forgotten.

## What the amendment does *not* do

- **It does not adopt iroh anywhere.** Naming a candidate in §8 of SPEC 01
  is an evaluation slot, not a commitment. Adoption requires a spike with
  measured benefit.
- **It does not change the trust model.** Article III.2 stands untouched.
  WG /24 remains the mesh trust domain. iroh transports, if adopted, run
  inside that trust domain by default.
- **It does not modify the ABP wire format.** The amendment is below the
  wire format, in the transport layer. The format (markdown frontmatter,
  EOM marker, RC codes, command vocabulary) is unchanged.
- **It does not affect SPECs 02, 03, 04, 05, 06, 07, 08, or 09.** Those
  chapters speak about ABP semantics, not transport. They remain valid as
  written.

## Open questions to resolve through spike work

1. **iroh-blobs vs custom postcard sync engine.** Throughput, RSS,
   dependency footprint, API composability. Decide before any custom sync
   engine work begins.
2. **Broker saturation point.** At what message rate does
   `cosmix-noded`'s single-process relay become the ceiling? A simple
   benchmark — push N messages/sec through noded between two clients,
   measure tail latency vs throughput — would tell us whether gossip is a
   5-month problem or a 5-year problem.
3. **ABP body parsing cost.** Markdown frontmatter parse + JSON body
   deserialize, in microseconds, on representative messages. Important
   input to "what's the actual ceiling of ABP-over-WebSocket?"

None of these are blocking the amendment. They are downstream investigations
the amendment makes legible.

## Status and review

This memo is *directional*. It documents the reasoning behind a clarifying
spec amendment. It commits to nothing. It will be reviewed when either (a)
one of the spike investigations above produces a result that warrants
adoption (in which case a substantive spec amendment follows), or (b)
2026-07-26 passes without movement, at which point the candidate evaluations
should either advance or be explicitly parked.
