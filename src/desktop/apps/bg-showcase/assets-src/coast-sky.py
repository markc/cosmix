"""Convert the CC0 Cape Hill EXR into a display cubemap inside Blender.

This is a tonemapped visual sky, not a prefiltered HDR lighting map.
Source: https://polyhaven.com/a/cape_hill ; creator Greg Zaal ; CC0-1.0.
"""
import bpy
import numpy as np
import math
import os
from pathlib import Path

root = Path(os.environ.get('COSMIX_MEDIA_ROOT', Path(os.environ.get('XDG_DATA_HOME', Path.home() / '.local/share')) / 'cosmix/media'))
source = bpy.data.images.load(str(root / 'source/cape_hill_4k.exr'), check_existing=False)
w, h = source.size
pixels = np.empty(w * h * 4, dtype=np.float32)
source.pixels.foreach_get(pixels)
pixels = pixels.reshape(h, w, 4)

def tone(rgb):
    # Soft highlight roll-off, preserving warm daylight and deep blue water.
    rgb = np.maximum(rgb * 0.7, 0)
    return np.clip((rgb * (2.51 * rgb + 0.03)) / (rgb * (2.43 * rgb + 0.59) + 0.14), 0, 1)

def save(name, data, dest):
    height, width = data.shape[:2]
    img = bpy.data.images.new(name, width=width, height=height, alpha=True, float_buffer=True)
    img.pixels.foreach_set(np.ascontiguousarray(data).ravel())
    scene = bpy.context.scene
    scene.view_settings.view_transform = 'Standard'
    scene.view_settings.look = 'None'
    scene.view_settings.exposure = 0
    scene.view_settings.gamma = 1
    scene.render.image_settings.file_format = 'PNG'
    scene.render.image_settings.color_mode = 'RGB'
    scene.render.image_settings.color_depth = '8'
    dest.parent.mkdir(parents=True, exist_ok=True)
    img.save_render(str(dest), scene=scene)

preview = pixels[::4, ::4].copy()
preview[..., :3] = tone(preview[..., :3])
preview[..., 3] = 1
save('Cape Hill reference', preview, root / 'previews/cape-hill-reference.png')

n = 512
u, v = np.meshgrid((np.arange(n) + 0.5) / n * 2 - 1, (np.arange(n) + 0.5) / n * 2 - 1)
one = np.ones_like(u)
directions = [(one, -v, -u), (-one, -v, u), (u, one, v), (u, -one, -v), (u, -v, one), (-u, -v, -one)]
faces = []
for x, y, z in directions:
    length = np.sqrt(x*x + y*y + z*z)
    longitude = np.arctan2(z, x)
    latitude = np.arcsin(y / length)
    px = ((longitude / (2 * math.pi) + 0.5) * w - 0.5) % w
    py = np.clip((latitude / math.pi + 0.5) * h - 0.5, 0, h - 1)
    ix, iy = np.floor(px).astype(int), np.floor(py).astype(int)
    fx, fy = (px - ix)[..., None], (py - iy)[..., None]
    ix1, iy1 = (ix + 1) % w, np.minimum(iy + 1, h - 1)
    data = ((1-fx)*(1-fy)*pixels[iy, ix] + fx*(1-fy)*pixels[iy, ix1]
            + (1-fx)*fy*pixels[iy1, ix] + fx*fy*pixels[iy1, ix1]).astype(np.float32)
    data[..., :3] = tone(data[..., :3])
    data[..., 3] = 1
    faces.append(data)
# Cubemap rows are top-down, Blender pixel storage is bottom-up.
save('Cape Hill cubemap', np.concatenate(faces, axis=0)[::-1].copy(), root / 'coast/skybox.png')
print('COAST_SKY_EXPORTED size=512x3072 license=CC0-1.0')
