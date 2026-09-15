# Cosmix Bevy render patch

Source: crates.io [`bevy_render` 0.19.1](https://crates.io/crates/bevy_render/0.19.1),
archive SHA-256 `fc86a32b2eef9859e1e7d323c6d1a9150fe77aa17c30fd05abebc12f645929dc`.
Upstream repository: https://github.com/bevyengine/bevy. MIT/Apache-2.0 licences
are included. Cargo.toml is the published, normalised manifest; src/ and the
upstream README are copied unchanged except for `src/texture/texture_cache.rs`.
The workspace `[patch.crates-io]` and lockfile route all consumers here.

## Load-bearing runtime patch

This is not a test hook. Upstream TextureCache has private entries, ages and
reservation flags. Its public update() combines reservation release with ageing
and eviction. The host cannot preserve selected ages through upstream's public
API. Skipping update leaves entries taken; swapping in an empty cache hides
reusable entries from PBR. Holding cloned texture handles does not restore cache
membership. A dev-dependency-only patch cannot change production behaviour, and
a wrapper resource would not be the TextureCache used by Bevy's built-in systems.

The only source patch adds a retained-ID set, opt-in recent-usage capture and
two host methods (set_update_policy/recently_used). Cleanup captures taken IDs,
skips age increments for retained IDs, always clears taken flags, and clears
the one-update retention policy. get() and descriptor matching are unchanged.
The usage IDs drive production per-output snapshots, not just assertions.

The host can retain selected texture IDs for waiting outputs,
including across control-only updates. Cleanup always releases the
per-update `taken` flags, so PBR preparation can reuse the real cache on the
next wake. Descriptor matching and the normal three-update eviction rule are
unchanged. Usage capture is opt-in; other consumers keep upstream behaviour.

The scene host keeps one latest rendered-batch usage snapshot per active
output (at most 16). When several outputs render together, their snapshots
conservatively share that batch's IDs, including global shadow maps. This can
retain some peer-output textures until the waiting output next renders, but
does not accumulate resize history: each snapshot is replaced, not extended.
Removed/suspended outputs lose their own snapshot key — though a removed
output's texture IDs can persist inside a peer waiting output's conservatively
shared snapshot until that output next renders; device changes invalidate all.
Unpinned entries still age on every cleanup, including control wakes, so
changing shadow-map descriptors during suspension cannot accumulate history.

Waiting-output retention deliberately has no time limit. The application's
animation-clock reset after callback withholding does not release GPU targets.
The scene fixture runs real PBR get() calls inside Render using
Render::base_schedule(), PrepareResources and Cleanup, rather than inserting
an artificial direct before() edge. Mid-wake reservations remain in the same
cache; there is no restored-cache Arc duplication path.

When refreshing upstream, preserve only this texture-cache patch and re-run
the shell-host PBR, mixed-output, descriptor and recovery fixtures. The PBR
fixture is feature-gated and does NOT compile under a bare
`cargo test -p cosmix-shell-host` — run
`cargo test -p cosmix-shell-host --features scene-3d` (or co-select
`-p cosmix-bg-showcase`, whose dependency enables it). Recheck
TexturePlugin's Cleanup registration and Render's preparation-before-cleanup
ordering. Drop the vendor when upstream exposes equivalent selective ageing
and usage reporting (or a replacement host integration no longer needs it).
