---
title: Package Installation — Retained NS4 Profile
chapter: 10c
version: 0.1.1
spec11_version: 1.0.0-rc.1
status: draft
date: 2026-09-05
---

# Package Installation — Retained NS4 Profile

**DAEMON-PACKAGE-001 — Retained NS4 installer profile.** The following sections preserve the NS4 package, trust-envelope, manifest, phase ordering, abort/rollback, identity and conformance requirements. Legacy section numbers are local to this profile. Legacy chapter 11 does not acquire a new wire/distribution ID through this editorial filename.

This is retained intended detail, not the current universal installation workflow, a GA release, or a deployment attestation. The current public entry points are `bootstrap` and `setup.mix`; see [composition](02-composition.md) and [daemon operation](10-daemon-agent-operation.md). The named NS4 installer and its tests were not executed or established as delivered in this audit. Dated distribution/libc values, pending/present labels, timing claims and sample rollout statements below remain historical, not current environment facts.

The [identity profile](10a-daemon-identity-profile.md) retains the referenced identity/verification rules. [Repair and improvement](10b-repair-improvement-profile.md) retains the recovery intent. [Mesh trust](08-mesh-trust.md) describes the present trust boundary. The suite's [authority rules](00-authority.md) apply: publication neither resolves conflicts by fiat nor grants deployment permission.

Known conflicts are retained for explicit disposition: the legacy tarball manifest still names `usr/local/bin`, while current production binaries use `/opt/cosmix/bin`; the exact staged-installer lifecycle differs from current bootstrap; distro and runtime prerequisites need fresh fixtures. These discrepancies must be resolved before claiming this installer profile conformant. They are not instructions to introduce parallel installs or execute old examples.

For public readability, former private source paths are represented as `deployment-tools/` and `deployment-config/` logical artifact roots. They identify helper/template names, not verified public filesystem paths. Private operational references are descriptive historical labels and host examples use placeholder identities. Preserve all required manifest checks, phase boundaries, authority and rollback conditions when adapting the profile to an implemented installer; convenience is not a waiver.

## 1. Introduction

**Version mapping.** Frontmatter `version` tracks this editorial chapter only.
`spec11_version: 1.0.0-rc.1` preserves the legacy NS4 contract version. All retained
references to the SPEC 11 version or a contract-version bump mean this separate
field, including MANIFEST generation, compatibility/downgrade checks and L12.
Editorial corrections must not change the version consumed by installers.

### 1.1 Purpose

This SPEC defines:

1. The package set NS 4.0 ships (§2).
2. The tarball layout, naming, versioning, and integrity envelope (§3).
3. Host preconditions on Debian 13 — kernel, systemd, glibc, WireGuard,
   hostname, `/etc/hosts`, sysadm — that the installer assumes (§4).
4. The on-host installation procedure that drives SPEC 10 §6's three
   verification phases through to a running, registered node (§5).
5. The Mix installer contract — the canonical script that performs
   §5 — and the Mix-first / never-Python rule it embodies (§6).
6. Upgrade, downgrade, rollback, and removal semantics (§7).
7. The sysadm boundary — what NS 4.0 expects of UID 1000, what it
   does not configure, and how the substrate daemons coexist with
   the operator account (§8).
8. The NS 3.0 vhost coexistence rule — UIDs ≥ 1001 are out of scope
   and SHALL NOT collide with the substrate registry (§9).
9. Conformance levels and the CI lint shape that prevent drift (§10,
   §11).

### 1.2 Scope

This SPEC applies to:

- Mesh nodes running Debian 13 (Trixie) as a host or in the live
  systemd-nspawn/`cosmix-nspawnd` lane (or an externally managed Proxmox CT).
- The cosmix-* daemon set as registered in SPEC 10 Appendix A.
- The base mesh-citizen package (`cosmix-base`, §2.1) and the
  optional knowledge-tier add-on (`cosmix-indexd`, §2.2).

It does not apply to:

- **The build environment.** How the binaries are *produced* is
  governed by the retired infra/build-and-distribution doc (2026-07-23, git history) and is
  intentionally out of scope here. NS 4.0 governs *deployed*
  artifacts, not the workspace that produced them.
- **NetServa vhost provisioning.** Per-tenant vhost users (UID
  ≥ 1001, NS 3.0 convention) are governed by NetServa, not by
  this SPEC. NS 4.0 only asserts non-collision (§9).
- **Session-scoped components.** `cosmix-comp`, CTK, and native desktop apps
  components excluded from the SPEC 10 registry per its §7 are
  outside NS 4.0's substrate-daemon scope and SHALL NOT appear in
  NS 4.0 packages.
- **Distributions other than Debian 13.** The tarball shape is
  reusable, but conformance is defined against Debian 13 only at
  this version. Other distributions are outside this profile;
  development workspaces do not imply NS 4.0 conformance.

### 1.3 Non-goals

- **A native `.deb` package.** NS 4.0 v1.0.0 ships as a tarball plus
  a Mix installer (§3, §6), matching the existing example-node deployment.
  A future amendment MAY define a `.deb` shape with the same
  install ordering. The tarball shape is normative now; `.deb` is a
  pre-commitment, not a current requirement.
- **An operating-system distribution.** NS 4.0 is a package install
  on top of an existing Debian 13 host. The host is provisioned by
  Debian itself (or by systemd-nspawn / Proxmox / cloud-init); NS 4.0 does
  not bootstrap the host.
- **Cross-distro abstraction.** NS 4.0 v1.0.0 names paths, package
  managers, and service-manager conventions specific to Debian 13.
  Cross-distro abstraction layers (e.g. distro-detection in the
  installer) are explicitly out of scope until a second target is
  declared.
- **Auto-correction of preflight failures.** Per SPEC 10 §6.1 and
  §6.2, NS 4.0 fails closed on any registry conflict and SHALL
  NOT silently work around it. The installer's response to "UID
  500 is held by another package" is to halt with a structured
  diagnostic, not to renumber.

### 1.4 Terminology

| Term | Definition |
|------|------------|
| **NS 4.0** | This specification's revision of the NetServa package install convention. |
| **NS 3.0** | The pre-existing daily-driver and vhost convention, continued for sysadm UID 1000 and vhost UIDs ≥ 1001. |
| **mesh node** | A Debian 13 host (bare metal, VM, or container) that has been admitted to the WireGuard /24 trust domain (roster: the `08-mesh-trust.md` signed inventory; narrative doc retired 2026-07-23, git history) and runs `cosmix-noded`. |
| **package** | A versioned NS 4.0 tarball (`cosmix-base`, `cosmix-indexd`) plus its embedded installer. |
| **package set** | The collection of packages installed on a node: always `cosmix-base`, optionally `cosmix-indexd`. |
| **tarball** | The on-the-wire artifact (`cosmix-<package>-<version>-<arch>.tar.zst`) shipped to a host, plus its detached integrity envelope (§3.4). |
| **installer** | The Mix script `ns4-install.mix` (§6) that drives §5's procedure on the host. |
| **sysadm** | The administrative human user, UID 1000, NS 3.0 convention, configured by the operator before NS 4.0 runs. |
| **base package** | `cosmix-base` — `cosmix-noded` + `cosmix-maild` + `cosmix-webd` + Mix runtime + canonical fragments (§2.1). |
| **knowledge add-on** | `cosmix-indexd` — optional vector / knowledge index daemon, separate package (§2.2). |
| **roles** | The set of cosmix-* daemons enabled on a node. NS 4.0 v1.0.0 defines two role packs: `mesh-citizen` (base) and `knowledge` (base + indexd). |
| **operator** | The human (or agent) running the installer, addressing the host as sysadm. |

### 1.5 Why NS 4.0 (relative to NS 3.0)

NS 3.0 was a daily-driver provisioning convention: it bootstrapped a
COSMIC desktop inside an Alpine container with a sysadm operator and
a `cosmix` desktop user, and it allocated UID 1001+ for vhost users.
That convention is still authoritative for the desktop and vhost
planes (`historical daily-driver setup guide (2026-03-24)`, deferred to a future
NetServa-vhost SPEC).

NS 4.0 covers a different surface: how a Cosmix substrate daemon set
lands on a server-class Debian 13 host as a *mesh citizen*, with
SPEC 10's identity contract enforced at install time. The two
conventions coexist on a single host: sysadm UID 1000 from NS 3.0,
substrate daemons UID 500–507 from SPEC 10 / NS 4.0, vhost users UID
≥ 1001 from NS 3.0. NS 4.0 does not replace NS 3.0 — it slots the
substrate plane into the same overall numbering scheme.

---

## 2. Package Set

NS 4.0 v1.0.0 defines two packages. A node's role determines which
packages are installed.

### 2.1 `cosmix-base` (REQUIRED)

The base mesh-citizen package. Every NS 4.0 mesh node SHALL install
`cosmix-base`. It contains:

| Component | Path on disk | Source / SPEC reference |
|-----------|--------------|--------------------------|
| `cosmix-noded` binary | `/opt/cosmix/bin/cosmix-noded` | SPEC 10 UID 500, ABP service `noded` |
| `cosmix-maild` binary | `/opt/cosmix/bin/cosmix-maild` | SPEC 10 UID 501 |
| `cosmix-webd` binary | `/opt/cosmix/bin/cosmix-webd` | SPEC 10 UID 502 |
| `mix` binary | `/opt/cosmix/bin/mix` | Mix runtime — required by the installer and by every cosmix-* daemon's lifecycle hooks |
| sysusers fragment | `/usr/lib/sysusers.d/cosmix.conf` | SPEC 10 §4, Appendix A — generated from canonical Markdown |
| tmpfiles fragment | `/usr/lib/tmpfiles.d/cosmix.conf` | SPEC 10 §3.3 — secret-config directories only |
| systemd units | `/usr/lib/systemd/system/cosmix-{noded,maild,webd}.service` | SPEC 10 §5, §9.2 — Level 2 declared |
| host snippet | `/usr/share/cosmix/etc/hosts-snippet.txt` | Mesh `/etc/hosts` block, operator-applied |
| installer script | `/usr/share/cosmix/install/ns4-install.mix` | §6 — the canonical Mix installer |
| audit scripts | `/usr/share/cosmix/install/spec10_{preflight,postcheck,audit}.{mix,sh}` | SPEC 10 §6.1–§6.3 verification phases |
| LICENSE + README | `/usr/share/cosmix/LICENSE`, `/usr/share/cosmix/README.md` | Provenance and orientation |

The base package SHALL NOT include any cosmix-* binary outside the
three named here. SPEC 10 registers `cosmix-agentd`, `cosmix-mcp`,
and `cosmix-cron` (UIDs 504, 505, 507) but their binaries
are not in scope for NS 4.0 v1.0.0; future amendments will add them
as the daemons mature, either as members of `cosmix-base` or as
separate add-ons.

**Pending materialisation.** As of v1.0.0-rc.1, the source tree
contains canonical units only for `cosmix-noded.service` and
`cosmix-maild.service` (`deployment-config/systemd/`); the `cosmix-webd`
binary and `cosmix-webd.service` unit are normatively in scope for
this SPEC but not yet committed. A `cosmix-base` v1.0.0 build
artifact SHALL NOT be cut until the webd unit and binary land in
the source tree and pass the §11 CI lints (the SPEC frontmatter
also bumps from `1.0.0-rc.N` to `1.0.0` at that point per
Appendix D). The first NS 4.0 deployment that ships only
noded+maild SHALL be tagged as a v0.x preview release, not as
v1.0.0 conformant.

### 2.2 `cosmix-indexd` (OPTIONAL)

The knowledge tier add-on. Mesh nodes designated as knowledge-bearing
SHALL install `cosmix-indexd` *in addition to* `cosmix-base`. It
contains:

| Component | Path on disk | Source / SPEC reference |
|-----------|--------------|--------------------------|
| `cosmix-indexd` binary | `/opt/cosmix/bin/cosmix-indexd` | SPEC 10 UID 503 |
| systemd unit | `/usr/lib/systemd/system/cosmix-indexd.service` | SPEC 10 §5, Level 2 declared |
| Per-daemon read-only data (e.g. embedding model assets) | `/usr/share/cosmix/indexd/` | Daemon-specific |

`cosmix-indexd` is a strict add-on: it depends on `cosmix-base`'s
sysusers fragment and on `cosmix-noded` being up at start time
(SPEC 10 §5.4). The installer SHALL refuse to install
`cosmix-indexd` on a host where `cosmix-base` is **not installed
and is not being installed in the same run**. The single-invocation
`--role knowledge` form (§6.2) installs base then indexd in one
run and satisfies the dependency by ordering, not by pre-existence.

### 2.3 Non-packaged components

NS 4.0 v1.0.0 does **not** package:

- `cosmix-agentd`, `cosmix-mcp`, `cosmix-cron` —
  registered in SPEC 10 but not yet shipped as mesh-node services.
- `cosmix-comp`, CTK, and native desktop applications — session-scoped or
  workspace-only; covered by NS 3.0 daily-driver conventions where
  applicable.
- The Cosmix source tree, build toolchain, private operational documents, specifications,
  and journals — the source of truth lives in the workspace, not on
  mesh nodes.
- A cargo or rustup toolchain — mesh nodes consume binaries; they
  do not build.

A future amendment SHALL extend §2.1 / §2.2 / §2.3 as additional
daemons mature and need a packaged form.

### 2.4 Role packs

NS 4.0 v1.0.0 defines two role packs. A node's role determines which
packages are installed.

| Role pack | Packages | Typical mesh node |
|-----------|----------|-------------------|
| `mesh-citizen` | `cosmix-base` | Default for every NS 4.0 mesh node. Provides per-node broker + mail + web. |
| `knowledge` | `cosmix-base` + `cosmix-indexd` | A node designated as a knowledge-tier participant per the planned distributed-indexd architecture. |

Role packs are an installer convenience, not a distinct artifact.
The installer takes a `--role <pack>` argument (§6.2) and installs
the corresponding tarballs.

---

## 3. Tarball

### 3.1 Naming

A tarball SHALL be named:

```
cosmix-<package>-<version>-<arch>.tar.zst
```

Where:

- `<package>` is one of `base`, `indexd` (matching §2 minus the
  `cosmix-` prefix).
- `<version>` is the package's semver (e.g. `1.0.0`,
  `1.2.3-rc1+gabcdef0`). The version SHALL match the embedded
  `MANIFEST` (§3.3).
- `<arch>` is the Debian architecture identifier — `amd64`,
  `arm64`. NS 4.0 v1.0.0 ships `amd64` only; `arm64` is a
  pre-commitment and SHALL be added by amendment when a build is
  produced.

The compression format SHALL be zstd. Other formats (gzip, xz) MAY
be added by amendment but are not normative at v1.0.0.

### 3.2 On-disk layout

A tarball SHALL extract to a single top-level directory matching
its base name (without `.tar.zst`). The internal layout SHALL mirror
the install paths defined in §2:

```
cosmix-base-1.0.0-amd64/
├── MANIFEST
├── usr/
│   ├── local/
│   │   └── bin/
│   │       ├── cosmix-noded
│   │       ├── cosmix-maild
│   │       ├── cosmix-webd
│   │       └── mix
│   ├── lib/
│   │   ├── sysusers.d/
│   │   │   └── cosmix.conf
│   │   ├── tmpfiles.d/
│   │   │   └── cosmix.conf
│   │   └── systemd/
│   │       └── system/
│   │           ├── cosmix-noded.service
│   │           ├── cosmix-maild.service
│   │           └── cosmix-webd.service
│   └── share/
│       └── cosmix/
│           ├── LICENSE
│           ├── README.md
│           ├── etc/
│           │   └── hosts-snippet.txt
│           └── install/
│               ├── ns4-install.mix
│               ├── spec10_preflight.mix
│               ├── spec10_postcheck.mix
│               └── spec10_audit.sh
```

`cosmix-indexd` follows the same structure with only the binary,
unit, and per-daemon assets it owns.

The installer (§6) SHALL refuse a tarball whose top-level layout
does not match this shape exactly. Loose or extra files SHALL cause
fail-closed.

### 3.3 MANIFEST

Every tarball SHALL contain a top-level `MANIFEST` file. The
MANIFEST is the canonical inventory of the tarball; the installer
verifies the tarball against it before any file is moved into a
live path.

The MANIFEST SHALL be a UTF-8 text file with the following
frontmatter-and-body shape (matching the ABP spec convention so a
MANIFEST is a valid ABP message body):

```
---
package: cosmix-base
version: 1.0.0
arch: amd64
target_distro: debian-13
target_glibc_max: 2.41
spec10_registry_version: 1.0.0
spec11_version: 1.0.0-rc.1
build_date: 2026-05-09T08:00:00Z
build_host_id: <opaque>
---
# files: <relative path> <mode> <sha256>
usr/local/bin/cosmix-noded 0755 <hex>
usr/local/bin/cosmix-maild 0755 <hex>
usr/local/bin/cosmix-webd  0755 <hex>
usr/local/bin/mix          0755 <hex>
usr/lib/sysusers.d/cosmix.conf 0644 <hex>
usr/lib/tmpfiles.d/cosmix.conf 0644 <hex>
usr/lib/systemd/system/cosmix-noded.service 0644 <hex>
...
```

Required frontmatter fields:

- `package` — `cosmix-base` or `cosmix-indexd`.
- `version` — semver string matching the tarball name.
- `arch` — Debian architecture matching the tarball name.
- `target_distro` — `debian-13` for v1.0.0. Other values SHALL be
  rejected by the installer at v1.0.0.
- `target_glibc_max` — the **highest** GLIBC symbol version any
  binary in the tarball links against. The installer compares this
  against the host's `ldd --version` output and fails closed on
  insufficient host glibc (§4.3, §5.1, and the
  `feedback_glibc_skew_local_to_container` memory).
- `spec10_registry_version` — the SPEC 10 Appendix A registry
  version against which the embedded sysusers fragment was
  generated. The installer cross-checks this against the on-host
  sysusers fragment, if any (§5.5).
- `spec11_version` — this SPEC's version. Read by the installer
  for compatibility decisions.
- `build_date` — RFC 3339 UTC.
- `build_host_id` — opaque identifier of the build host (build
  reproducibility metadata).

The body SHALL list every file in the tarball, one per line, in
the form `<relpath> <octal-mode> <sha256-hex>`. The installer
SHALL verify each file's mode and digest before any file is
extracted into a live path.

The MANIFEST SHALL NOT list itself.

### 3.4 Integrity envelope

Each tarball SHALL be accompanied by a detached integrity envelope:

```
cosmix-<package>-<version>-<arch>.tar.zst
cosmix-<package>-<version>-<arch>.tar.zst.sig
```

The envelope SHALL be a minisign signature (Ed25519, the format used
by `minisign` / `rsign2`). The signing key is operator-managed and
distributed out of band; key management is out of scope for this
SPEC.

#### 3.4.1 Bootstrap trust path

The bootstrap chicken-and-egg — the in-tarball Mix installer
cannot itself be the trust root for the tarball it ships in —
SHALL be resolved by the operator's host-side toolchain, not by
the in-tarball installer:

1. **Envelope verification (host-trusted).** The operator SHALL
   verify `cosmix-<package>-<version>-<arch>.tar.zst.sig` against
   their pinned operator public key using a host-installed
   `minisign` (or `rsign2`) binary that came from the host's
   distribution package manager — i.e. **not** from the tarball
   being verified. On Debian 13 the canonical install is
   `apt-get install minisign`. The operator's pinned public key
   SHALL be stored at a host path the operator controls
   (typically `/etc/cosmix/keys/release.pub`) and SHALL exist
   before the tarball arrives on the host.
2. **Extract under a quarantine path.** Extraction occurs at
   Phase 1 (§5.2) under `/var/lib/cosmix/.staging/<package>-<version>/`
   (a root-owned `0700` directory, self-bootstrapped by the
   installer per §5.2 2a; the parent `/var/lib/cosmix/.staging`
   is created if absent). The path is private to the install
   and outside any path on the default `PATH`. Operators MAY
   override via `--staging-dir <path>`.
3. **Per-file digest verification.** The installer (now executing
   from the just-extracted tarball) re-verifies every file
   listed in the MANIFEST against its sha256, before any file
   is promoted to a live path. This is defence-in-depth — the
   envelope already proves the tarball was produced by the
   operator's signing key; per-file digest verification catches
   in-flight or on-disk corruption between envelope check and
   promotion.
4. **No file promotion before all verifications pass.** Phases
   3+ (sysusers, tmpfiles, binaries, units) SHALL NOT promote
   any file out of the quarantine path until both the envelope
   and every MANIFEST digest have verified clean.

Re-runs of the installer on an already-installed host MAY use
the previously installed `/opt/cosmix/bin/mix`, but the envelope
verification and per-file digest verification SHALL still happen
against the freshly delivered tarball — the trust chain restarts
on every install.

A future amendment MAY add transparency-log or reproducible-build
attestation; v1.0.0 requires only the host-trusted minisign
envelope plus the in-tarball MANIFEST and per-file digest
verification.

### 3.5 Versioning

Package versions are independent semver strings. `cosmix-base` and
`cosmix-indexd` MAY ship at different versions; the installer
SHALL accept any combination where:

- Both packages' `spec10_registry_version` match the host's
  installed sysusers fragment (or, if none yet installed,
  match each other).
- `cosmix-indexd.MANIFEST.spec11_version <=
  cosmix-base.MANIFEST.spec11_version` (the base package's NS 4.0
  level is the floor for the host).

Across major NS 4.0 versions (e.g. 2.0.0), a forward amendment
SHALL define the upgrade path.

---

## 4. Host Preconditions

NS 4.0 makes specific assumptions about the Debian 13 host. The
installer SHALL verify each precondition before any state is
mutated. Failures are fail-closed (no auto-fix, no silent fallback).

Preconditions split into two groups by phase:

- **Host-intrinsic** (§4.1, §4.2, §4.6, §4.7, §4.8) — verified at
  Phase 0 (§5.1), before the tarball is touched. These checks need
  no MANIFEST and no extracted artifact.
- **MANIFEST-derived** (§4.3, §4.4, §4.5) — verified at Phase 1
  (§5.2), after the tarball envelope (§3.4) and per-file digests
  (§3.3) have been verified. These checks compare host facts
  against fields or artifacts inside the verified tarball.

The split exists because the MANIFEST and the in-tarball snippets
are not trusted until envelope verification has succeeded. Reading
them at Phase 0 would mean trusting unverified data; the installer
SHALL NOT do this.

### 4.1 Distribution and kernel

| Property | Required value | Verification |
|----------|----------------|--------------|
| Distribution ID (`/etc/os-release` `ID=`) | `debian` | `grep` on `/etc/os-release` |
| Distribution version (`VERSION_ID=`) | `13` | Same |
| Kernel | Linux 6.x (Debian 13's Trixie kernel or backport) | `uname -r` |
| systemd | ≥ 256 (Debian 13 default) | `systemctl --version` |
| Architecture | matches tarball `arch` | `dpkg --print-architecture` |

Ubuntu, Alpine, and other distributions are NOT supported
targets at v1.0.0. The installer SHALL refuse to run on a host
whose `/etc/os-release` does not match.

### 4.2 systemd availability

The host SHALL be running systemd as PID 1, with the following
facilities available:

- `systemd-sysusers` — for SPEC 10 §4 sysusers application.
- `systemd-tmpfiles` — for SPEC 10 §3.3 secret-config and other
  fragments.
- `systemctl daemon-reload`, `systemctl enable`, `systemctl start`.

Containers without systemd as PID 1 (e.g. minimal Alpine CTs) are
NOT supported. The installer SHALL detect `init` as PID 1 and
refuse if it is not `systemd`.

### 4.3 glibc compatibility (MANIFEST-derived, Phase 1)

Per the `feedback_glibc_skew_local_to_container` memory: binaries
compiled on a host with a newer glibc cannot run on a host with an
older glibc. NS 4.0 v1.0.0 binaries are built on Debian 13
(glibc 2.41). The installer SHALL verify, **at Phase 1 after
MANIFEST validation**:

```
host_glibc >= MANIFEST.target_glibc_max
```

A host with older glibc SHALL fail-closed with a message
recommending either rebuilding the tarball on a host of equal or
older glibc, or upgrading the target host. The installer SHALL
NOT attempt static-fallback or LD_LIBRARY_PATH workarounds.

This check is intentionally not at Phase 0: `target_glibc_max`
lives in the in-tarball MANIFEST and SHALL NOT be consulted before
the envelope (§3.4) and per-file digests (§3.3) have been verified.

### 4.4 Bind policy and edge-node opt-in

NS 4.0 defines a **layered bind policy** for cosmix-* daemons.
Every daemon's listen sockets follow the same default-and-opt-in
shape; the older `feedback_wg_only_binding` memory's blanket "WG
only, never 0.0.0.0" claim is superseded by this section, which is
the new substrate-of-record.

#### 4.4.1 Default bind

A cosmix-* daemon's default listen address selection SHALL be:

1. If a WireGuard interface is present and holds a mesh-/24
   address, bind that address only.
2. Otherwise, bind loopback (`127.0.0.1` and `::1`) only.

A daemon SHALL NEVER bind `0.0.0.0` or `::` by default. The two
default cases above are the only legitimate fall-through paths.

#### 4.4.2 Edge-node opt-in

Within a single mesh, **at most one node** MAY be designated the
**edge node**. The edge node MAY additionally bind the host's
primary external IPv4 and IPv6 addresses, on a per-port allowlist,
to serve as the single ingress point for both:

- general internet ingress (incoming SMTP on 25, HTTPS on 443,
  optionally SSH on 22), and
- cross-mesh ingress from peer meshes that route to this node by
  public address rather than over WireGuard.

The conventional edge-port allowlist is `{22, 25, 443, 465, 8443}`
— matching the example-node deployment journal (§12.3 example). The
allowlist is a per-node configuration value, not a mesh-wide
constant; an edge node MAY enable a strict subset.

Edge-node designation SHALL be:

- An explicit per-host configuration value: `edge_node = true`
  plus `edge_ports = [...]` in `/etc/cosmix/site.toml` (§5.9).
  Per-daemon configs that bind public ports SHALL each bind a
  port that appears in `site.toml.edge_ports`.
- Verified by the installer at Phase 8 (§5.9): if any daemon's
  config binds a public address, `site.toml.edge_node` SHALL be
  `true` and every bound public port SHALL appear in
  `site.toml.edge_ports`. Otherwise the install fails-closed.

Mesh inventory (the `08-mesh-trust.md` signed inventory; the old narrative doc was retired 2026-07-23, git history) is
*operator references*, not a trusted on-host input — they live
in the documentation tree and are not part of the installed
footprint per §2.3. Authoritative on-host edge-node state lives
in `site.toml`.

Mesh-wide invariants — that **at most one** node holds edge-node
status per mesh — are not directly enforceable by a single-host
installer and are tracked as a substrate-self-aware concern
(SPEC 07). The single-host installer SHALL NOT silently allow
two edges, but cannot itself prove a peer is not also edge.

#### 4.4.3 Verifications at Phase 1

The installer SHALL verify, after MANIFEST-derived staging:

1. A WireGuard interface exists, OR the operator passed the
   explicit `--allow-loopback-only` flag (§6.2). Default behaviour
   without that flag is to fail-closed when WG is absent.
2. When WG is present, the interface holds an address inside the
   `mesh_cidr` declared in `/etc/cosmix/site.toml` (§5.9). NS 4.0
   does not consult any in-tree document or out-of-band inventory
   for this check; the `site.toml` value is the authoritative
   declaration on the host. Mismatch between the live WG address
   and `mesh_cidr` is a fail-closed condition.
3. Edge-node designation is internally consistent: if any daemon
   config asserts `edge_node = true`, the host's
   `/etc/cosmix/site.toml` SHALL also assert `edge_node = true`,
   and the bound public ports SHALL all be in the §4.4.2
   allowlist `{22, 25, 443, 465, 8443}`. The installer does NOT
   enforce mesh-wide single-edge uniqueness; that is an operator
   policy concern (§4.4.2).

The mesh inventory and WG configuration are out of scope for NS
4.0 — they are assumed to have been provisioned before the
installer runs (typically by a separate operator step or by the
host's cloud-init / systemd-nspawn profile). NS 4.0 verifies presence and
internal consistency on the *single host* it is running on, using
only host-local trusted inputs (`site.toml` + live network state);
it does not consult private operational documents, which is a documentation tree and not
part of the installed footprint per §2.3.

### 4.5 `/etc/hosts` mesh block

The host SHOULD carry the canonical mesh `/etc/hosts` block. The
canonical text lives at `deployment-config/hosts-snippet.txt` in the source
tree and is bundled into the tarball at
`usr/share/cosmix/etc/hosts-snippet.txt`. The installer SHALL
diff the live `/etc/hosts` against the staged-from-tarball copy
**at Phase 1**, after the tarball has been verified, and SHALL
warn (not fail) if they disagree — the operator may have
intentional local overrides.

This check is informational. It SHALL NOT gate any later phase.

### 4.6 sysadm operator account

The installer SHALL run as a non-root user in the `sudo` group, or
as `root` directly. The expected operator identity is `sysadm`
(NS 3.0 convention, UID 1000, in the `sudo` or `wheel` group). If
the operator is not `sysadm`, the installer SHALL emit a warning
but proceed; NS 4.0 does not pin the administrative UID.

The installer SHALL NOT run as a `cosmix-*` registry user. SPEC 10
fail-closed semantics require that registry users be created by
sysusers, not by the installer running as one of them.

### 4.7 Storage and filesystem

| Property | Required | Verification |
|----------|----------|--------------|
| Root filesystem writable | yes | `test -w /` |
| `/opt` exists, writable by root | yes | `test -d /opt && test -w /opt` |
| `/var/lib` exists, writable by root | yes | `test -d /var/lib` |
| Free space on `/var` | ≥ 256 MiB | `df` |
| `/var/lib/cosmix/` does not exist OR is owned by `root:root 0755` | yes | `stat` |

A pre-existing `/var/lib/cosmix/` not owned `root:root 0755`
SHALL fail-closed; the installer SHALL NOT chown the parent tree
to satisfy the SPEC.

### 4.8 No conflicting Cosmix install

Before running, the installer SHALL detect any pre-existing
Cosmix install:

- Any binary at `/opt/cosmix/bin/cosmix-*` not matching the
  MANIFEST's expected version is treated as an upgrade or
  conflict (§7).
- Any sysusers fragment at `/usr/lib/sysusers.d/cosmix.conf` is
  compared against the tarball's. Disagreement on a non-tombstoned
  entry is a fail-closed condition until reconciled.
- An older or pre-NS-4.0 hand-installed configuration (e.g. the
  pre-SPEC-10 `cosmix-jmap` paths from the example-node deployment
  history) SHALL be flagged for operator migration, not
  auto-migrated.

---

## 5. Installation Procedure

NS 4.0's install procedure drives SPEC 10 §6's three verification
phases through to a running, registered node. The procedure is
defined as a strictly ordered sequence; the installer (§6) is the
canonical implementation.

The procedure is **idempotent**: re-running it on a correctly
installed node SHALL be a no-op and SHALL exit zero. This is the
property `spec10_audit.sh` and the CI lint depend on.

### 5.1 Phase 0 — host-intrinsic preconditions

Run only the **host-intrinsic** preconditions: §4.1 (distro and
kernel), §4.2 (systemd availability), §4.6 (sysadm operator
identity), §4.7 (storage and filesystem), §4.8 (no conflicting
Cosmix install). Fail-closed on any failure. Phase 0 SHALL NOT
mutate any host state and SHALL NOT consult MANIFEST or any
in-tarball artifact.

Required output: a structured precondition report logged to
journald with `cosmix.spec=11 cosmix.spec.version=<spec11-version>
cosmix.phase=preconditions cosmix.result=ok|fail`, where
`<spec11-version>` is the `spec11_version` field from this SPEC's
frontmatter at build time (currently `1.0.0-rc.1`; matches L12).

### 5.2 Phase 1 — stage and MANIFEST-derived verification

Phase 1 establishes the trust chain and runs the
**MANIFEST-derived** preconditions (§4.3, §4.4, §4.5):

1. Verify the tarball envelope per §3.4.1 step 1 — using a
   host-installed `minisign`, **not** any binary from the
   tarball. (When invoked via the §12.1 bootstrap sequence the
   operator has already run this step manually; the installer
   re-runs it as defence-in-depth.)
2. Determine and **claim** the staging directory. Throughout
   this SPEC the metavariable `<staging>` refers to this
   directory — either the default
   `/var/lib/cosmix/.staging/<package>-<version>/` or, when
   the operator passes `--staging-dir <path>` (§6.2), that
   path. Every later use of `<staging>` (in §5.4–5.8 promote
   examples, §6.3 cleanup, §7.3 abort recovery, §12 worked
   examples) resolves to the directory claimed here. The
   trust model is single-principal: only `root` may write to
   the staging tree at any time, enforced by mode `0700`
   ownership `0:0` on the directory itself. All later phases
   trust files read from this tree; defending against a
   root-side attacker is out of scope for this SPEC (a root
   attacker has already won by the time the installer runs).

   2a. *Path selection.* If `--staging-dir <path>` was passed,
   use that path; otherwise default to
   `/var/lib/cosmix/.staging/<package>-<version>/`. The
   ancestor chain `/var/lib/cosmix/.staging/` is Phase-1
   self-bootstrapped: if `/var/lib/cosmix` does not exist,
   the installer SHALL `install -d -o 0 -g 0 -m 0755
   /var/lib/cosmix` first; then if `/var/lib/cosmix/.staging`
   does not exist, the installer SHALL `install -d -o 0 -g 0
   -m 0700 /var/lib/cosmix/.staging`. Both ancestor creations
   precede claiming the per-package staging directory.
   The §5.6 tmpfiles fragment is NOT a precondition for Phase 1
   staging (it cannot be — tmpfiles is promoted by Phase 5,
   which runs after Phase 1). `/tmp/...` defaults are not
   chosen because stock-Debian `/tmp` is sticky world-writable
   and protecting a child against pre-mkdir injection there is
   racier than picking a root-only parent.

   2b. *Two extraction cases.*

   - **Installer-extracts (the §6.2 already-installed form):**
     the installer SHALL `mkdir(<staging>, 0700)` as root, with
     `mkdir`'s atomic "fails if exists" semantics — fail-closed
     if `<staging>` already exists *unless* `--staging-dir`
     pointed at it (the operator-pre-extracted bootstrap case
     below). Then extract the tarball into `<staging>` with
     `tar --strip-components=1` so the tarball's single
     top-level directory (per §3.2) is collapsed away and
     `<staging>` is the immediate parent of `usr/`, `MANIFEST`,
     etc.
   - **Operator-pre-extracts (the §6.2 bootstrap form):** the
     operator's `mkdir` and `tar` calls SHALL run under `sudo`
     and SHALL be sequenced as `sudo install -d -o 0 -g 0 -m
     0700 <staging>` immediately followed by `sudo tar
     --strip-components=1 -xf … -C <staging>`. The directory is
     therefore root-owned `0700` *before* any tarball content
     lands inside it. The installer SHALL verify on entry that
     `<staging>` is owned `0:0` mode `0700`; if not, fail-closed.

   The exec-mountpoint probe (writing and executing a `0700`
   test file inside `<staging>`) SHALL run *after* the staging
   directory exists and is claimed — i.e. after 2b in the
   installer-extracts case, or as the first action on entering
   the installer in the operator-pre-extracts case. In the
   bootstrap case the operator has already executed
   `<staging>/opt/cosmix/bin/mix`, which serves as a stronger
   pre-installer exec probe than the SPEC could mandate; the
   in-installer probe is therefore defence-in-depth there.

   2c. *Sanitize content.* After `<staging>` holds the
   collapsed (`--strip-components=1`) tarball contents and
   before digest verification (step 4), the installer SHALL
   walk the tree and reject as fail-closed any:

   - setuid or setgid file,
   - world-writable file or directory (a sanity check; the
     `0700` parent already prevents non-root traversal into
     the tree),
   - symlink whose `realpath` resolves outside `<staging>`,
   - hard-link whose target inode is outside `<staging>`,
   - special file (block / char / FIFO / socket).

   The walk is one-pass and aborts on first hit.

   2d. *Path-based reads, root-only window.* All subsequent
   reads of staged files (digest verification in step 4, every
   promotion `install` call in phases 3–8) are path-based —
   the SPEC does not require `O_NOFOLLOW`/`fstat`-identity
   tracking at the Mix layer, and current Mix builtins
   (`read_file`, `hash_sha256`, `walk`, `run_rc("install …")`)
   are path-based. The path-based primitives are sufficient
   *given* the root-owned `0700` staging window: between 2c's
   walk and the last Phase-9 read, only `root` can mutate the
   tree, and a root-side attacker is already out of scope. A
   future amendment MAY tighten this to fd-relative ops if Mix
   gains the corresponding builtins; v1.0.0 does not depend on
   that.

   2e. *Failure semantics.* Any failure in 2a–2c SHALL
   fail-closed (exit 2) with an actionable diagnostic naming
   the offending path / mode / owner. The installer SHALL NOT
   silently fall back to a different path or relax any rule.
3. Parse and validate the MANIFEST: required frontmatter fields
   present, `target_distro == debian-13`, `spec10_registry_version`
   matches the SPEC 10 Appendix A version this installer was
   built against.
4. Verify every file in the tarball against its MANIFEST mode
   and sha256 entry.
5. Run §4.3 glibc check (`host_glibc >= MANIFEST.target_glibc_max`).
6. Validate `/etc/cosmix/site.toml` exists, is owned `root:root
   0644`, and parses as TOML with the §5.9 schema (the keys
   `mesh_cidr`, `edge_node`, `edge_ports` present and
   well-formed). This is read-only — the file is the operator's
   trusted on-host declaration of mesh membership and edge-node
   status. Phase 1 SHALL NOT touch the file.
7. Run §4.4.3 bind-policy and edge-node consistency checks
   against the parsed `site.toml` from step 6 plus live network
   state.
8. Run §4.5 `/etc/hosts` snippet diff (warn-only, never gates).

Phase 1 SHALL NOT touch any path under `/usr/`, `/etc/`,
`/run/`, or `/usr/lib/systemd/`. The only permitted Phase-1
writes under `/var/lib/` are the staging-tree ancestors and
the staging tree itself: when any of `/var/lib/cosmix`,
`/var/lib/cosmix/.staging`, or the per-package `<staging>`
directory does not exist, the installer SHALL create the
missing ancestor(s) with `install -d -o 0 -g 0 -m 0755` for
`/var/lib/cosmix` and `install -d -o 0 -g 0 -m 0700` for
`/var/lib/cosmix/.staging` and `<staging>` itself, in that
order, per §5.2 2a–2b. The installer SHALL NOT touch any
other path under `/var/lib/` during Phase 1. Step 2b's
`chmod` and `chown` operate only inside the claimed staging
tree. Any failure in steps 1–7 SHALL fail-closed (exit 2);
step 8 SHALL warn but not gate.

Step ordering is load-bearing:

- 2 (claim staging) precedes 4 (per-file digest verify), so
  verification runs against an immutable-to-non-root tree.
- 6 (site.toml parse) precedes 7 (bind-policy / edge-node check),
  so the bind check has a parsed `mesh_cidr` and `edge_*` fields
  to consult.
- Phase 8 (§5.9) re-uses the already-parsed `site.toml` to
  consistency-check daemon configs; it does NOT re-validate
  schema/ownership — that is Phase 1's responsibility.

### 5.3 Phase 2 — SPEC 10 §6.1 install preflight

Run `spec10_preflight.mix` (the canonical preflight script,
matching SPEC 10 §6.1 pseudocode) against the staged sysusers
fragment and the host's `getent passwd|group`. Fail-closed on
any conflict.

This phase is the gate before any registry mutation. The
installer SHALL NOT proceed past phase 2 on any preflight
warning that maps to a §6.1 fail-closed condition.

### 5.4 Phase 3 — promote sysusers fragment

```
sudo install -o 0 -g 0 -m 0644 \
  <staging>/usr/lib/sysusers.d/cosmix.conf \
  /usr/lib/sysusers.d/cosmix.conf
sudo systemd-sysusers /usr/lib/sysusers.d/cosmix.conf
```

Phase 3 SHALL be atomic from the point of view of an interrupting
operator: the fragment is installed, then sysusers is invoked,
with no intervening state. Failure in either step rolls back the
fragment to its previous state (or removes it if there was none)
and fails-closed.

### 5.5 Phase 4 — SPEC 10 §6.2 post-sysusers verification

Run `spec10_postcheck.mix` (the canonical post-sysusers script,
matching SPEC 10 §6.2). Verification is per-class, exactly as
SPEC 10 §6.2 specifies:

- **Daemon-identity entries** SHALL resolve to a user *and*
  same-numbered group with `uid == gid == registered UID` and
  `name == registered name`.
- **Shared-credential group entries** SHALL resolve to a group
  with `gid == registered GID` and `name == registered name`,
  SHALL have *no* same-named user, and every declared `m <user>
  <group>` line SHALL be present in `getent group <group>`.

Fail-closed on any mismatch. This phase is mandatory before any
tmpfiles fragment is applied or any unit is enabled.

### 5.6 Phase 5 — promote tmpfiles fragment (when present)

For packages that include secret-config directories or
package-created roots (per SPEC 10 §3.3), promote the tmpfiles
fragment and apply it:

```
sudo install -o 0 -g 0 -m 0644 \
  <staging>/usr/lib/tmpfiles.d/cosmix.conf \
  /usr/lib/tmpfiles.d/cosmix.conf
sudo systemd-tmpfiles --create /usr/lib/tmpfiles.d/cosmix.conf
```

Phase 5 SHALL run *after* phases 3–4. SPEC 10 §3.3 requires that
the cosmix-* groups exist with the correct GIDs before tmpfiles
chowns secret-config directories.

### 5.7 Phase 6 — install binaries

First, ensure the canonical binary directory exists (a fresh host has
neither `/opt/cosmix/` nor `/opt/cosmix/bin/`):

```
sudo install -d -o 0 -g 0 -m 0755 /opt/cosmix/bin
```

Then for each binary in the MANIFEST:

```
sudo install -o 0 -g 0 -m 0755 \
  <staging>/opt/cosmix/bin/<binary> \
  /opt/cosmix/bin/<binary>
```

Per the `feedback_copy_release_binary` memory, the install command
SHALL use `install -o 0 -g 0 -m 0755`, not `cp`. This guarantees
ownership and mode regardless of the staging directory's umask.

Binaries SHALL be installed into `/opt/cosmix/bin/` — the canonical
install location for every cosmix daemon binary AND the `mix`
interpreter, matching the global Mix-first / never-Python policy.

### 5.8 Phase 7 — install systemd units

For each unit in the MANIFEST:

```
sudo install -o 0 -g 0 -m 0644 \
  <staging>/usr/lib/systemd/system/<unit> \
  /usr/lib/systemd/system/<unit>
```

Then:

```
sudo systemctl daemon-reload
```

Phase 7 SHALL NOT enable or start any unit. Activation is phase 9.

### 5.9 Phase 8 — install per-host configuration (operator-driven)

NS 4.0 v1.0.0 does **not** ship per-host configuration. Per-host
values (WG bind addresses, mail hostnames, TLS material) are
operator-supplied and live under `/etc/cosmix/<d>/`. A single
host-wide `/etc/cosmix/site.toml` declares mesh-membership facts
that are not daemon-specific:

```toml
# /etc/cosmix/site.toml — operator-authored, root:root 0644
mesh_cidr   = "192.0.2.0/24"
edge_node   = false                    # true on the nominated edge
edge_ports  = []                       # MUST be subset of {22, 25, 443, 465, 8443}
```

The installer SHALL (Phase 8; the `site.toml` schema/ownership check
itself runs at Phase 1 step 6, §5.2 — Phase 8 takes that parse as
input and consistency-checks it against the daemon configs):

1. Verify `/etc/cosmix/<d>/config.toml` exists for each daemon in
   scope, owned `root:root 0644` for non-secret configs,
   `root:cosmix-<d> 0640` for configs containing secrets, or
   `root:cosmix-tls 0640` for TLS keypairs read by ≥2 daemons
   (per SPEC 10 §3.3 v1.1.0; public certificate material MAY remain
   `root:root 0644`). The shared-keypair pattern relies on the
   `cosmix-tls` group and its memberships being present on the host;
   that is satisfied automatically when this SPEC's Phase 3 promotes
   the canonical sysusers fragment (which carries the `g cosmix-tls`
   line plus the `m <d> cosmix-tls` lines per SPEC 10 v1.1.0
   Appendix A and §9.1).
2. Refuse to proceed (fail-closed) if any required daemon config
   file is absent. The operator SHALL author per-host configs
   out of band; NS 4.0 does not template them.
3. Validate config bindings against §4.4 using the
   already-parsed `site.toml`:
   - **Default case (`site.toml.edge_node = false`):** every
     listen address SHALL be either the host's WG address (when
     WG is present) or a loopback address (`127.0.0.1` / `::1`,
     when `--allow-loopback-only`). Any literal `0.0.0.0`,
     `[::]`, or non-WG public address is fail-closed.
   - **Edge case (`site.toml.edge_node = true`):** the default
     listens above are required, AND additional public-interface
     listens are permitted on ports listed in `site.toml.edge_ports`.
     Every entry in `edge_ports` SHALL be in the allowlist
     `{22, 25, 443, 465, 8443}`; any out-of-allowlist port is
     fail-closed. Each public-interface listen in a daemon
     config SHALL bind a port that appears in `edge_ports`.

A future amendment MAY define a `cosmix-config` add-on package or
a templating step; v1.0.0 is operator-authored only.

### 5.10 Phase 9 — enable and start

For each unit in scope:

```
sudo systemctl enable --now cosmix-<d>.service
```

`cosmix-noded.service` SHALL be enabled first (per SPEC 10 §5.4
unit ordering). The other daemons follow once the broker is up.

Phase 9 watches each `systemctl is-active` for ≤ 60 seconds and
fails-closed if any unit does not reach `active (running)`.

### 5.11 Phase 10 — SPEC 10 §6.3 startup verification

`cosmix-noded.service`'s startup runs SPEC 10 §6.3 internally
before binding the ABP socket. The installer SHALL additionally
run `spec10_audit.sh` against the live host:

- Every unit in scope is `active (running)`.
- Every running daemon's `User=` and `Group=` resolve to the
  registry name and the registered UID.
- The sysusers fragment on disk matches the canonical Markdown
  registry's projection.
- §5.2 hardening directives are declared on every unit
  (CT-effective degradation in unprivileged containers is
  recorded but not failed-closed; see §10.3).

Phase 10 emits a structured success report on the `preflight`
ABP topic (now that the broker is up), with fields `cosmix.spec=11
cosmix.spec.version=<spec11-version> cosmix.phase=audit
cosmix.result=ok` plus the registry inventory. `<spec11-version>`
is the `spec11_version` from MANIFEST at build time (currently
`1.0.0-rc.1`).

### 5.12 Phase 11 — cleanup

The installer SHALL remove the claimed `<staging>` directory
(default `/var/lib/cosmix/.staging/<package>-<version>/`, or
the `--staging-dir` override) and SHALL NOT leave cached
tarballs or extracted trees on the host. The on-disk record of the installed package set is the
MANIFEST snapshot at `/var/lib/cosmix/.ns4-manifest/<package>-<version>.manifest`,
which the installer copies before cleanup (§7).

---

## 6. Mix Installer Contract

Per the global Mix-first / never-Python policy
(`feedback_no_python`, the workspace `CLAUDE.md`'s tooling-policy
section), the NS 4.0 installer is a Mix script.

### 6.1 Canonical name and path

The installer SHALL be named `ns4-install.mix` and SHALL be shipped
inside `cosmix-base` at:

```
/usr/share/cosmix/install/ns4-install.mix
```

A symlink `/usr/local/sbin/ns4-install` MAY be installed for
operator ergonomics; v1.0.0 does not require it.

### 6.2 Invocation

The installer's command-line **option grammar** is fixed. The
*interpreter path* and *script path* are parameterised by
install state, since on a fresh host neither `/opt/cosmix/bin/mix`
nor `/usr/share/cosmix/install/ns4-install.mix` exists yet — both
are first promoted by Phase 6 of the very install being run.

**Bootstrap form** (fresh host, or any install where the live
`mix` is about to be replaced):

```
sudo <staging>/opt/cosmix/bin/mix \
    <staging>/usr/share/cosmix/install/ns4-install.mix \
    <options ...>
```

**Already-installed form** (add-on install on a host where
`cosmix-base` is already at the same major-minor version, e.g.
`--role knowledge` over an existing `mesh-citizen`):

```
sudo /opt/cosmix/bin/mix /usr/share/cosmix/install/ns4-install.mix \
    <options ...>
```

In either form `<options ...>` SHALL conform to the same option
grammar:

```
    --tarball <path-to-cosmix-base.tar.zst>
    --pubkey <path-to-minisign-pubkey>
    --role <pack>
    [--indexd-tarball <path-to-cosmix-indexd.tar.zst>]
    [--staging-dir <path>]
    [--dry-run]
    [--no-start]
    [--allow-loopback-only]
    [--allow-downgrade --justification <reason>]
```

The bootstrap form SHALL pass `--staging-dir <staging>`
explicitly so Phase 1 reuses the operator's pre-extraction
(§5.2 step 2). The already-installed form MAY omit
`--staging-dir`; the installer extracts to its default location.

Required arguments:

- `--tarball` — absolute path to the `cosmix-base` tarball.
- `--pubkey` — absolute path to the operator's minisign public key.
- `--role` — one of `mesh-citizen`, `knowledge` (§2.4).

Optional arguments:

- `--indexd-tarball` — absolute path to the `cosmix-indexd`
  tarball. REQUIRED when `--role knowledge`. Forbidden otherwise.
- `--staging-dir <path>` — directory the installer SHALL use for
  tarball extraction and quarantined-mix execution. Default is
  `/var/lib/cosmix/.staging/<package>-<version>/` (the parent
  `/var/lib/cosmix/.staging` is Phase-1 self-bootstrapped per
  §5.2 2a — created `0:0 0700` if absent). The path SHALL live
  on an exec-permitted filesystem; the installer probes for
  `noexec` and fails-closed (exit 2) if it cannot execute from
  the path. The override is required on hardened hosts whose
  `/var` mount is `noexec`, or in the §6.2 bootstrap form when
  the operator pre-extracts the tarball outside the default
  parent. If the directory already contains the extracted
  tarball (§12.1 bootstrap case) the installer reuses it after
  re-verifying per-file digests; it does not re-extract.
- `--dry-run` — run phases 0–2 only; do not promote any
  artifact or mutate live package state. Quarantined writes
  to `<staging>` and its `/var/lib/cosmix*` ancestors per
  §5.2 2a are permitted (they are how Phase 1 stages content
  for verification). Useful for first-time host audits.
- `--no-start` — run through phase 8 (units installed) but
  SHALL NOT enable or start any service. Reserved for staged
  rollouts.
- `--allow-loopback-only` — permit the §4.4.3 WG-presence check to
  pass when no WireGuard interface exists, falling back to
  loopback-only binds. Intended for development hosts and CI
  fixtures only; SHALL NOT be combined with any `edge_node = true`
  config (§4.4.2). Default behaviour without this flag is to
  fail-closed when WG is absent.
- `--allow-downgrade` — permit a downgrade across the SPEC 10
  registry-version or SPEC 11 major-version boundary (§7.2).
  REQUIRES the companion `--justification "<reason>"` argument;
  the installer SHALL fail-closed (exit 64) if `--allow-downgrade`
  is passed without `--justification`, and SHALL log the
  justification string to journald per §6.4.
- `--justification "<reason>"` — operator-supplied free-text
  rationale for an exceptional install. REQUIRED with
  `--allow-downgrade`; ignored (with a warning) otherwise.
  Multi-word justifications SHALL be quoted.

Unknown arguments SHALL fail-closed (exit 64).

### 6.3 Exit codes

| Code | Meaning |
|------|---------|
| 0 | All phases completed; node is at NS 4.0 conformance for the role pack. |
| 1 | Phase 0 (preconditions) failure. |
| 2 | Phase 1 (envelope / MANIFEST) failure. |
| 3 | Phase 2 (SPEC 10 §6.1) failure. |
| 4 | Phase 3–5 (sysusers / tmpfiles) failure. |
| 5 | Phase 6–8 (binary install / unit install / per-host config validation) failure. |
| 6 | Phase 9 (start) failure. |
| 7 | Phase 10 (audit) failure. |
| 64 | Usage error (bad arguments). |
| 70 | Internal Mix evaluator error. |

Phase 11 (cleanup) failures are non-fatal: the installer SHALL
log them to journald and SHALL still exit 0 if all earlier
phases succeeded. A failed cleanup leaves staging files under
the `--staging-dir` path (default
`/var/lib/cosmix/.staging/<package>-<version>/`) for operator
review and removal; the install itself is complete.

The exit code identifies the *first* failed phase. Subsequent
phases SHALL NOT run.

### 6.4 Logging

The installer SHALL emit a structured run record to journald
under syslog identifier `ns4-install`, with fields:

- `cosmix.spec=11 cosmix.spec.version=<version>`
- `cosmix.phase=<0..11>` per phase
- `cosmix.result=ok|fail`
- `cosmix.errors=<list>` on failure

journald is the canonical sink for installer logs. The
installer SHALL NOT create `/var/log/cosmix/` for its own use:
per SPEC 10 §3.5, that parent SHALL exist only when a daemon
requires it, and the installer is not a daemon. An operator
who wants a file-based audit trail SHALL `journalctl -u
ns4-install` or `journalctl -t ns4-install` to materialise it
out of band.

### 6.5 Mix-first discipline

The installer SHALL be expressible entirely in Mix builtins. Any
shell-out (`sudo`, `systemctl`, `getent`, `install`,
`systemd-sysusers`, `systemd-tmpfiles`, `wg show`, `dpkg`,
`uname`, `df`, `stat`) is permitted because these tools are the
*right* primitive — the rule is that no Python, no Lua, and no
home-grown shell loops perform logic that should be a Mix
builtin.

When the installer hits a missing Mix capability, the response
SHALL be to add the builtin to `cosmix-lib-mix` and rebuild
(`feedback_mix_use_builtins_not_shell`,
`project_mix_builtin_gaps`). The `deployment-tools/spec10_*.mix` scripts
that informed this SPEC are the ground-truth precedent.

### 6.6 Source vs shipped

The installer source SHALL live at:

```
deployment-tools/ns4-install.mix
```

(parallel to the existing `deployment-tools/spec10_*.mix` scripts).

The shipped artifact at `/usr/share/cosmix/install/ns4-install.mix`
SHALL be byte-identical to the source at the package's build SHA.
A CI lint (§11 L14) verifies this.

**Materialisation status.** As of this SPEC's first revision the
installer source is **pending** — it has not yet been authored.
CI lints **L3, L7, L14** all depend on the file existing and SHALL
report `skipped — installer source pending` in their CI output
until `deployment-tools/ns4-install.mix` lands; the lint suite as a whole
SHALL still pass during this window. The remaining lints
(L1–L2, L4–L6, L8–L13) are independent of the installer source
and SHALL remain hard gates.

Once the source lands, the SPEC 11 frontmatter SHALL be bumped to
`1.0.0-rc.2` (or directly to `1.0.0` if no other changes
accumulate), L3/L7/L14 SHALL become hard gates, and a
`cosmix-base` v1.0.0 tarball MAY be cut for production deployment.
A v1.0.0 tarball SHALL NOT be cut while any of L3/L7/L14 is in
the skipped state.

---

## 7. Upgrade, Downgrade, Rollback, Removal

### 7.1 Upgrade

An upgrade is an install of a newer-version tarball over an
existing installed package. The installer SHALL run the phases in
the order below; the **stop-before-mutate** discipline (step 3
preceding all file promotion) is the transaction boundary that
keeps the live system from running half-replaced binaries against
unchanged units or vice versa:

1. Run §5 phases 0–2 against the new tarball (preconditions,
   stage + MANIFEST verify, SPEC 10 §6.1 preflight). All checks
   that can fail SHALL fail before any service is touched.
2. Compare the staged sysusers fragment to the live one. Append-
   only additions (new tombstones, new entries with UIDs > the
   previous next-free) are accepted. Renumbering, renaming, or
   tombstone removal SHALL fail-closed (per SPEC 10 §2.4).
3. **Stop services** in reverse-dependency order, **before** any
   file is promoted out of staging:
   `cosmix-indexd` → `cosmix-webd` → `cosmix-maild` →
   `cosmix-noded`.
4. Promote new sysusers / tmpfiles fragments (§5.4–§5.6),
   re-running the SPEC 10 §6.2 post-sysusers verification (this
   SPEC's §5.5).
5. Replace binaries (§5.7) and units (§5.8) atomically (via
   `install` over the existing path, which is mv-replace). Each
   `install` call is itself atomic; the upgrade as a whole is
   *not* a single transaction (see §7.3).
6. Promote per-host config validation (§5.9, no file changes —
   read-only check that the operator's existing configs still
   satisfy §4.4 against the new units).
7. Restart services in dependency order (§5.10).
8. Run §5.11 audit. Fail-closed on any regression.

The `MANIFEST` of the *previous* installed version SHALL be
preserved at
`/var/lib/cosmix/.ns4-manifest/<package>-<previous-version>.manifest`
as an audit record. It identifies *what* was installed before
(versions, file digests) but does not contain file contents and
SHALL NOT be used to attempt a content-restoring rollback (see
§7.3).

### 7.2 Downgrade

Downgrade is permitted within the same SPEC 10 registry version
and the same SPEC 11 major version. The installer SHALL refuse a
downgrade across either boundary unless given an explicit
`--allow-downgrade` flag plus a documented operator justification.

The reason for the gate: downgrading binaries against a registry
that has already been extended (new daemons, new tombstones) can
leave the live system referencing entries the older binaries do
not understand.

### 7.3 Rollback and phase-N abort recovery

NS 4.0 v1.0.0 does **not** ship an auto-restoring rollback: the
installer does not retain previous-version file contents on disk.
Two distinct cases are addressed below.

**Case A — Phase-N abort during an in-progress install/upgrade.**
The installer's transaction boundaries (§7.1 step 3 stop-before-
mutate; §5 phases 0–2 perform no promotion or live-package
mutation — their only writes are the quarantined staging tree
and its `/var/lib/cosmix*` ancestors per §5.2 2a) define which
states are "safe-aborted" vs "mid-mutation":

- **Abort at phases 0–2 (no live-package mutation):** no
  registry fragment, binary, unit, or `/etc/cosmix/` config
  has been promoted; the live package set is untouched. The
  installer MAY have created `/var/lib/cosmix`,
  `/var/lib/cosmix/.staging`, and `<staging>` (per §5.2 2a)
  and populated `<staging>` with the extracted tarball — these
  are quarantined writes, not live-system mutations. The
  installer SHALL exit non-zero, leave the staging tree
  (default `/var/lib/cosmix/.staging/<package>-<version>/`, or
  the `--staging-dir` override) for inspection, and require
  no recovery action beyond optionally removing the staging
  tree.
- **Abort at phases 3–5 (sysusers / tmpfiles):** registry
  fragments are promoted but no binary or unit has been replaced
  yet. SPEC 10 §2.4 append-only semantics guarantee the new
  fragment is forward-compatible with the still-running old
  binaries. The installer SHALL exit non-zero and SHALL log the
  half-promoted state; the operator MAY safely restart the install
  with the same tarball after addressing the cause.
- **Abort at phases 6–8 (binaries / units / per-host config) on
  a fresh install:** services have not yet been started (phase 9
  is not reached). The installer SHALL exit non-zero; the
  operator SHALL re-run the installer to completion, or remove
  the partially installed package via §7.4.
- **Abort at phases 6–8 on an upgrade:** services were stopped at
  §7.1 step 3 and are still down. Some binaries / units may have
  been replaced and others not. The live system is in a
  service-down mixed state. Recovery: the operator SHALL re-run
  the installer with the *previous* tarball using the §7.3 Case B
  procedure below to restore a coherent set, OR re-run with the
  current tarball after addressing the cause.
- **Abort at phase 9 (start) or phase 10 (audit):** all files
  have been promoted; the failure is a runtime problem (unit
  refused to start, audit caught a regression). The previous
  binaries are gone; there is no in-installer rollback. The
  operator SHALL diagnose the start/audit failure or invoke
  Case B with the previous tarball.

In every Case A path the §7.1 step 3 stop-before-mutate ordering
guarantees that a mid-upgrade abort does NOT leave a daemon
running against a partially replaced unit + binary pair: the
worst case is "all daemons stopped, files in a mix" — recoverable
by re-running an installer (current or previous tarball) to
completion.

**Case B — Operator-driven rollback to a previous version.**
Rollback is performed by:

1. Locating the previously installed package's tarball
   (`cosmix-<package>-<previous-version>-<arch>.tar.zst` plus its
   `.sig` envelope), which the operator SHALL have retained out
   of band as part of release management. The
   `/var/lib/cosmix/.ns4-manifest/<package>-<previous-version>.manifest`
   audit record identifies exactly which prior version is needed.
2. Re-running the installer with that tarball. Rollback of the
   `cosmix-base` package SHALL use the §6.2 **bootstrap form**
   (extract the *previous* tarball into a fresh root-owned `0700`
   staging directory, invoke the previous tarball's quarantined
   mix and `ns4-install.mix`) — even though a live `mix` already
   exists on the host, the live binary is about to be replaced
   with the older one and SHALL NOT be used to drive its own
   replacement. Rollback of `cosmix-indexd` only (the live
   `mix`/`ns4-install.mix` are not being replaced) MAY use the
   §6.2 **already-installed form**. Either form takes the same
   options:

   ```
   --tarball <previous>.tar.zst
   --pubkey /etc/cosmix/keys/release.pub
   --role <pack>
   --allow-downgrade --justification "<reason>"
   ```

3. The §7.2 downgrade gate applies: across SPEC 10 registry-version
   or NS 4.0 major-version boundaries the installer fails-closed
   without `--allow-downgrade` and the operator's
   `--justification` string.

The `.ns4-manifest/<package>-<previous-version>.manifest` audit
record is the *identifier* (versions, file digests) of what was
previously installed; it is not a content backup. A future
amendment MAY add an auto-rolling `--rollback` mode backed by a
content-preserving package cache (e.g. retaining the previous
tarball under `/var/cache/cosmix/`); v1.0.0 explicitly does not.

### 7.4 Removal

Removal of a package SHALL:

1. Stop and disable every unit in scope.
2. Remove binaries, units, and per-package data under
   `/usr/share/cosmix/`.
3. Remove the package's sysusers entries via a new fragment that
   tombstones them (rather than deleting them), per SPEC 10 §2.4.
4. **NOT** remove `/etc/cosmix/<d>/` (operator may have local
   secrets) or `/var/lib/cosmix/<d>/` (state may be valuable).

A separate `--purge` flag SHALL extend removal to also delete
`/etc/cosmix/<d>/` and `/var/lib/cosmix/<d>/`. `--purge` is
explicit, never default.

Removal of `cosmix-base` while `cosmix-indexd` is installed
SHALL fail-closed: the operator SHALL remove `cosmix-indexd`
first.

---

## 8. The sysadm Boundary

NS 4.0 expects but does not configure the host's administrative
account.

### 8.1 Expected operator identity

The host SHALL carry a non-root administrative user. By NS 3.0
convention this user is `sysadm` at UID 1000, in the `sudo` group,
with passwordless sudo. NS 4.0 v1.0.0 does NOT pin the UID and
does NOT enforce the `sudo` membership; it only requires that the
installer's invoking user can elevate to root via `sudo`.

The recommended configuration matches `historical daily-driver setup guide (2026-03-24)`
§4 (NS 3.0 user setup), but that document is not normative for
NS 4.0.

### 8.2 What NS 4.0 does and does not configure on sysadm

NS 4.0 SHALL NOT:

- Create the sysadm account.
- Modify sysadm's groups, shell, or home directory.
- Install SSH keys for sysadm.
- Configure sudoers fragments.

NS 4.0 SHALL:

- Verify sudo capability of the invoking user (§4.6).
- Refuse to run as a `cosmix-*` registry user (§4.6).
- Log the operator's username and UID into the §6.4 install
  record.

### 8.3 Cohabitation with NS 3.0 desktop convention

A host MAY simultaneously be an NS 3.0 daily-driver workspace
(per `historical daily-driver setup guide (2026-03-24)`) and an NS 4.0 mesh
node. The UID space NS 4.0 cares about is partitioned as:

- 500–599 — substrate daemons (SPEC 10, NS 4.0; the only range
  this SPEC normatively owns).
- 1000 — sysadm administrative user (NS 3.0 convention; not
  pinned by NS 4.0 but the install procedure assumes a
  sudo-capable non-root operator at this UID by convention).
- ≥ 1000 — local interactive users and NS 3.0 NetServa vhost
  users (SPEC 10 §1.2 boundary). The exact partition between
  named human users and vhost users inside this range is an
  NS 3.0 / NetServa concern outside NS 4.0's scope.

NS 4.0 only owns 500–599. The other ranges are referenced for
collision-avoidance (§9) and SHALL be respected.

---

## 9. NS 3.0 Vhost Coexistence

NetServa vhost users (per SPEC 10 §1.2: UID ≥ 1000 by NS 3.0
convention) are out of scope for this SPEC. NS 4.0 makes one normative claim about them:

**Vhost UIDs SHALL NOT collide with the SPEC 10 substrate
registry (500–599).**

Per SPEC 10 §6.1, the install preflight already detects this
case: a vhost user holding UID 500–599 is a fail-closed
condition. NS 4.0's installer surfaces such conflicts with a
specific message naming the conflicting vhost user, the substrate
entry it collides with, and a recommended manual remediation
(typically: rename or renumber the vhost user, since the
substrate registry is append-only and cannot move).

NS 4.0 does NOT:

- Allocate vhost UIDs.
- Document the vhost UID range or naming.
- Provision vhost user accounts, groups, home directories, or
  per-vhost services.
- Configure NetServa virtual hosts in any web/mail/proxy server.

Future SPECs (working title: "NetServa Vhost") may address those
concerns. Until then, NS 3.0 conventions (`historical daily-driver setup guide (2026-03-24)`
plus historical NetServa documentation) are the operator's
reference.

---

## 10. Conformance

### 10.1 Conformance levels

A node conforms to NS 4.0 at one of three levels.

**Level 0 (Pre-conformance).** Binaries copied by hand,
sysusers fragment authored ad hoc, units written by hand, no
preflight run. Permitted only as a transitional state during
SPEC 10 / NS 4.0 bootstrap. Not permitted on any production
mesh node.

**Level 1 (Installed).** The node was installed by `ns4-install.mix`
through phase 10 (which includes the §5.11 audit) succeeding, and
SPEC 10 conformance Level 1 holds. Required for every NS 4.0 mesh
node.

**Level 2 (Hardened).** Level 1 plus SPEC 10 conformance Level 2
(§5.2 mandatory hardening directives present on every unit) plus
the phase 10 audit emitted a `preflight.ok` event with the §5.11
inventory. Required for any internet-exposed mesh node.

### 10.2 Per-package conformance

Each installed package independently asserts a conformance level.
A `cosmix-base` Level 2 install plus a hand-installed
`cosmix-indexd` is a `mesh-citizen Level 2` + `knowledge Level 0`
node. Whole-node conformance is the **minimum** of installed
packages' conformance levels; a `Level 0` add-on prevents the
node as a whole from claiming `Level 2`.

### 10.3 CT-effective degradation (historical Incus evidence)

> **Port note (2026-08-31):** Incus left the live system on 2026-08-09.
> The evidence below explains the old exception but does not define current
> nspawnd/systemd-nspawn degradation semantics; those are not verifiable from
> this chapter.

Per the example-node first-deployment finding (`historical first-deployment evidence (2026-05-09)`
F2), the then-current unprivileged Incus / LXC containers carried an auto-injected
drop-in that relaxes a subset of §5.2 hardening directives. NS
4.0 v1.0.0 records this as a known degradation:

- The unit declarations SHALL still be Level 2 (CI lint enforces).
- The runtime is recognized as "Level 2 declared / CT-effective
  degraded" by the §5.11 audit and the §6.4 install record.
- Userspace-only directives (`MemoryDenyWriteExecute`,
  `RestrictAddressFamilies`, `SystemCallFilter`,
  `CapabilityBoundingSet`/`AmbientCapabilities`,
  `StateDirectory`, `ProtectSystem=strict`) are confirmed
  in-effect via `systemctl show` and remain Level 2 in the
  audit's effective view.

This is documented behaviour, not a fail-closed condition.
Internet-exposed mesh nodes SHOULD run on a kernel and container
configuration that does not require this degradation.

### 10.4 Re-attestation cadence

A node's NS 4.0 conformance attestation SHALL be re-run:

- On every NS 4.0 install or upgrade (via the §5.11 phase).
- On every `cosmix-noded` cold start (via SPEC 10 §6.3).
- On operator demand via `spec10_audit.sh` (no-op if the node
  is conformant).

Continuous attestation (e.g. on every `daemon-reload` or every
`systemctl restart`) is OPTIONAL and is a substrate-self-aware
concern (SPEC 07), not an NS 4.0 requirement.

---

## 11. CI Lint Shape

A CI lint SHALL be runnable in the source tree and SHALL verify
all of the following invariants. The lint is part of the
substrate's self-observation surface (per the Three Design
Criteria) and is itself agent-operable.

| ID | Invariant |
|----|-----------|
| L1 | The canonical `deployment-config/sysusers/cosmix.conf` matches the projection of SPEC 10 Appendix A (regenerate, diff, fail on diff). This is a re-statement of SPEC 10 lint L7 from the NS 4.0 packaging side. |
| L2 | Every shipped `deployment-config/systemd/cosmix-*.service` carries the SPEC 10 §5.1 + §5.2 directives (incorporates SPEC 10 lints L8, L13). |
| L3 | The `ns4-install.mix` source at `deployment-tools/ns4-install.mix` parses and runs `--dry-run` in CI against a synthetic Debian 13 host fixture. (Pending materialisation: SHALL report `skipped — installer source pending` until the file lands; see §6.6.) |
| L4 | The installer's `MANIFEST` reader rejects synthetic-bad fixtures: missing required frontmatter field, MANIFEST hash mismatch, MANIFEST mode mismatch, MANIFEST extra-file (file in tarball not in MANIFEST), MANIFEST missing-file (file in MANIFEST not in tarball), envelope signature failure. (Six cases; each SHALL fail-closed in CI.) |
| L5 | The §3.1 tarball naming regex matches every artifact produced by the build: `^cosmix-(base|indexd)-[0-9]+\.[0-9]+\.[0-9]+(-[a-z0-9.+-]+)?-(amd64|arm64)\.tar\.zst$`. |
| L6 | The installer's `--role` matrix is exhaustive: every value in §2.4 is handled, no value not in §2.4 is accepted. |
| L7 | The phase ordering of §5 is encoded as a constant in the installer source and is referenced (rather than re-inlined) by every phase function. The SPEC table and the source constant SHALL match exactly. (Pending materialisation: SHALL report `skipped — installer source pending` until the file lands; see §6.6.) |
| L8 | Glibc-skew check: `MANIFEST.target_glibc_max <= host_glibc_max` is enforced in §5.2 / §4.3 (Phase 1, *after* MANIFEST is trusted), exits 2 on failure (§6.3). CI runs the check against a synthetic too-old-glibc fixture and verifies fail-closed at Phase 1. |
| L9 | Layered-bind check (§4.4): every shipped *default* config (the templates referenced by `deployment-config/systemd/cosmix-*.service`) binds WG-or-loopback only — no `0.0.0.0`, no `[::]`, no public address literal. A unit MAY bind a public address only when its per-host config sets `edge_node = true` AND the bound port is in the §4.4.2 allowlist `{22, 25, 443, 465, 8443}`; CI verifies this by parsing default-config templates (substring-rejecting `0.0.0.0` / `[::]`) and, separately, parsing per-host edge-node fixtures (rejecting any bound port not in the allowlist). Supersedes the prior blanket "no `0.0.0.0` substring" rule and re-scopes `feedback_wg_only_binding` accordingly. |
| L10 | No `python` or `python3` invocation appears in any shipped install artifact (`ns4-install.mix`, `spec10_*.{mix,sh}`, MANIFEST files, units). (`feedback_no_python` enforcement.) |
| L11 | Tarball MANIFEST's `spec10_registry_version` matches `10a-daemon-identity-profile.md` Appendix A's version field. The lint extracts both and compares. |
| L12 | Tarball MANIFEST's `spec11_version` matches the `spec11_version:` contract field in this profile's frontmatter at build time, not its editorial `version:`. |
| L13 | Conformance levels in §10 are pairwise consistent with SPEC 10 §8.1: NS 4.0 Level N requires SPEC 10 Level N for every installed package. |
| L14 | The shipped `ns4-install.mix` is byte-identical to `deployment-tools/ns4-install.mix` at the build SHA (regenerate from source on every CI run, diff, fail on diff). (Pending materialisation: SHALL report `skipped — installer source pending` until the file lands; see §6.6.) |

The lint SHALL be invoked by CI on every pull request that
touches `10c-package-install-profile.md`, `deployment-tools/ns4-install.mix`,
`deployment-config/sysusers/cosmix.conf`, `deployment-config/systemd/cosmix-*.service`,
or any file in `$COSMIX/src/crates/cosmix-{noded,maild,webd,indexd}/`.
Failure SHALL block merge.

---

## 12. Examples

### 12.1 First-time install on a fresh Debian 13 mesh node

The first action SHALL be a host-side envelope verification using a
`minisign` binary installed by the host's package manager (per §3.4.1
clause 1) — **before** the tarball is extracted. Only after that
verification passes does the installer get extracted and run:

```
# 0. Host-side trust anchor: install minisign from the OS package
#    manager (one-time, before the first tarball ever arrives).
sysadm@example-node:~$ sudo apt-get install -y minisign

# 1. Host-side envelope verification of the tarball using the
#    operator's pinned pubkey. This is the trusted action that
#    permits extraction; it MUST happen before tar/zstd touches
#    the archive.
sysadm@example-node:~$ minisign -Vm /tmp/cosmix-base-1.0.0-amd64.tar.zst \
    -p /etc/cosmix/keys/release.pub
Signature and comment signature verified

# 2. Only now extract under a quarantine path. The trust gate
#    above is what authorises this step. The staging directory
#    SHALL be root-owned mode 0700 from the moment any tarball
#    content lands inside it (§5.2 step 2). The default location
#    is `/var/lib/cosmix/.staging/<package>-<version>/` (root-only
#    parent, self-bootstrapped by Phase 1 if absent — §5.2 2a);
#    a sibling under `/tmp` such as `/tmp/cosmix-stage-XXXX` is
#    also acceptable provided the directory itself is created
#    root-owned 0700 *before* tar runs. The example below uses
#    /tmp for visibility (a single mkdir + tar pair on a fresh
#    host); operators may equally pre-create
#    `/var/lib/cosmix/.staging/cosmix-base-1.0.0` so the
#    chosen path matches the installer's default. The
#    bootstrap form still passes `--staging-dir "$STAGING"`
#    regardless of whether `$STAGING` is the default path or
#    an override — without it the installer would treat the
#    pre-extracted directory as a fail-closed collision per
#    §5.2 2b. Hardened hosts mounting the chosen filesystem
#    noexec SHALL pass `--staging-dir <path>` pointing to an
#    exec-permitted parent.
sysadm@example-node:~$ STAGING=/tmp/cosmix-stage-base-1.0.0
sysadm@example-node:~$ sudo install -d -o 0 -g 0 -m 0700 "$STAGING"
sysadm@example-node:~$ sudo tar --zstd \
    -xf /tmp/cosmix-base-1.0.0-amd64.tar.zst \
    --strip-components=1 \
    -C "$STAGING"

# 3. Invoke the in-tarball installer using the **quarantined**
#    Mix binary from the just-extracted tarball — NOT a
#    preinstalled `/opt/cosmix/bin/mix` (a fresh Debian host has
#    none; on a base upgrade the live mix is about to be replaced
#    by Phase 6). After --strip-components=1 the tarball's single
#    top-level directory (§3.2) has been collapsed away and the
#    interpreter lives at $STAGING/opt/cosmix/bin/mix. The installer
#    re-runs envelope and per-file digest verification as
#    defence-in-depth (§3.4.1 clauses 2–3); on success it promotes
#    the quarantined mix to /opt/cosmix/bin/mix as part of Phase 6.
#    The installer is given the same $STAGING path so Phase 1
#    reuses this extraction rather than re-extracting.
sysadm@example-node:~$ sudo "$STAGING/opt/cosmix/bin/mix" \
    "$STAGING/usr/share/cosmix/install/ns4-install.mix" \
    --tarball /tmp/cosmix-base-1.0.0-amd64.tar.zst \
    --pubkey /etc/cosmix/keys/release.pub \
    --staging-dir "$STAGING" \
    --role mesh-citizen
```

This is the §6.2 **bootstrap form** — used on fresh hosts AND on any
base-package upgrade where the live `mix` is about to be replaced.
Add-on installs against an already-running `cosmix-base` use the
§6.2 already-installed form (§12.2).

Expected timeline (example-node-class CT, ~50 ms tarball, AMD64):

| Phase | Duration | Effect |
|------:|---------:|--------|
| 0 | < 1 s | Preconditions logged |
| 1 | < 1 s | Tarball verified, MANIFEST validated |
| 2 | < 1 s | SPEC 10 §6.1 OK to create (8 entries) |
| 3 | ~ 1 s | sysusers fragment promoted, sysusers run, 8 users created |
| 4 | < 1 s | SPEC 10 §6.2 verification OK |
| 5 | < 1 s | tmpfiles promoted (none secret-config in v1.0.0 base, fragment may be empty) |
| 6 | ~ 1 s | 4 binaries installed |
| 7 | < 1 s | 3 units installed, daemon-reload run |
| 8 | < 1 s | Per-host configs verified |
| 9 | ~ 5 s | noded → maild → webd enabled-and-started |
| 10 | < 1 s | Audit OK, `preflight.ok` emitted |
| 11 | < 1 s | Cleanup |

Total: ~ 12 s on a warm cache.

### 12.2 Adding the knowledge add-on

Both tarballs SHALL be envelope-verified host-side (§12.1 step 1)
before invocation:

```
sysadm@example-node:~$ minisign -Vm /tmp/cosmix-base-1.0.0-amd64.tar.zst \
    -p /etc/cosmix/keys/release.pub
sysadm@example-node:~$ minisign -Vm /tmp/cosmix-indexd-1.0.0-amd64.tar.zst \
    -p /etc/cosmix/keys/release.pub

# Add-on installs run on a host where cosmix-base is already
# installed, so `/opt/cosmix/bin/mix` and the shipped installer at
# `/usr/share/cosmix/install/ns4-install.mix` already exist as
# Phase-6-promoted artifacts; the live mix is acceptable here
# because no mix-replacing phase runs against the indexd tarball.
sysadm@example-node:~$ sudo /opt/cosmix/bin/mix \
    /usr/share/cosmix/install/ns4-install.mix \
    --tarball /tmp/cosmix-base-1.0.0-amd64.tar.zst \
    --indexd-tarball /tmp/cosmix-indexd-1.0.0-amd64.tar.zst \
    --pubkey /etc/cosmix/keys/release.pub \
    --role knowledge
```

In this dated worked example (2026-05-09), the installer detects
`cosmix-base 1.0.0` is already installed,
skips its phases, and runs phases 1–11 against the indexd
tarball only. The indexd unit is enabled and started after the
base set is confirmed live.

### 12.3 First-time install — fail-closed on UID conflict

This is the §12.1 bootstrap path on a fresh host (quarantined mix,
not preinstalled mix), failing at Phase 2 because the host already
has a non-cosmix user occupying a SPEC 10 reserved UID. The
operator's pre-extraction step (§12.1 step 2) is elided here for
brevity; assume `$STAGING` was created root-owned 0700 and the
tarball was extracted into it with `--strip-components=1`.

```
sysadm@example-host:~$ STAGING=/tmp/cosmix-stage-base-1.0.0
sysadm@example-host:~$ sudo "$STAGING/opt/cosmix/bin/mix" \
    "$STAGING/usr/share/cosmix/install/ns4-install.mix" \
    --tarball /tmp/cosmix-base-1.0.0-amd64.tar.zst \
    --pubkey /etc/cosmix/keys/release.pub \
    --staging-dir "$STAGING" \
    --role mesh-citizen
ERROR: SPEC 10 §6.1 preflight failed.
  - registry entry cosmix-noded (uid 500) conflicts with existing
    user 'somepkg' (uid 500). Resolve manually before re-running:
    rename or renumber 'somepkg' to a UID outside 500-599.
ns4-install: phase 2 failed; halting at preflight.
exit 3
```

The installer makes no live-package changes — no registry
fragment, binary, unit, or `/etc/cosmix/` config has been
promoted (§7.3 Case A, phases 0–2). The pre-extracted staging
tree at `$STAGING` (an operator-controlled path here, not
under `/var/lib/cosmix/.staging/`) remains for inspection and
reuse. The operator inspects the diagnostic, removes or
renumbers the conflicting user, and re-runs from the same
`$STAGING` path.

### 12.4 Failed install — glibc skew

```
sysadm@example-old-host:~$ STAGING=/tmp/cosmix-stage-base-1.0.0
sysadm@example-old-host:~$ sudo "$STAGING/opt/cosmix/bin/mix" \
    "$STAGING/usr/share/cosmix/install/ns4-install.mix" \
    --tarball /tmp/cosmix-base-1.0.0-amd64.tar.zst \
    --pubkey /etc/cosmix/keys/release.pub \
    --staging-dir "$STAGING" \
    --role mesh-citizen
ERROR: phase 1 MANIFEST-derived check failed.
  host glibc 2.31 < MANIFEST.target_glibc_max 2.41.
  Either rebuild this tarball on a host of glibc <= 2.31, or
  upgrade this host to a Debian release with glibc >= 2.41.
ns4-install: phase 1 failed.
exit 2
```

This is the case the `feedback_glibc_skew_local_to_container`
memory was written to prevent re-discovering. The check runs at
Phase 1 (after MANIFEST is trusted, §4.3) — not Phase 0 — so the
operator sees a glibc-skew error before any file is promoted.

---

## Appendix A. Package Manifest (initial draft)

This appendix records the v1.0.0 package contents as committed
*shape*. The CI lint (§11 L1–L4) regenerates and diffs against
this appendix; drift is a build error once every listed source
file lands.

**Materialisation status (informative).** Each entry below carries
a `[present]` / `[pending]` marker reflecting the state of the
corresponding source file in the tree at this SPEC revision.
Pending entries SHALL land in the source tree before a
`cosmix-base` v1.0.0 production tarball is cut; the SPEC's
frontmatter version SHALL bump (e.g. to `1.0.0-rc.2`) when each
pending entry materialises. CI lints L1–L4 SHALL treat pending
entries as informational (skip their per-file check) and SHALL
hard-gate present entries.

### A.1 `cosmix-base-1.0.0-amd64.tar.zst`

```
# tarball-path                                        mode    source-status
MANIFEST                                              (frontmatter + one line per packaged file below)
usr/local/bin/cosmix-noded                            0755    [present, $COSMIX/src/crates/cosmix-noded]
usr/local/bin/cosmix-maild                            0755    [present, $COSMIX/src/crates/cosmix-maild]
usr/local/bin/cosmix-webd                             0755    [present in tree, $COSMIX/src/crates/cosmix-webd]
usr/local/bin/mix                                     0755    [present, $COSMIX/src/crates/cosmix-mix]
usr/lib/sysusers.d/cosmix.conf                        0644    [present, deployment-config/sysusers/cosmix.conf]
usr/lib/tmpfiles.d/cosmix.conf                        0644    [pending, deployment-config/tmpfiles/cosmix.conf]
usr/lib/systemd/system/cosmix-noded.service           0644    [present, deployment-config/systemd/cosmix-noded.service]
usr/lib/systemd/system/cosmix-maild.service           0644    [present, deployment-config/systemd/cosmix-maild.service]
usr/lib/systemd/system/cosmix-webd.service            0644    [pending, deployment-config/systemd/cosmix-webd.service]
usr/share/cosmix/LICENSE                              0644    [present, ./LICENSE]
usr/share/cosmix/README.md                            0644    [present, ./README.md]
usr/share/cosmix/etc/hosts-snippet.txt                0644    [present, deployment-config/hosts-snippet.txt]
usr/share/cosmix/install/ns4-install.mix              0755    [pending, deployment-tools/ns4-install.mix — see §6.6]
usr/share/cosmix/install/spec10_preflight.mix         0755    [present, deployment-tools/spec10_preflight.mix]
usr/share/cosmix/install/spec10_postcheck.mix         0755    [present, deployment-tools/spec10_postcheck.mix]
usr/share/cosmix/install/spec10_audit.sh              0755    [present, deployment-tools/spec10_audit.sh]
```

### A.2 `cosmix-indexd-1.0.0-amd64.tar.zst`

```
# tarball-path                                        mode    source-status
MANIFEST                                              (frontmatter + one line per packaged file below)
usr/local/bin/cosmix-indexd                           0755    [present, $COSMIX/src/crates/cosmix-indexd]
usr/lib/systemd/system/cosmix-indexd.service          0644    [pending, deployment-config/systemd/cosmix-indexd.service]
usr/share/cosmix/indexd/                              (per-daemon read-only data, contents TBD)
```

The `cosmix-indexd` package's per-daemon data layout is left
under-specified at v1.0.0 because the daemon's model assets are
not yet finalised. A future amendment SHALL nail this down.

**Pending → present checklist (must clear before v1.0.0 cut):**

- `deployment-config/systemd/cosmix-webd.service`
- `deployment-config/systemd/cosmix-indexd.service`
- `deployment-config/tmpfiles/cosmix.conf`
- `deployment-tools/ns4-install.mix` (§6.6)
- A buildable `cosmix-webd` binary build target wired into the
  workspace's release-build matrix (the `$COSMIX/src/crates/cosmix-webd`
  crate exists in tree but is not yet listed as a release artifact
  on every mesh node per `feedback_copy_release_binary`).

The `[present]` markers in this appendix were verified against
the working tree at SPEC 11 v1.0.0-rc.1; if a marker disagrees
with the tree at a later revision, the appendix is the bug, not
the tree — file a fix before relying on the lint.

---

## Appendix B. Glibc Skew Reference Table

Informational. Records the glibc baselines for distributions
relevant to NS 4.0's build / target boundary.

| Distribution | glibc | Role |
|--------------|-------|------|
| Debian 13 Trixie | 2.41 | NS 4.0 v1.0.0 build host AND target |
| Debian 12 Bookworm | 2.36 | Below v1.0.0 target — fail-closed if installer attempts |
| Ubuntu 24.04 LTS | 2.39 | Below v1.0.0 target — fail-closed |
| Alpine Linux | musl, no glibc | Out of scope (NS 3.0 daily-driver only) |

The build host's glibc SHALL be ≤ every target host's glibc.
The former recommendation was to build inside a Debian 13 Incus CT. Incus is
no longer live; this chapter does not verify a replacement build fixture.
Use the current monorepo build/deploy guidance before producing artefacts.

---

## Appendix C. NS 3.0 ↔ NS 4.0 transition notes

Informational. Records the migration expectations for an
existing NS 3.0 host being brought to NS 4.0 substrate plane.

### C.1 What carries over

- `sysadm` UID 1000 — unchanged.
- vhost users UID ≥ 1000 (NS 3.0 numbering, per SPEC 10 §1.2) — unchanged, per §9.
- `/etc/hosts` mesh block — unchanged shape; the NS 4.0 snippet
  is the same content as `deployment-config/hosts-snippet.txt`.
- WireGuard configuration — unchanged; NS 4.0 verifies but does
  not provision.

### C.2 What changes

- Substrate daemons (any pre-NS-4.0 hand-installed
  `cosmix-noded`, `cosmix-maild`, `cosmix-jmap` from the
  pre-SPEC-10 era) SHALL be migrated to the SPEC 10 identity
  registry. The example-node first-deployment journal documents the
  worked example.
- Pre-SPEC-10 paths (e.g. `/var/lib/cosmix-jmap/`) SHALL be
  moved to SPEC 10 paths (`/var/lib/cosmix/maild/`) with chown
  to the registry user.
- Hand-rolled systemd units SHALL be replaced by the canonical
  units from the tarball.

### C.3 What is NOT migrated by NS 4.0

- The cosmix desktop user (NS 3.0 `cosmix` UID 1001 in the
  daily-driver convention) — out of scope.
- NetServa vhost users — out of scope.
- COSMIC desktop, fonts, themes, etc. — out of scope.
- Operator SSH key material — operator-managed.

---

## Appendix D. Changes to this Specification

### 1.0.0-rc.1 — 2026-05-09

Initial publication, release-candidate 1. Establishes the NS 4.0
package set, tarball layout, MANIFEST and integrity envelope,
host preconditions (including the host-side minisign-first
bootstrap-trust path in §3.4.1 and the layered WG-or-loopback
bind policy with single-edge opt-in in §4.4), the 12-phase
install procedure with stop-before-mutate transaction boundary
in §7.1, the Mix installer contract (`ns4-install.mix`, §6),
upgrade / rollback / phase-N abort recovery semantics (§7),
sysadm boundary, NS 3.0 vhost coexistence rule, three
conformance levels with CT-effective degradation handling, and
14 CI lint invariants (L3 / L7 / L14 deferred until installer
source materialises per §6.6).

Target distribution: Debian 13 Trixie, amd64.

`1.0.0` (production cut) is reserved for the revision in which
the §6.6 / Appendix A pending list is fully cleared and L3 / L7 /
L14 become hard gates. Release-candidate revisions
(`1.0.0-rc.N`) accumulate fixes from the cooperation-loop review
cycle until that condition holds.

---

© 2026 the project maintainer / Cosmix Project. Licensed under the same terms
as the Cosmix repository (LICENSE).
