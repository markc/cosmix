"""Prepare CC0 Coast Sand03 texture maps inside Blender; no runtime Python."""
import bpy
import os
from pathlib import Path

root = Path(os.environ.get('COSMIX_MEDIA_ROOT', Path(os.environ.get('XDG_DATA_HOME', Path.home() / '.local/share')) / 'cosmix/media'))
for source, dest, colourspace in [
    ('coast_sand_03_diff_4k.png', 'sand-basecolor.png', 'sRGB'),
    ('coast_sand_03_nor_gl_4k.png', 'sand-normal.png', 'Non-Color'),
]:
    image = bpy.data.images.load(str(root / 'source' / source), check_existing=False)
    image.colorspace_settings.name = colourspace
    image.scale(1024, 1024)
    image.filepath_raw = str(root / 'coast' / dest)
    image.file_format = 'PNG'
    image.save()
    print(f'COAST_MATERIAL_EXPORTED {dest} 1024x1024 CC0-1.0')
