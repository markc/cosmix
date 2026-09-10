# Unrestricted media library — proposal

Status: design proposal, 2026-09-07. No public hosting or publication service
has been deployed. The background preview currently uses a local media root.

## Purpose and licence boundary

Provide images, textures, 3D models, audio, MIDI, video and editable originals
that anyone can download, modify, redistribute and use commercially without
payment, an account, attribution conditions or share-alike requirements.
The core catalogue accepts CC0-1.0 and verified public-domain material.
Original project media is dedicated to CC0; source code retains its existing
MIT licence. Neither a free download nor an open licence automatically meets
this stricter catalogue policy. Record the rights for every constituent input,
including sound samples, musical compositions and performances. Do not claim
to clear unrelated trademark, personality or other non-copyright rights.

## Split metadata from objects

Git stores text: catalogue records, recipes, scripts, text model sources,
dependency manifests, hashes, licence/provenance records and documentation.
Binary originals and derivatives live outside Git, including `.blend`, `.glb`,
MIDI, image, audio and video files. No Git LFS dependency is required for users.
Existing history is not rewritten as part of this policy change.

A plain filesystem object store is sufficient initially:

```text
catalogue/index.json
catalogue/packs/coast-v1.json
objects/sha256/ab/<full-sha256>/skybox.png
objects/sha256/cd/<full-sha256>/source.blend
```

Objects are immutable and identified by the SHA-256 of their bytes. A changed
object receives a new path. Catalogue records give human names, creator,
licence and evidence URL, upstream source URL, retrieval date, byte count,
media type, dimensions/duration, dependencies, derivative recipe/tool version,
and hashes for each rendition. Preserve originals alongside practical runtime
and preview versions. IDs must not depend on the hosting provider or hostname.

Public GET/HEAD downloads need no account, API key, expiring link or DRM.
An ordinary browser or download client can use them. Include a browsable
catalogue and downloadable manifests so access does not depend on Cosmix.

## Native implementation

Use a dedicated public `cosmix-webd` vhost with static `www_dir` storage;
do not introduce a second server stack or a new media daemon. Generic static
serving already uses streaming `ServeDir` with bounded read buffers and
supports HEAD, byte ranges and Last-Modified revalidation. `/assets/` is a
reserved docs route; use `/objects/` for media. The existing CMS image upload
path is not a general-purpose multi-gigabyte publication pipeline.

Publication and replication between nodes must use filesd/noded over ABP.
The currently inspected `fs.read_blob` and `fs.write` paths do not provide a
general binary transfer: the former is a bounded text preview and the latter
accepts a string. Extend the owning native component with resumable bounded
binary chunks, staging, size/hash verification and atomic finalisation. Publish
a manifest only after every referenced object is verified and available.

Add immutable caching for hashed objects and revalidation for mutable catalogue
files; generic webd static serving does not currently set that policy. Asset
generation, rendering, thumbnail creation and audio/video transcoding run on
authoring machines, not in download requests.

## Desktop access and offline use

The preview's local root defaults to `$XDG_DATA_HOME/cosmix/media`, falling back
to `$HOME/.local/share/cosmix/media`, with an explicit `--media-root` override.
It does not fetch media while rendering. Future catalogue integration should
offer a small preinstalled starter pack, on-demand larger packs, size estimates,
hash-verified resumable fetches, and pinning for offline use. Keep editable
originals in durable user data; only evict files explicitly designated cache.

## Capacity and acceptance

One CPU and 1 GB RAM are a reasonable initial target for modest static-download
traffic. Total library capacity does not need to fit in RAM. Throughput,
concurrency, TLS CPU, egress allowance, disk and backup headroom must be tested
and monitored; no capacity benchmark is claimed by this proposal.

Start with a plain volume and a second independent backup/mirror. At larger
scale, keep the same URLs/manifests while adding disk, mirrors or a cache layer.
Content addressing allows verification and deduplication; it is not a backup.
Avoid accumulating rendered variants that can cheaply be regenerated.

Before public launch, verify through the native stack:

- Publish and download a binary larger than server RAM with bounded memory.
- Interrupt/resume upload and download; verify final length and SHA-256.
- Exercise HEAD, 206, 416, conditional requests and the cache policy.
- Ensure incomplete objects are never referenced by the published catalogue.
- Confirm anonymous read access and no public mutation route for objects.
- Restore an object and catalogue from the independent backup.
- Test an offline desktop pack and a corrupted/missing download.

## References

- [CC0](https://creativecommons.org/publicdomain/zero/1.0/)
- [Poly Haven asset licence](https://polyhaven.com/license)
- [Git LFS design](https://docs.github.com/en/repositories/working-with-files/managing-large-files/about-git-large-file-storage)
- [GitHub LFS billing and quota behaviour](https://docs.github.com/en/billing/concepts/product-billing/git-lfs)
