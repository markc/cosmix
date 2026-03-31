# Build and Distribution

How cosmix binaries are built, optimized, and deployed across the mesh.

## Foundation: Alpine Edge + musl

All cosmix binaries target musl for static linking. Build natively on Alpine Edge where all system libraries (mesa, wayland, fontconfig) are musl-linked, eliminating cross-compilation issues.

Static musl binaries are self-contained ELF executables with no runtime libc dependency. They run on any Linux distribution — CachyOS, Debian, Ubuntu, Fedora, Alpine, bare initramfs — anything with a Linux kernel.

## Performance: mimalloc global allocator

musl's built-in allocator is simple but slow under multithreaded contention (5-15% overhead for allocation-heavy async Rust). Adding mimalloc as the global allocator eliminates this gap entirely:

```rust
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

Added to all 9 binary crates via workspace dependency `mimalloc = { version = "0.1", default-features = false }`.

## Performance: x86-64-v3 target CPU

CachyOS rebuilds all packages with x86-64-v3 (AVX2, FMA, BMI1/BMI2). The same optimization applies to builds via RUSTFLAGS:

```bash
RUSTFLAGS="-C target-cpu=x86-64-v3" cargo build --release
```

This gives AVX2/FMA SIMD codegen while keeping static portability. The binary runs on any x86-64-v3 capable CPU (Haswell 2013+). Combined with mimalloc, this matches CachyOS-v3 glibc performance.

## Binary classification

| Binary | Type | Lua | Notes |
|--------|------|-----|-------|
| `cosmix` | daemon | Lua 5.4 / LuaJIT | Feature-gated: `--features lua54` for musl |
| `cosmix-web` | headless service | Lua 5.4 / LuaJIT | Feature-gated: `--features lua54` for musl |
| `cosmix-portd` | headless service | Lua 5.4 / LuaJIT | Feature-gated: `--features lua54` for musl |
| `cosmix-embed` | headless service | None | Always musl-compatible |
| `cosmix-jmap` | headless service | None | Always musl-compatible |
| `cosmix-calc` | GUI (libcosmic) | None | Build natively on Alpine |
| `cosmix-mail` | GUI (libcosmic) | via cosmix_lib | Feature-gated: propagates lua54 from cosmix-daemon |
| `cosmix-toot` | GUI (libcosmic) | None | Build natively on Alpine |
| `cosmix-view` | GUI (libcosmic) | None | Build natively on Alpine |

GUI apps require mesa/wayland/fontconfig — available as Alpine packages. Cross-compiling GUI apps from a glibc host fails due to pkg-config mismatches. Native Alpine builds work.

## Lua runtime

LuaJIT does not build with musl (uses glibc internals: `_dl_find_object`, `MAP_32BIT`). All musl builds use Lua 5.4 instead, controlled by cargo features:

- Default build: `luajit` feature (for glibc development on CachyOS)
- musl build: `lua54` feature

Feature gates exist on: `cosmix-daemon`, `cosmix-web`, `cosmix-portd`, `cosmix-mail` (propagates to cosmix-daemon).

Lua 5.4 is ~10-20% slower for script-heavy workloads but cosmix scripts are glue, not compute-heavy. The difference is not measurable in practice.

## Build scripts

| Script | Purpose | Where to run |
|--------|---------|-------------|
| `scripts/build-alpine.sh` | Native Alpine musl build (all tiers) | Inside cosmixos CT |
| `scripts/build-musl.sh` | Cross-compile headless musl + glibc daemon | CachyOS host |
| `scripts/build-image.sh` | Create distributable CT images | CachyOS host |
| `scripts/cosmixos-setup.sh` | First-boot WireGuard + hostname setup | Inside fresh CT |

### build-alpine.sh

Builds all 9 binaries natively on Alpine Edge. On Alpine, `cargo build --release` produces musl binaries by default — no `--target` flag needed.

```bash
./scripts/build-alpine.sh              # Build all tiers
./scripts/build-alpine.sh headless     # Tier 1+2 only
./scripts/build-alpine.sh desktop      # Tier 3+4 only
./scripts/build-alpine.sh --strip      # Strip after building
./scripts/build-alpine.sh --install    # Install to ~/.local/bin
```

### build-musl.sh

Cross-compiles headless musl binaries from a glibc host. GUI apps cannot be cross-compiled.

```bash
./scripts/build-musl.sh               # Build headless + daemon
./scripts/build-musl.sh --strip        # Strip binaries
./scripts/build-musl.sh --install      # Install to ~/.local/bin
```

## Image variants

| | cosmixos-mesh | cosmixos-desktop |
|---|---|---|
| **Purpose** | Headless mesh node | Full desktop + self-rebuild |
| **COSMIC desktop** | No | Yes (1.0.8) |
| **Binaries** | 4 headless | All 9 |
| **Rust toolchain** | No | Yes |
| **Source code** | No | Yes (git clone) |
| **WireGuard** | Yes | Yes |
| **Estimated size** | ~50-80 MB compressed | ~450-550 MB compressed |

Build with: `./scripts/build-image.sh [mesh|desktop|both]`

## OpenRC services

Alpine uses OpenRC. Service files in `scripts/etc/init.d/`:

```bash
rc-service cosmix-web start
rc-service cosmix-portd start
rc-service cosmix start
rc-update add cosmix-web default    # enable on boot
```

## Deployment

Static binaries deploy via simple copy:

```bash
scp cosmix-web node:/usr/local/bin/
```

No package manager, no dependency resolution, no container runtime needed.

For CT images, use `incus image import` or extract `rootfs.tar.xz` for Proxmox `pct create`.

## Install path

All cosmix binaries install to `~/.local/bin/` (dev) or `/usr/local/bin/` (CT images).
