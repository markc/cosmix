# Shared static assets

Cosmix uses a shared installation tree for fonts, Material Symbols and colour
emoji, with optional XDG user overrides. Applications read local files through
`cosmix-lib-assets`; webd can serve those same files through an explicitly
enabled versioned route. Static resources need no asset daemon, database,
blobd/filesd dependency or runtime network request.

## Install

From a checkout with Mix installed:

```text
mix setup.mix --assets-only
mix setup.mix --assets-only --system
```

The first command installs into `$XDG_DATA_HOME/cosmix/assets`, defaulting to
`$HOME/.local/share/cosmix/assets`. The second installs shared resources under
`$COSMIX_SHARE/assets`, normally `/opt/cosmix/share/assets`, using sudo for the
system destination. Neither command builds or installs binaries. Desktop setup
also installs the assets; use `--skip-assets` when using a preinstalled set or
working offline.

The standalone bootstrap supports scratch destinations and inspection:

```text
mix share/assets/install.mix --list
mix share/assets/install.mix --root /tmp/cosmix-assets
mix share/assets/install.mix --root /tmp/cosmix-assets --verify
```

For an installed system tree, use
`mix /opt/cosmix/share/assets/install.mix --system --verify`, adjusting the prefix
if configured differently. Run `--user` without sudo; it uses the current
process user's HOME and XDG variables. The system prefix parent must already
exist and be readable/traversable by the intended consumers. The installer
creates a missing share directory, but does not alter existing ancestors.

The pinned core set includes Inter Variable upright, Noto Serif upright/italic,
JetBrains Mono upright/italic, Material Symbols Rounded with its upstream
codepoint catalogue, and Noto Color Emoji 2.051. Upstream WOFF2 files are included
where available. There are 15 downloads totalling 37,575,307 bytes (35.83 MiB).
Inter italic and additional script/CJK packs are not part of this initial set.

The source lock is `share/assets/core.conf.mix`. It records upstream revisions,
HTTPS URLs, sizes, SHA-256/BLAKE3 checksums, role paths, actual family names,
licences and a local stylesheet. Downloads use generic Mix HTTP primitives;
no application contacts Google Fonts while rendering.

## Storage and lookup

```text
assets/
  install.mix                 # installed system bootstrap
  core.conf.mix               # installed bootstrap lock
  current -> sets/2026-10-01-core-2
  sets/2026-10-01-core-2/
    manifest.conf.mix
    fonts.css
    fonts/
    icons/
    emoji/
    web/
    licences/
```

Set IDs are immutable. The bootstrap downloads into a private staging directory
on the destination filesystem, verifies all files, publishes the complete set
and atomically switches `current`. Repeating installation verifies and reuses
the existing bytes. An error preserves the previous activation. An existing ID
with different manifest bytes is refused. Directories use 0755 and files 0644;
only the installing operator writes system resources.

Native discovery chooses a complete set in this order:

1. `$XDG_DATA_HOME/cosmix/assets/current`.
2. `<entry>/cosmix/assets/current` in `$XDG_DATA_DIRS` order, default
   `/usr/local/share:/usr/share`.
3. `$COSMIX_SHARE/assets/current`, default `/opt/cosmix/share/assets/current`.

Relative XDG paths are ignored. Absent sets fall through; malformed existing
sets report an error. A selected `current` link is resolved once, preserving one
consistent set directory throughout its use. `COSMIX_SHARE` is a shared resource
setting independent of the source checkout's `COSMIX`; configure it consistently
for applications and daemons when changing the installation prefix.

Adding `/opt/cosmix/share` to `XDG_DATA_DIRS` alone would search
`/opt/cosmix/share/cosmix/assets`. The resolver explicitly handles the installation
prefix fallback, so no such environment adjustment is required.

Glyph atlases and other disposable rendering caches belong in memory or
`$XDG_CACHE_HOME/cosmix`, default `$HOME/.cache/cosmix`. Font registration uses the
verified files directly. Placing files under this directory does not automatically
make fontconfig scan them; applications using fontconfig need explicit directory
configuration and a font-cache rebuild.

## Rust API

The Cargo package is `cosmix-lib-assets`; the Rust crate is `cosmix_assets`.

```rust
use cosmix_assets::AssetSet;

fn main() -> anyhow::Result<()> {
    if let Some(set) = AssetSet::discover()? {
        if let Some(path) = set.font_path("sans") {
            println!("{}: {}", set.set_id(), path.display());
        }
        if let Some(home) = set.icon("home") {
            println!("{home}");
        }
    }
    Ok(())
}
```

`load_current(root)` uses an explicit installation root. `open_published(root,
set_id)` opens one fixed set for server use. `font_paths()` provides the native
TTF files; `family(role)` provides the font's internal family name. `icon(name)`
uses the pinned Material Symbols catalogue. `file_path(relative)` only resolves
allowlisted manifest files and the known stylesheet/manifest metadata. `verify()`
streams size and hash checks without retaining the complete set in memory.

Renderer adapters register the shared fonts in their existing font systems.
Explicit user font choices retain precedence over the installed role defaults.
Variable-axis handling and colour emoji support remain renderer capabilities;
the presence of a font file is not proof that every renderer supports FILL,
optical size, bitmap emoji or COLRv1. Existing system/embedded fallbacks remain
available when no installed set exists.

## webd and mesh

webd can expose explicitly configured system sets through
`/_cos/assets/<set-id>/<path>`. Browser pages load the set's `fonts.css`, whose
relative URLs keep every font request bound to that same set. The route serves
only manifest files and known metadata; staging directories, bootstrap resources,
`current` and user overrides are not exposed. The existing per-site `/assets/`
route keeps its existing behaviour.

Fixed-set URLs permit long-lived immutable caching. Mutable discovery URLs must
revalidate instead. Font/CSS MIME types, `nosniff`, conditional requests and HEAD
are part of the static serving contract. The default is same-origin use. Enable
`serve --assets-cross-origin` or `webd.shared_assets_cross_origin: true` to let
pages on other nodes/origins load these public fonts anonymously. This adds
`Access-Control-Allow-Origin: *` to shared asset responses and never enables
credentialed cross-origin reads.

A filesystem path is local to one node. Browser clients can fetch public bytes
from webd; native clients install a local set for offline startup. Requests to
install or activate sets across nodes use noded ABP control. HTTP static delivery
must not become a substitute for node control.

## Licences and maintenance

The fonts retain OFL-1.1 and Material Symbols retain Apache-2.0. Original licence
texts accompany the downloaded set. Preserve applicable NOTICE and reserved font
names when adding or modifying resources. These software resources are separate
from an unrestricted CC0 media catalogue; do not relabel third-party fonts/icons.

Git stores text scripts, locks, provenance and licence records. Downloaded binary
assets stay outside Git. When updating resources, pin a new upstream revision,
verify its bytes and licences, then assign a new set ID. Keep previously published
sets while native readers or versioned web URLs still need them. An interrupted
installer may leave an inactive staging directory; remove it manually only after
confirming the installer is no longer running. Atomic visibility is guaranteed;
power-loss durability of the final directory/link renames is not claimed.

See the [XDG specification](https://specifications.freedesktop.org/basedir/latest/),
[Inter](https://github.com/rsms/inter),
[JetBrains Mono](https://github.com/JetBrains/JetBrainsMono),
[Material Symbols](https://github.com/google/material-design-icons) and
[Noto Emoji](https://github.com/googlefonts/noto-emoji/tree/v2.051).
