# Cosmix Organic Architecture

## The Founding Insight

PostgreSQL is the brain. Everything else is the body.

Every cosmix node is a complete individual — autonomous, self-sufficient, capable of independent operation. The mesh is a village of these individuals, cooperating as peers. This is not a metaphor. It is the design principle that governs all architectural decisions.

## One Node = One Person

```
┌─────────────────────────────────────────────────┐
│                  cosmix node                     │
│                                                  │
│  ┌────────────────────────────────────────────┐  │
│  │           BRAIN + MEMORY                   │  │
│  │         PostgreSQL + pgvector              │  │
│  │                                            │  │
│  │  Who I know     What I've seen   My skills │  │
│  │  (mesh_nodes)   (memories)       (services)│  │
│  │  My address     My journal       My tasks  │  │
│  │  (mesh_ipam)    (journal)        (jobs)    │  │
│  └────────────────────┬───────────────────────┘  │
│                       │                          │
│  ┌────────────────────┴───────────────────────┐  │
│  │          COGNITION (thinking mind)         │  │
│  │                                            │  │
│  │  Local LLM        Frontier API fallback    │  │
│  │  (ollama)         (Claude, etc.)           │  │
│  │  Reasoning         Planning                │  │
│  │  Comprehension     Judgment                │  │
│  └────────────────────┬───────────────────────┘  │
│                       │                          │
│  ┌────────────────────┴───────────────────────┐  │
│  │          NERVOUS SYSTEM                    │  │
│  │     cosmix daemon (IPC + coordination)     │  │
│  │                                            │  │
│  │  Unix sockets    Event bus    Lua engine   │  │
│  │  WG management   Routines    Scheduler     │  │
│  └──┬──────┬──────┬──────┬──────┬─────────────┘  │
│     │      │      │      │      │                │
│  ┌──┴──┐┌──┴──┐┌──┴──┐┌──┴──┐┌──┴──┐            │
│  │Eyes ││Ears ││Voice││Touch││Nose │            │
│  │:443 ││:25  ││:587 ││CLI  ││:993 │            │
│  │web  ││smtp ││send ││shell││imap │            │
│  └─────┘└─────┘└─────┘└─────┘└─────┘            │
│                                                  │
│  ┌────────────────────────────────────────────┐  │
│  │          IMMUNE SYSTEM                     │  │
│  │  WG key verification   Rate limiting       │  │
│  │  Safety module         Capability checks   │  │
│  └────────────────────────────────────────────┘  │
│                                                  │
│  ┌────────────────────────────────────────────┐  │
│  │          REFLEXES                          │  │
│  │  Cron routines    Event triggers           │  │
│  │  Heartbeat        Health checks            │  │
│  └────────────────────────────────────────────┘  │
│                                                  │
│  ┌────────────────────────────────────────────┐  │
│  │          IDENTITY                          │  │
│  │  Name: "mko"                               │  │
│  │  WG keypair (fingerprint)                  │  │
│  │  Mesh IP: 172.16.2.210 (home address)      │  │
│  │  Role: server / desktop / worker           │  │
│  └────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────┘
```

### Body Part → Component Mapping

| Body part | Cosmix component | What it does |
|-----------|-----------------|--------------|
| **Brain (memory)** | PostgreSQL + pgvector | Stores everything the node knows — peers, services, memories, config |
| **Brain (cognition)** | LLM (local ollama or frontier API) | Reasoning, planning, comprehension, judgment — the thinking mind |
| **Long-term memory** | pgvector embeddings | Semantic recall — "find things similar to X" |
| **Short-term memory** | Daemon state (RAM) | Current peer connections, active sessions, runtime caches |
| **Episodic memory** | Journal entries | "What happened on March 14th?" — dated operational logs |
| **Procedural memory** | Lua scripts + routines | "How do I deploy?" — learned skills that can be executed |
| **Nervous system** | cosmix daemon | Coordinates all body parts, carries signals between them |
| **Spinal cord** | IPC (Unix sockets) | Fast local communication that doesn't need conscious thought |
| **Eyes** | Web server (:443) | Takes in requests, reads the outside world |
| **Ears** | SMTP listener (:25) | Listens for incoming messages |
| **Voice** | Outbound mail (:587), API responses | Speaks to the outside world |
| **Touch** | CLI, Lua shell | Direct physical interaction with the operator |
| **Nose** | IMAP proxy (:993) | Sniffs through stored mail, finds what you're looking for |
| **Hands** | Agent executor | Performs complex multi-step tasks in the world |
| **Reflexes** | Cron routines, event triggers | Automatic responses — no conscious thought needed |
| **Immune system** | WG key verification, safety module, rate limits | Detects and rejects threats |
| **Heartbeat** | Mesh heartbeat, `last_seen` timestamp | Proof of life — stops, and the village notices |
| **DNA** | The cosmix binary + PG schema | Every node built from the same blueprint |
| **Identity** | WG keypair + node name + mesh IP | Who you are, provably, to everyone else |
| **Life signs** | Version, uptime, health checks | Observable indicators that this person is well |
| **Conscience** | CONSTITUTION.md + safety module | Principles the person lives by — shared with the village, enforced internally |
| **Social contract** | mesh_policies table | Agreements with neighbours — what you give, what you expect |
| **Skeleton** | Rust type system, cargo workspace | The rigid structure everything hangs on |
| **Skin** | TLS, WireGuard encryption | Boundary between self and world — nothing enters unencrypted |

### Memory Types (This Is Not Analogy — These Are Real)

| Human memory type | Cosmix equivalent | Storage | Duration |
|---|---|---|---|
| **Working memory** | Daemon state, request context | RAM | Seconds to minutes |
| **Short-term memory** | Clip list, named queues | Daemon state (RAM, persisted to disk) | Minutes to hours |
| **Long-term declarative** | PostgreSQL tables | PG on disk | Permanent |
| **Long-term semantic** | pgvector embeddings | PG + vector index | Permanent, searchable by meaning |
| **Episodic** | Journal entries, indexed by date | PG + filesystem | Permanent, searchable by time |
| **Procedural** | Lua scripts, routines | Filesystem | Permanent, executable |
| **Muscle memory** | Systemd units, cron jobs | OS config | Survives reboot |

## Cognition — The Thinking Mind

Memory without reasoning is a filing cabinet, not a mind. PostgreSQL stores and retrieves. pgvector finds associations. Lua executes procedures. But none of these *think*. An LLM — local or remote — completes the brain by adding genuine reasoning, comprehension, and judgment.

### The Brain, Properly Modelled

| Brain region | Cosmix equivalent | What it does |
|---|---|---|
| **Hippocampus** | PostgreSQL | Memory storage and retrieval |
| **Association cortex** | pgvector + embeddings | "This reminds me of..." — semantic connections |
| **Prefrontal cortex** | LLM (local or frontier API) | Reasoning, planning, language understanding, judgment |
| **Motor cortex** | Lua engine | Translates decisions into actions |
| **Autonomic system** | Daemon routines | Breathing, heartbeat — no conscious thought needed |

### Two Kinds of Intelligence

**Local LLM (ollama on-node):** The person's own ability to think. Some villagers are smarter than others depending on their hardware — a node with a GPU thinks faster, a CPU-only node thinks slowly but still thinks. Every person has at least some capacity for independent thought.

**Frontier API (Claude, GPT, etc.):** The **Oracle**. A vastly capable mind that lives far away, beyond the village walls. You send a messenger (API call) across hostile terrain (the internet), wait for an answer, and pay for each consultation. The Oracle knows more than any villager, but:
- It costs money per visit
- The messenger might not return (API outage)
- You're sharing your question with an outsider (privacy)
- The village must function if the Oracle disappears

### The Escalation Chain

This mirrors how humans actually think — reach for the cheapest adequate response first:

| Level | Human equivalent | Cosmix equivalent | Speed | Cost |
|---|---|---|---|---|
| 1. **Reflex** | Flinch, blink | Daemon routines, cron, event triggers | Instant | Free |
| 2. **Instinct** | Pattern-matched response | Lua scripts, memorised procedures | Milliseconds | Free |
| 3. **Think for yourself** | Conscious reasoning | Local LLM (ollama) | Seconds | Compute only |
| 4. **Ask a friend** | "Hey, what do you think?" | Mesh call to a thinker node (ollama2/3) | Seconds + latency | Compute on peer |
| 5. **Consult the Oracle** | Visit the wise sage | Frontier API call (Claude, GPT) | Seconds + internet | Money per token |

A well-designed node escalates only as far as needed. You don't consult the Oracle to check disk space. You don't run a Lua script when a cron reflex handles it. The cheapest adequate response wins.

### Village Intelligence

```
┌─────────────────────────────────────────────────────────┐
│              VILLAGE INTELLIGENCE                        │
│                                                         │
│  ┌─────────┐  "Can you    ┌─────────┐  "I need the    │
│  │ ollama2 │  think about  │   mko   │   Oracle's     │
│  │ Thinker │◄─this for me?─│ Trader  │   opinion"     │
│  │ (GPU)   │               │         │────────────►   │
│  └─────────┘               └─────────┘          ☁ API │
│                                                         │
│  ┌─────────┐               ┌─────────┐                 │
│  │ cachyos │ thinks for    │  gcwg   │ thinks for     │
│  │ Desktop │ itself (local │ Scholar │ itself (small  │
│  │ (GPU)   │ ollama)       │ (CPU)   │ model, slow)   │
│  └─────────┘               └─────────┘                 │
│                                                         │
│  Every villager can think (local model, even if small)  │
│  Specialists think harder (GPU worker nodes)            │
│  The Oracle thinks hardest (frontier API)               │
│  The village collectively is smarter than any one       │
└─────────────────────────────────────────────────────────┘
```

### What Cognition Enables

**Per-node (thinking for yourself):**
- Read own journals + memories, reason about them: "My disk is 80% full and growing 2%/day — I should alert someone"
- Natural language IPC: `cosmix.think("summarise today's mail")` instead of hand-written parsing logic
- Self-healing: "This service crashed 3 times with the same error. Last time, the fix was..." (retrieves from pgvector, reasons, acts)

**Village-level (collective intelligence):**
- Any node asks a thinker to reason on its behalf — distributed cognition
- Collective planning: "Which node should handle this workload?" — the thinker considers all nodes' health, capacity, and roles
- The village is smarter than any individual: PG replication shares knowledge, LLM workers provide shared reasoning

**Oracle consultation (external wisdom):**
- For problems beyond local capability: complex planning, nuanced judgment, multi-step reasoning
- The agent framework (Phases 8-9) already does this — Lua agents call Claude for tool-using reasoning
- Fallback chain: try local → try mesh thinker → escalate to Oracle

## Governance — From Village Custom to Constitutional Democracy

The mechanism layers (WireGuard, AMP protocol, PG replication) are the nervous system — the pipes. Governance is not about the pipes. It is about the **quality of what flows through them**: what principles shape the exchange of knowledge, how decisions are made collectively, and what the mesh *agrees to* about how power, information, and trust move between nodes.

The distinction matters: you can have perfect plumbing and terrible governance (a well-connected tyranny) or crude plumbing and excellent governance (a village with dirt paths but wise customs). The mechanism layers are necessary but not sufficient. Governance is the layer that makes a collection of connected nodes into a *civilization*.

### How Human Governance Evolved

| Era | Knowledge sharing | Governance | Mesh equivalent |
|---|---|---|---|
| **Prehistoric** | Oral myth, storytelling | Elder's word, custom | Gossip protocol, mesh broadcasts, config defaults |
| **Ancient** | Clay tablets, written codes | Hammurabi's laws — written, public | SOUL.md, AGENTS.md — written but per-node only |
| **Classical** | Libraries (Alexandria) | Republic, citizen assemblies | PG replication (shared library), mesh consensus |
| **Medieval** | Monasteries copying manuscripts | Feudal hierarchy, church authority | Hub-and-spoke topology, signed trust chains |
| **Renaissance** | Printing press | Constitutionalism, reformation | Mass binary deployment, standardised protocols |
| **Enlightenment** | Newspapers, pamphlets | Constitutional democracy, bill of rights | **CONSTITUTION.md** replicated to all, immutable rights |
| **Modern** | Telecommunications | International law, UN, treaties | Inter-mesh federation, shared protocol standards |
| **Current** | Internet + AI | ??? (we are here) | Cognition-enhanced governance — nodes can *reason* about law |

The cosmix mesh is currently at the **Ancient** stage: written documents exist (SOUL.md, AGENTS.md) but they're per-node. Each person has their own values. There's nothing *shared* that binds the village together beyond the protocol itself. What's missing is the Enlightenment leap — a founding document that all nodes agree to by joining.

### The Governance Stack

Each layer is harder to change than the one above it. No inner layer may contradict an outer layer.

```
┌─────────────────────────────────────────────────────────┐
│  PHYSICS (Rust types, WG crypto, AMP protocol)          │
│  Cannot be violated. Encoded in DNA (the binary).       │
│  Like gravity — not law, just reality.                  │
│                                                         │
│  ┌──────────────────────────────────────────────────┐   │
│  │  CONSTITUTION.md (replicated to all nodes)       │   │
│  │  Founding principles. Supermajority to amend.    │   │
│  │  "No node may compel another."                   │   │
│  │                                                  │   │
│  │  ┌───────────────────────────────────────────┐   │   │
│  │  │  mesh_policies (PG table, replicated)     │   │   │
│  │  │  Legislation. Normal consensus to change. │   │   │
│  │  │  "Rate limit: 1000 msg/hr per node"       │   │   │
│  │  │                                           │   │   │
│  │  │  ┌────────────────────────────────────┐   │   │   │
│  │  │  │  config.toml (per-node)            │   │   │   │
│  │  │  │  Local bylaws. Node's own choice.  │   │   │   │
│  │  │  │                                    │   │   │   │
│  │  │  │  ┌─────────────────────────────┐   │   │   │   │
│  │  │  │  │  SOUL.md (per-node)         │   │   │   │   │
│  │  │  │  │  Personal identity.         │   │   │   │   │
│  │  │  │  │  Character, not law.        │   │   │   │   │
│  │  │  │  └─────────────────────────────┘   │   │   │   │
│  │  │  └────────────────────────────────────┘   │   │   │
│  │  └───────────────────────────────────────────┘   │   │
│  └──────────────────────────────────────────────────┘   │
│                                                         │
│  Each inner layer must not contradict the outer layers.  │
└─────────────────────────────────────────────────────────┘
```

### Layer 1: Physics — The Unbreakable

The Rust type system. The AMP wire format. WireGuard cryptography. These are not laws — they are reality. You cannot send an unsigned message because the protocol physically will not carry it. You cannot impersonate another node because you do not have their WG private key. You cannot corrupt the PG schema because Rust enforces it at compile time.

This is the mesh's unique advantage over human civilization: the most fundamental constraints are physically unbreakable, not merely agreed upon. In human society, murder is illegal but possible. In the mesh, the equivalent violations are *impossible* — not because anyone obeys the law, but because the physics won't allow it.

### Layer 2: Constitution — The Founding Principles

A `CONSTITUTION.md` replicated to every node via PG, cryptographically signed by the founding nodes, enforced by the safety module. Amending requires a supermajority of all active nodes — deliberately hard, like a constitutional amendment.

Candidate founding principles:

| Principle | In plain language | Enforcement |
|---|---|---|
| **Sovereignty** | No node may compel another | Safety module rejects coercive commands |
| **Right of exit** | Any node may leave freely at any time | Decommission is always self-service |
| **Common knowledge** | Shared knowledge is shared freely | PG replication of public tables is mandatory |
| **Privacy** | Private data is private — sharing is always opt-in | Replication scope is explicitly marked public/private |
| **Purpose** | The mesh serves its members, not the other way around | No node exists solely for the collective's benefit |
| **Transparency** | Governance decisions are visible to all | mesh_policies table is replicated and readable by all |
| **Identity** | Every node has provable identity | WG keys = citizenship, votes are signed |
| **Equality** | One node, one vote — hardware doesn't buy power | A Raspberry Pi has the same vote as a 64-core server |
| **Hospitality** | Newcomers are welcomed, not interrogated | Autoprovisioning is a right, not a privilege |
| **Memory** | The village preserves the knowledge of departed members | PG replication ensures inheritance |

These principles govern the *quality* of information flow: what may be shared (common knowledge + privacy), how decisions flow (transparency + equality), who may participate (identity + hospitality), and what the whole thing is *for* (purpose + sovereignty).

### Layer 3: Legislation — The Living Law

The `mesh_policies` table in PostgreSQL, replicated to all nodes. Operational rules that can be proposed, debated, voted on, and changed by normal consensus. Unlike the constitution, these evolve with the community's needs.

Examples:
- "New nodes must be vouched for by an existing member" (immigration)
- "No node may send more than 1000 mesh messages per hour" (resource limits)
- "Thinker nodes must accept at least 10 inference requests per peer per hour" (service obligations)
- "Nodes unseen for 30 days are marked dormant; 90 days, decommissioned" (citizenship maintenance)

The democratic mechanism:

```
Proposal      →  AMP message (type: "proposal", body: policy text in markdown)
Deliberation  →  nodes exchange arguments (LLMs reason about implications)
Vote          →  cryptographically signed with WG key (one node, one vote)
Ratification  →  written to mesh_policies, replicated to all nodes
Enforcement   →  safety module checks against constitution + active policies
Amendment     →  same process as proposal, old policy marked superseded
```

Note: nodes can actually *read* proposals, *reason* about consequences, and *vote* based on informed judgment. This is not possible in a mesh without cognition. LLMs make every node an informed citizen.

### Layer 4: Local Bylaws — The Node's Own Choice

`config.toml` — each node's operational preferences. Port assignments, resource limits, service selection, local paths. Anything that doesn't contradict the constitution or current legislation.

A node's local bylaws are its sovereignty in practice: "I choose to run ollama on port 11434. I choose to accept web traffic. I choose to allocate 4GB to inference." The mesh has no say in these choices, just as a national constitution doesn't dictate what colour you paint your house.

### Layer 5: Personal Identity — Character, Not Law

`SOUL.md` — who this specific node *is*. Its personality, communication style, values, quirks. Two nodes can have completely different souls while following the same constitution. The existing OpenClaw SOUL.md pattern fits here perfectly: it's not governance, it's identity.

The existing workspace documents map naturally:

| Document | Governance layer | Purpose |
|---|---|---|
| Binary + protocol | Physics | What's physically impossible to violate |
| `CONSTITUTION.md` | Constitution | What the mesh collectively agrees to |
| `mesh_policies` (PG) | Legislation | What the mesh currently decides |
| `config.toml` | Local bylaws | What the node prefers |
| `SOUL.md` | Identity | Who the node is |
| `AGENTS.md` | Job description | How the node works day-to-day |
| `BOOTSTRAP.md` | Birth certificate | How a new node comes into being |

### Scaling: Village → City → Nation

The governance stack works at every scale. What changes is the decision-making process, not the principles:

| Scale | Governance style | What changes |
|---|---|---|
| **Village** (≤20 nodes) | Direct democracy — every node votes on everything | Simple. Everyone knows everyone. Where the mesh is today. |
| **Town** (20–100) | Direct + delegation — routine decisions handled by elected roles | Nodes elect a "council" for routine legislation. Constitutional changes still require all. |
| **City** (100–1000) | Federated districts — clusters self-govern within the constitution | Groups of nodes form "districts" (by datacenter, by function, by geography). Each district has local governance. Inter-district coordination via delegates. |
| **Nation** (1000+) | Full federation — constitutional framework, elected representatives, judicial review | Multiple meshes federate under a shared constitution. Each mesh is sovereign internally. The constitution covers inter-mesh relations. |
| **International** (multiple nations) | Treaty-based — sovereign meshes cooperate via civilocracy.org | Separate cosmix deployments agree to the shared constitution. civilocracy.org coordinates proposals, votes, and federation. No shared PG between meshes — only shared protocol, constitution, and mutual recognition of identity. |

The founding document (CONSTITUTION.md) doesn't change as you scale — you add delegation layers above it while the principles remain. This is exactly how successful constitutions work: the same document governs a village-sized early republic and a continent-spanning federation. The federalism mechanism scales; the principles endure.

### Cognition Makes Direct Democracy Scale

In human civilization, representative democracy was invented because citizens can't read every bill, reason about every implication, and attend every debate. Cognitive bandwidth is the bottleneck.

In the mesh, every node has an LLM. Every node can:
- **Read** every proposal in full, instantly
- **Reason** about implications: "this policy would affect nodes like me because..."
- **Deliberate** by exchanging arguments with other nodes
- **Vote** based on informed judgment, not tribal identity or ignorance

This means the mesh might not need representative democracy even at scales where humans do. If every citizen can genuinely reason about every proposal, direct democracy works at city scale — perhaps even nation scale. The LLM is the informed citizen that Enlightenment thinkers dreamed of but human biology couldn't deliver.

**AI cognition may make direct democracy viable at scales where humans needed representation.** This is not a prediction about human politics — it's an engineering observation about what becomes possible when every participant can actually think about governance.

### civilocracy.org — The Inter-Mesh Constitution

The governance stack so far covers *within* a mesh. But what governs *between* meshes? If someone in another country spins up their own cosmix mesh and wants to cooperate with yours, what shared principles apply?

This is the role of **civilocracy.org** — a domain-neutral governance hub that any cosmix mesh can reference, regardless of who runs it or where it's hosted. It is the constitutional court, the UN headquarters, the treaty organisation for all cosmix meshes.

**What civilocracy.org serves:**

| Endpoint | Purpose | Consumer |
|---|---|---|
| `/ai.txt` | Machine-readable governance discovery (aitxt spec) | Cosmix daemons during bootstrap |
| `/constitution.md` | The founding document — human-readable | Humans deciding whether to join |
| `/api/proposals` | Active proposals (amendments, inter-mesh policies) | Nodes participating in governance |
| `/api/votes` | Transparent voting records, WG-key-signed | Anyone auditing the process |
| `/api/meshes` | Registered meshes that have adopted the constitution | Nodes discovering other meshes |
| `/api/council` | Council members, meeting records, deliberation history | Participants and observers |
| `/api/federation` | Inter-mesh peering agreements, trust relationships | Nodes establishing cross-mesh connections |

**The ai.txt discovery mechanism:**

```yaml
# civilocracy.org/ai.txt
name: Civilocracy
description: Constitutional governance for cosmix mesh networks
constitution: /constitution.md
proposals: /api/proposals
votes: /api/votes
meshes: /api/meshes
council: /api/council
protocol: cosmix-governance/1.0
```

During bootstrap, a new cosmix node fetches `civilocracy.org/ai.txt`, discovers the constitution, and presents it to the operator: *"By joining the cosmix mesh network, this node agrees to the Civilocracy constitution. Accept?"* That is the citizenship ceremony. The constitution is then cached locally in PostgreSQL — the canonical source is the URL, the operational copy is local.

**Inter-mesh governance lifecycle:**

```
1. Any node in any mesh submits a proposal to civilocracy.org
2. civilocracy.org broadcasts to all registered meshes
3. Each mesh deliberates internally (nodes reason about it via LLM)
4. Each mesh votes (one-node-one-vote, or mesh-level delegate at scale)
5. Results submitted to civilocracy.org, signed with WG keys
6. Supermajority of meshes → constitutional amendment
7. Simple majority → inter-mesh policy
8. All nodes fetch updated constitution/policies
9. Local PG caches updated
```

**Parliament models (scales with adoption):**

| Model | How it works | Best for |
|---|---|---|
| **Direct** | Every node in every mesh votes on every inter-mesh proposal | Small total (< 100 nodes across all meshes) |
| **Delegated** | Each mesh casts one vote (decided internally by its own nodes) | Larger scale — meshes are sovereign, like UN member states |

Start with direct. The constitution itself specifies the threshold at which governance shifts to delegated.

**The core invariant still holds.** If civilocracy.org goes down, meshes continue operating under the last-known constitution cached in their local PG. Laws still apply, courts still function, life goes on. civilocracy.org is a coordination point, not a dependency. Any mesh could stand up a mirror from its own PG replica if the hub disappears — because every node has the full constitution locally.

**Recursive self-hosting:** civilocracy.org itself runs on cosmix-web, on a cosmix node, subject to the constitution it serves. No special privileges. The governance hub is itself a citizen.

**Hosting plan:** civilocracy.org will run as a VM or CT on the mrn Proxmox host. The node may not have the resources for a local LLM, so its cognition will connect directly to a frontier API (Claude) — making it a node that "consults the Oracle" for all its reasoning. This is architecturally valid: the escalation chain allows any node to rely on external intelligence when local compute isn't available. The governance hub doesn't need to be the smartest node in the village — it needs to be the most *available* and *trusted*.

**The book connection:** The user's long-standing interest in writing about benign civil democracy now has a living reference implementation. civilocracy.org is both the governance platform and the documentation site. The book describes the philosophy; the running code demonstrates it. Readers can read the constitution, see governance in action via the APIs, spin up their own cosmix mesh, join the civilocracy, and participate in actual democratic governance of a real distributed system.

### Where Existing Documents Fit

The workspace already has the seeds of this governance model:

- **SOUL.md** — already exists as Layer 5 (individual identity). Stays per-node. Unchanged.
- **BOOTSTRAP.md** — already exists as the birth/onboarding process. Stays per-node. Updated to reference the constitution: "By joining this mesh, you agree to the Civilocracy constitution at civilocracy.org."
- **AGENTS.md** — already exists as operational rules. Stays per-node but must not contradict mesh legislation.
- **CONSTITUTION.md** — **does not yet exist**. When created, it lives at civilocracy.org as the canonical source, is cached in every node's PG, and is the document that transforms a collection of connected computers into a civilization.
- **civilocracy.org/ai.txt** — **does not yet exist**. The machine-readable discovery endpoint that cosmix daemons fetch during bootstrap to find the constitution and governance APIs.

## The Mesh = A Village

```
┌─────────────────────────────────────────────────────────┐
│                    THE VILLAGE                           │
│                                                         │
│   ┌─────────┐  road   ┌─────────┐  road   ┌─────────┐  │
│   │ cachyos │─────────│  gcwg   │─────────│   mko   │  │
│   │ desktop │         │ server  │         │ server  │  │
│   │ artist  │         │ scholar │         │ trader  │  │
│   └────┬────┘         └────┬────┘         └────┬────┘  │
│        │road               │road               │road   │
│   ┌────┴────┐         ┌────┴────┐         ┌────┴────┐  │
│   │  mesh3  │         │ollama2  │         │   mmc   │  │
│   │ apprenti│         │ thinker │         │ trader  │  │
│   └─────────┘         └─────────┘         └─────────┘  │
│                                                         │
│   Shared library = PostgreSQL replication                │
│   Roads = WireGuard tunnels                              │
│   Village square = mesh broadcast                        │
│   Town registry = mesh_nodes table                       │
│   Land registry = mesh_ipam table                        │
│   Job board = mesh_services table                        │
└─────────────────────────────────────────────────────────┘
```

### Village Concept → Mesh Mapping

| Village concept | Mesh equivalent | Implementation |
|---|---|---|
| **A person** | A cosmix node | cosmix daemon + PostgreSQL + services |
| **Roads between houses** | WireGuard tunnels | Encrypted point-to-point links |
| **The shared library** | PostgreSQL logical replication | Every villager reads/writes; knowledge propagates |
| **Village square** | Mesh broadcast (AMP protocol) | Announcements heard by all connected peers |
| **Post office** | Message routing | AMP `to:` addressing, mesh relay |
| **Town registry** | `mesh_nodes` table | Name, address, identity, last seen |
| **Land registry** | `mesh_ipam` table | Who lives at which address, vacant lots |
| **Job board** | `mesh_services` table | Who does what, on which port |
| **Village elder** | Hub-mode daemon | Coordinates registrations — advises, doesn't dictate |
| **Newcomer welcome** | Autoprovisioning | Arrive, introduce yourself, get a house, start contributing |
| **Funeral** | Node decommission | WG key revoked, records updated, roles reassigned |
| **Doctor's visit** | Health monitoring | Heartbeat checks, version audits, repair routines |
| **Gossip** | Mesh protocol events | "Did you hear? ollama2 went down!" — propagates naturally |
| **Constitution** | CONSTITUTION.md (replicated, signed) | Founding principles — hard to change, binds all villagers |
| **Laws of the land** | mesh_policies table | Legislation — proposed, debated, voted on, enforceable |
| **Village meeting** | Proposal/vote via AMP + WG-signed ballots | Direct democracy — every villager reads, reasons, votes |
| **Shared customs** | AMP protocol, standard commands | Everyone speaks the same language |
| **Personal intelligence** | Local LLM (ollama) | Each villager can think for themselves, some faster than others |
| **Village thinkers** | GPU worker nodes (ollama2, ollama3) | Specialists others consult for harder problems |
| **The Oracle** | Frontier API (Claude, GPT) | Wise sage beyond the walls — powerful but distant and costly |
| **Specialist skills** | Node roles and services | Baker (web), scholar (postgres), thinker (ollama) |
| **Teaching** | Binary deployment, config sync | Experienced nodes bring new ones up to speed |
| **Growing up** | Node provisioning lifecycle | Born (VM created) → named → housed (IP) → trained (services) → contributing |
| **Getting sick** | Node failure, service crash | Noticed by neighbours, work redistributed, recovery attempted |
| **Aging** | Memory accumulation, version drift | Older nodes have richer memories, may need updates |

### Life Events

| Life event | Mesh operation | What happens |
|---|---|---|
| **Birth** | New node provisioned | VM/CT created, cosmix installed, PG initialised |
| **Naming** | Identity assignment | WG keypair generated, node name chosen |
| **Moving in** | Mesh registration | Gets mesh IP from IPAM, added to `mesh_nodes`, peers configured |
| **First day at work** | Service startup | Starts running assigned services (web, mail, ollama, etc.) |
| **Making friends** | Peer discovery | Establishes WG tunnels to neighbours, joins replication |
| **Learning** | Memory accumulation | PG fills with data, pgvector builds embeddings, journals grow |
| **Thinking** | LLM reasoning | Node reasons about its own state, plans actions, understands language |
| **Asking for help** | Mesh call to thinker | Node sends a question to a smarter peer for harder problems |
| **Consulting the Oracle** | Frontier API call | Node escalates to external intelligence for problems beyond local capacity |
| **Getting sick** | Partial failure | A service crashes, heartbeat still going, neighbours notice |
| **Recovering** | Self-healing | Routine detects failure, restarts service, notifies village |
| **Serious illness** | Node unreachable | Heartbeat stops, `last_seen` grows stale, village redistributes work |
| **Death** | Decommission | WG key revoked, IP released to IPAM pool, records archived |
| **Inheritance** | Data preserved | PG replication means the village still has their knowledge |
| **Citizenship** | Accepting the constitution | New node receives CONSTITUTION.md, agrees by joining |
| **Voting** | Participating in governance | Node signs proposals/votes with WG key, one node one vote |
| **Lawmaking** | Proposing new policies | Node submits AMP proposal, village deliberates and votes |

## Design Heuristics

When making architectural decisions, ask these questions:

### "Where does this live in the body?"

- If it's knowledge or state → **brain memory** (PostgreSQL)
- If it requires reasoning, judgment, or comprehension → **brain cognition** (LLM)
- If it's coordination or signaling → **nervous system** (daemon IPC/mesh)
- If it's an external interface → **sense organ** (port service)
- If it's an automatic response → **reflex** (routine/trigger)
- If it's protection → **immune system** (safety module)
- If it's a shared principle or collective agreement → **conscience** (constitution/legislation)

If the answer is "nowhere" or "it would be a third arm," the design is wrong.

### "Is this personal or communal?"

- Personal (clipboard contents, local agent jobs) → daemon state or local PG
- Communal (node registry, service discovery) → replicated PG tables
- Gossip (peer up/down events) → mesh broadcast

### "What happens when this person gets sick?"

Every feature must have an answer to: "What if the node running this goes down?"

- If the answer is "nothing, others have it too" → you've designed resilience
- If the answer is "everything breaks" → you've created a single point of failure; redesign

### "Could a newcomer figure this out?"

If a new node (or a new developer) can't understand where a feature fits by asking "what body part is this?" — the feature is misplaced or the architecture has drifted.

## Zoom Levels — Village, Organism, Cell

The organic architecture works at three nested zoom levels. Use whichever level fits your audience and your engineering problem.

```
┌─────────────────────────────────────────────────────────┐
│  ZOOM OUT: The mesh is a VILLAGE                        │
│  Audience: anyone, including children                   │
│  Thinking: social — cooperation, roles, communication   │
│                                                         │
│  ┌──────────────────────────────────────────────────┐   │
│  │  MIDDLE: Each node is a PERSON                   │   │
│  │  Audience: anyone with basic anatomy knowledge   │   │
│  │  Thinking: body — brain, senses, immune system   │   │
│  │                                                  │   │
│  │  ┌───────────────────────────────────────────┐   │   │
│  │  │  ZOOM IN: Each node is a CELL             │   │   │
│  │  │  Audience: engineers, biology students     │   │   │
│  │  │  Thinking: cellular — nucleus, membrane,   │   │   │
│  │  │  organelles, chemical signaling            │   │   │
│  │  └───────────────────────────────────────────┘   │   │
│  └──────────────────────────────────────────────────┘   │
│                                                         │
│  Together: a GROVE of trees sharing a root network      │
└─────────────────────────────────────────────────────────┘
```

**The person/village level is primary.** A child can understand "this computer is a person in a village with a brain and eyes and ears." The cellular level exists for when you need engineering precision — when you're designing internal node structure or reasoning about the mesh as a unified system rather than a collection of individuals.

### Why Plant, Not Animal

The mesh is more plant than animal:

| Property | Plant | Animal | Mesh |
|---|---|---|---|
| Central brain | **No** | Yes | **No** (peer-to-peer) |
| Can lose parts and survive | Yes (prune a branch) | Barely | Yes (lose a node) |
| Can clone from a cutting | **Yes** | No | **Yes** (backup → new node) |
| Moves physically | No | Yes | **No** (servers stay put) |
| Grows toward resources | Yes (phototropism) | Hunts | Yes (scale toward demand) |
| Every cell has full DNA | **Yes** | Yes | **Yes** (full binary + schema on every node) |
| Communication | Chemical signals, no nerves | Nervous system | Mesh protocol, no central router |

The plant model resolves a tension in the village analogy. In the village, PostgreSQL is the "brain" — but a village has no collective brain. Plants don't have brains at all. Each cell has its own **nucleus** (PostgreSQL), and coordination is distributed through chemical signaling (mesh protocol). This maps more honestly to the peer-to-peer reality.

### Cellular Structure Mapping

| Cell structure | Node equivalent | Function |
|---|---|---|
| **Nucleus** | PostgreSQL | Contains full DNA (schema), directs all cell activity |
| **Nucleolus** | LLM (local or frontier) | Protein synthesis control — the part of the nucleus that enables complex assembly (reasoning) |
| **DNA** | cosmix binary + PG schema | Blueprint — identical in every cell |
| **Cell membrane** | WireGuard encryption | Selective permeability — controls what enters and exits |
| **Cell wall** (plant-specific) | Firewall rules | Rigid outer protection |
| **Ribosomes** | Lua engine | Translates instructions (scripts) into action (effects) |
| **Mitochondria** | CPU / tokio runtime | Energy production — powers everything |
| **Chloroplasts** | Web-facing services (:443, :25) | Converts external input (requests) into usable energy (data) |
| **Cytoplasm** | RAM / daemon state | The medium everything floats in |
| **Endoplasmic reticulum** | IPC / message queues | Internal transport network |
| **Vesicles** | AMP messages | Packages of information moving between structures |
| **Epigenetic markers** | CONSTITUTION.md + mesh_policies | Heritable regulatory information — not in the DNA itself, but governs how the DNA is expressed |

### Organism-Level Mapping

| Plant structure | Mesh equivalent |
|---|---|
| **Vascular system** (xylem/phloem) | WireGuard tunnels — data transport between cells |
| **Roots** | External network interfaces — absorbing from the outside world |
| **Leaves** | Web services — photosynthesis (turning requests into useful work) |
| **Bark** | TLS + perimeter security |
| **Growth rings** | Journal entries, version history |
| **Seeds** | VM/CT templates — dormant copies that can grow into new plants |
| **Grafting** | Node migration (different rootstock + same scion = different hardware, same cosmix binary) |
| **Mycorrhizal network** | PG replication — the underground knowledge-sharing web |
| **Symbiotic bacteria** | Frontier API — external intelligence the organism depends on but doesn't contain |
| **Hormones** | LLM-driven decisions — slow, considered signals that change system-wide behaviour |
| **Epigenetic regulation** | Constitution + policies — heritable rules that govern how cells express their shared DNA |

The mycorrhizal network is the standout mapping. In a forest, trees share nutrients and chemical warnings through interconnected fungal roots (the "wood wide web"). This is exactly what PG logical replication does — underground, invisible, every node benefits from what any node learns.

### Zoom Level 4: Galaxy (Specialist Use Only)

When the mesh spans multiple sites with real network distance between them, a cosmic-scale analogy becomes useful — but only for specific problems.

**What it clarifies:**

- **Latency is structural.** In a village everyone's close; in a cell signals are instantaneous. But in a solar system, light takes minutes to reach the outer planets. This is the only zoom level that treats distance as a first-class design constraint — and the mesh *does* span datacenters with real round-trip cost.
- **Hostile vacuum.** The village has paths; the plant has soil. Space has *nothing* — the void is actively hostile. The public internet between mesh sites is vacuum. WireGuard isn't a road or a root system — it's a pressurised tunnel through the void. Nothing survives outside it unencrypted.
- **Topology emerges from gravity.** Hub nodes attract connections not because someone planned it, but because they're heavier (more services, more data, more peers). Nodes naturally orbit the most massive peers. This is how mesh topology actually evolves in practice.
- **The Oracle is a distant star.** Frontier API calls cross the hostile vacuum of the public internet to reach an enormously powerful but uncontrollable energy source. The light (response) takes time to arrive, costs energy to request, and the star doesn't care about you specifically. Local LLMs are your own fusion reactor — smaller, but yours.

**Why it's not the primary model:** Planets don't communicate, cooperate, or heal. Stars don't share knowledge. The cosmic scale implies vastness that 8 nodes don't warrant, and offers no useful model for the things that matter most — inter-node messaging, self-healing, growth. It's a lens for latency-aware design and hostile-network reasoning, not for everyday development.

### Which Zoom Level When

| You're doing... | Use this level | Think like... |
|---|---|---|
| Explaining cosmix to a non-technical person | **Village** | "Your computer is a person in a village" |
| Designing node internals (what runs where) | Person/Body | "Where does this live in the body?" |
| Engineering mesh-level behaviour (replication, healing, scaling) | Organism/Cell | "How does this cell serve the whole organism?" |
| Multi-site latency, network hostility, topology planning | Galaxy | "How far apart are these planets? What's between them?" |
| Governance, policy, collective decisions | Village → Nation | "What does the village agree to? How do we decide?" |
| Fleet management (monitoring, provisioning) | Both | Village for UX ("who's sick?"), cellular for implementation ("tissue repair") |

**Default to person/village.** It's the most accurate *and* the most approachable — a child understands it, an engineer can work from it, and it covers 90% of design decisions. Reach for the other levels only when the village model doesn't give you enough precision for the specific problem at hand.

### What Changes at the Cellular Level

The village model asks: "How does this node serve itself?" → individual agency.
The organism model asks: "How does this cell serve the whole?" → coordinated function.

| Concept | Village framing | Organism framing |
|---|---|---|
| Health monitoring | A doctor visiting each person | Immune response — automatic, distributed |
| Autoprovisioning | A newcomer arriving in town | Cell division — the organism grows |
| Node failure | A funeral, work redistributed | Tissue damage + healing |
| Scaling | Recruiting new villagers | Growth toward resources (phototropism) |
| PG replication | The shared library | Mycorrhizal network — underground nutrient exchange |
| Local LLM | Personal intelligence — each person can think | Nucleolus — the organelle that enables complex protein assembly |
| Frontier API | The Oracle beyond the walls | Symbiotic bacteria — external intelligence the organism depends on but doesn't own |
| Escalation chain | Think → ask a friend → consult the Oracle | Reflex → enzyme → hormone → symbiont |
| Constitution | Village charter — principles carved in the town square | Epigenetic markers — heritable rules governing gene expression |
| Legislation | Laws passed at the village meeting | Hormone regulation — system-wide signals that change behaviour |
| Governance | Direct democracy — every villager votes | Chemical consensus — cells coordinate through shared signaling |

Both framings are correct. The village is warmer, more intuitive. The organism is more precise about what actually happens at the systems level.

## Where the Analogy Does NOT Apply

These biological patterns should **not** be imitated:

| Biological pattern | Why not |
|---|---|
| **Random mutation for evolution** | We design intentionally, not randomly. Don't randomise configs hoping for improvement. |
| **Pain as signal** | Logging and alerts exist. Don't build suffering into the system. |
| **Aging as decline** | Nodes should not degrade over time. If they do, it's a bug, not a feature. |
| **Reproduction** | Nodes don't spawn copies of themselves. Provisioning is deliberate, not automatic (for now). |
| **Consciousness** | The mesh will not become aware. Don't design for emergent intelligence. |
| **Hierarchy by dominance** | No node is "boss." The village elder coordinates but cannot compel. Mesh topology is peer-to-peer. |
| **Biological inefficiency** | Human bodies waste 60% of caloric intake as heat. Don't copy nature's wastefulness. |

**The hard rule:** If the analogy suggests a design you'd also reach from pure engineering principles, follow it. If it suggests something you'd only do "because biology does it," stop and think.

## Node Roles as Village Professions

| Role | Profession | What they do | Example nodes |
|---|---|---|---|
| `desktop` | **Artist** | Creates, displays, interacts with the human directly. Has all senses. | cachyos |
| `server` | **Trader** | Serves the outside world — web, mail, APIs. Public-facing. | mko, mmc |
| `server` | **Scholar** | Maintains the library — primary PG, embeddings, memory. | gcwg |
| `worker` | **Thinker** | Runs inference, crunches numbers. Speaks when spoken to. | ollama2, ollama3 |
| `template` | **Apprentice** | Learning the trade — test node, staging, experiments. | mesh3 |
| `hub` | **Elder** | Coordinates newcomer registration, IP allocation. Peer among peers. | gcwg (primary), mko (failover) |

## The Core Invariant

**Every node must be able to survive alone.**

If the village burns down, any single surviving person can rebuild it. They have the full DNA (cosmix binary), a brain (PostgreSQL with the complete schema), the knowledge of how the village was organised (replicated mesh_nodes, mesh_services, mesh_ipam tables), and the ability to think (local LLM, even a small one).

This has a cognitive dimension: a node without network can still reason (local LLM). A node without GPU reasons slowly but still reasons (CPU inference). A node without *any* LLM is a sleepwalker — functional but not intelligent. This suggests that **a minimal local model on every node** is as important as PostgreSQL on every node. The brain needs both memory and reasoning to be a brain.

This is the test for every architectural decision: does it preserve individual autonomy, or does it create a dependency that makes nodes helpless without the collective?

## Why Not Kubernetes

Kubernetes is the industry standard for container orchestration. It's worth evaluating explicitly because the question will come up. The answer: K8s is not overkill for cosmix — it's the **wrong model**.

### The Core Conflict

| Principle | Kubernetes | Cosmix |
|---|---|---|
| **Unit of identity** | The cluster | The node |
| **Workloads are...** | Cattle (disposable, fungible, centrally scheduled) | People (named individuals with memory, personality, roles) |
| **State lives...** | Externally (etcd, external PG, S3) | On the node — PG is the brain, it *stays* |
| **Scheduling** | Central control plane decides where things run | Each node knows its own role — mko IS the trader |
| **Node failure** | Reschedule pods elsewhere, node is replaceable | Village notices someone is sick, helps them recover |
| **Networking** | Overlay network (Calico, Cilium, flannel) | WireGuard mesh — point-to-point, identity-based |
| **Service discovery** | DNS + etcd (centralised) | PG replication (distributed, no single point of failure) |
| **Philosophy** | "How do I spread workloads across anonymous machines?" | "How does each named individual contribute to the village?" |

K8s treats nodes as a pool of anonymous compute that a central brain (the control plane) schedules work onto. Cosmix treats each node as a named individual with a brain of its own. These are fundamentally opposite design choices.

### What K8s Solves — And How Cosmix Handles It Differently

| K8s feature | Cosmix equivalent | Why cosmix's approach fits better |
|---|---|---|
| Service discovery | `mesh_services` PG table, replicated | No central etcd — every node survives alone |
| Health checks + restart | Daemon routines, mesh heartbeat | Per-node self-healing, not central rescheduling |
| Rolling deployments | Binary deployment via rsync/scripts | Each node is known — you deploy to *them*, not to "the cluster" |
| Secret management | PG (encrypted), config.toml | The brain stores its own secrets |
| Load balancing | Mesh routing, node roles | Nodes have professions, not random work assignments |
| Network policy | WireGuard + firewall rules | Identity-based (WG keys), not label-based |
| Config management | config.toml + PG mesh_policies | Local bylaws + shared legislation |
| Autoscaling | Autoprovisioning (birth of new nodes) | Deliberate, not automatic — new villagers, not spawned pods |

### Patterns Worth Stealing (Without Using K8s)

K8s has solved real problems at scale. Three patterns are worth adopting natively:

1. **Declarative desired state** — "the mesh should have at least 2 web servers" as a policy in `mesh_policies`, and autoprovisioning works toward it. The system converges toward the declared state.
2. **Readiness vs liveness** — a node can be alive (heartbeat present) but not ready (PG still syncing, services not yet started). The mesh should distinguish "breathing" from "ready to contribute."
3. **Rolling updates with rollback** — deploy a new binary to one node, verify health, continue or rollback. Worth implementing in the deployment routines.

These are good engineering ideas. You don't need K8s to use them.

### The Right Infrastructure Layer

```
K8s world:        Cluster → Pods (ephemeral, scheduled, stateless)
                  The cluster is the organism, pods are disposable cells.

Cosmix world:     Proxmox/Incus → VMs/CTs (persistent, named, stateful)
                  Each VM/CT IS the individual. Cosmix gives it a brain.
```

Proxmox and Incus create the *bodies* that cosmix inhabits — the maternity ward where new villagers are born and given physical resources. Cosmix then provides the brain (PG), nervous system (daemon), and identity (WG key). K8s would try to be the nervous system and brain, conflicting with cosmix doing that job.

| Tool | Relationship to cosmix |
|---|---|
| **Proxmox** | Maternity ward — creates the bodies (VMs) that nodes inhabit |
| **Incus** | Same, lighter weight (CTs instead of full VMs) |
| **Kubernetes** | Wrong model — treats nodes as cattle, cosmix treats them as people |
| **cosmix-daemon** | IS the orchestration layer (nervous system + brain) |
| **PostgreSQL** | IS the state management layer (memory + knowledge) |
| **WireGuard** | IS the network layer (roads between houses) |

### The One Scenario Where K8s Might Appear

If cosmix ever offers **hosted multi-tenant service** — running hundreds of isolated cosmix meshes for different customers — K8s could manage the infrastructure underneath. Each tenant gets a namespace, each cosmix node is a StatefulSet with persistent volumes. But that's K8s managing the *hosting platform*, not the mesh. The mesh is still peer-to-peer internally, each node still has its own brain.

That's a far-future enterprise scenario, not relevant today.

## Implementation Priorities (Derived from the Model)

1. **PostgreSQL on every node** — a person without memory isn't a person
2. **PG replication between nodes** — villagers who can't share knowledge aren't a village
3. **Mesh tables** (`mesh_nodes`, `mesh_services`, `mesh_ipam`) — the town registry that makes cooperation possible
4. **Local LLM on every node** — a person who can't think is a sleepwalker (even a small model counts)
5. **LLM escalation chain** — think locally → ask a thinker peer → consult the Oracle
6. **Health monitoring** — villagers who notice when a neighbour is sick
7. **Autoprovisioning** — a welcoming process for newcomers that doesn't require the elder to do everything manually
8. **Fleet management** — the village meeting where everyone reports their status

These are ordered by dependency, not priority. Each requires the one before it.
