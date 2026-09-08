# Observatory source assets

Original Cosmix media is CC0; code retains MIT. See `../MEDIA-LICENSE.md`.
Created with Blender 5.2.1 LTS. Editable `.blend` sources and runtime GLBs
live outside Git under the media root (`source/` contains editable originals).

From this package's directory, run `/opt/cosmix/bin/mix assets-src/export.mix`.
The media root defaults to `$XDG_DATA_HOME/cosmix/media`, falling back to
`$HOME/.local/share/cosmix/media`; set `COSMIX_MEDIA_ROOT` for authoring elsewhere.
This uses Python solely inside Blender to author/export assets. The generated
scene has no cameras, lights, external textures or Blender runtime scripts.
The optional MeshOptimizer exporter library is unnecessary for this plain GLB.

The generator overwrites these two generated assets. Save hand edits under
another name before regenerating. Blender may create a `.blend1` backup;
backups are not runtime assets and should not be committed.

`celestial-engine.blend` is a second original scene, with 50 nested motion
pivots. Regenerate it and its GLB using `export-celestial.mix` from the package
directory. Its generator is `celestial.py`, also executed only inside Blender.
