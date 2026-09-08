"""Original Cosmix observatory. Run only inside Blender via the Mix build script."""
import bpy
import math
import json
import struct
import os
from pathlib import Path

root = Path(os.environ.get('COSMIX_MEDIA_ROOT', Path(os.environ.get('XDG_DATA_HOME', Path.home() / '.local/share')) / 'cosmix/media'))
(root / 'source').mkdir(parents=True, exist_ok=True)
bpy.ops.wm.read_factory_settings(use_empty=True)

def material(name, colour, metallic=0.0, emission=0.0):
    mat = bpy.data.materials.new(name)
    mat.use_nodes = True
    p = mat.node_tree.nodes.get('Principled BSDF')
    p.inputs['Base Color'].default_value = (*colour, 1)
    p.inputs['Metallic'].default_value = metallic
    p.inputs['Roughness'].default_value = 0.28
    p.inputs['Emission Color'].default_value = (*colour, 1)
    p.inputs['Emission Strength'].default_value = emission
    return mat

brass = material('Satin brass', (0.65, 0.32, 0.07), 0.8)
dark = material('Midnight titanium', (0.025, 0.055, 0.09), 0.7)
cyan = material('Ion cyan', (0.02, 0.65, 0.85), 0.2, 5)
warm = material('Amber markers', (1.0, 0.3, 0.025), 0.2, 3)

def finish(obj, name, mat, parent=None):
    obj.name = name
    obj.data.materials.append(mat)
    if parent:
        obj.parent = parent
    for poly in obj.data.polygons:
        poly.use_smooth = True
    return obj

def torus(name, radius, thickness, mat, parent=None):
    bpy.ops.mesh.primitive_torus_add(major_segments=128, minor_segments=12,
        major_radius=radius, minor_radius=thickness)
    return finish(bpy.context.object, name, mat, parent)

for i, (radius, axis, rate, tilt) in enumerate([
    (2.0, 'y', 0.18, (0.4, 0.2, 0.0)),
    (2.7, 'x', -0.12, (1.1, 0.3, 0.2)),
    (3.4, 'z', 0.08, (0.2, 1.0, 0.4)),
]):
    pivot = bpy.data.objects.new(f'Orbit {i + 1}', None)
    bpy.context.collection.objects.link(pivot)
    pivot.rotation_euler = tilt
    # Axes describe exported glTF local coordinates, not Blender world axes.
    pivot['cosmix_bg'] = {'version': 1, 'axis': axis, 'radians_per_second': rate}
    torus(f'Brass orbit {i + 1}', radius, 0.055, brass, pivot)
    torus(f'Luminous trace {i + 1}', radius - 0.09, 0.018, cyan, pivot)
    for j in range(12):
        angle = j * math.tau / 12
        bpy.ops.mesh.primitive_uv_sphere_add(segments=12, ring_count=8, radius=0.085,
            location=(radius * math.cos(angle), radius * math.sin(angle), 0))
        finish(bpy.context.object, f'Orbit {i + 1} marker {j + 1}', warm, pivot)

bpy.ops.mesh.primitive_ico_sphere_add(subdivisions=3, radius=0.7)
finish(bpy.context.object, 'Luminous core', cyan)
torus('Core equator', 0.9, 0.09, dark)
for z, radius, depth, mat in [(-2.45, 2.0, 0.25, dark), (-2.25, 1.7, 0.12, brass)]:
    bpy.ops.mesh.primitive_cylinder_add(vertices=96, radius=radius, depth=depth, location=(0, 0, z))
    finish(bpy.context.object, 'Observatory plinth', mat)

# Native .blend remains editable; only the exported GLB is embedded at runtime.
bpy.ops.wm.save_as_mainfile(filepath=str(root / 'source' / 'kinetic-observatory.blend'))
bpy.ops.export_scene.gltf(filepath=str(root / 'kinetic-observatory.glb'),
    export_format='GLB', export_extras=True, export_cameras=False,
    export_lights=False, export_animations=False)
data = (root / 'kinetic-observatory.glb').read_bytes()
magic, version, total = struct.unpack_from('<III', data)
assert magic == 0x46546C67 and version == 2 and total == len(data)
length, kind = struct.unpack_from('<II', data, 12)
assert kind == 0x4E4F534A
document = json.loads(data[20:20 + length])
motions = [n['extras']['cosmix_bg'] for n in document['nodes']
           if 'cosmix_bg' in n.get('extras', {})]
assert len(motions) == 3 and {m['axis'] for m in motions} == {'x', 'y', 'z'}
assert not document.get('cameras') and not document.get('animations')
assert not document.get('images')
assert 'KHR_lights_punctual' not in document.get('extensions', {})
assert len(document['materials']) == 4
print(f'GLB_VALIDATED nodes={len(document["nodes"])} meshes={len(document["meshes"])} motions={len(motions)} bytes={len(data)}')
print('COSMIX_OBSERVATORY_EXPORTED')
