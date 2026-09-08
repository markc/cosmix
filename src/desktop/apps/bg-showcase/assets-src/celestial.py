"""The Celestial Engine: original kinetic sculpture, authored inside Blender."""
import bpy
import math
import json
import struct
import os
from pathlib import Path
from mathutils import Vector

root = Path(os.environ.get('COSMIX_MEDIA_ROOT', Path(os.environ.get('XDG_DATA_HOME', Path.home() / '.local/share')) / 'cosmix/media'))
(root / 'source').mkdir(parents=True, exist_ok=True)
bpy.ops.wm.read_factory_settings(use_empty=True)

def material(name, colour, metallic=0.0, glow=0.0):
    mat = bpy.data.materials.new(name)
    mat.use_nodes = True
    p = mat.node_tree.nodes.get('Principled BSDF')
    p.inputs['Base Color'].default_value = (*colour, 1)
    p.inputs['Metallic'].default_value = metallic
    p.inputs['Roughness'].default_value = 0.3
    p.inputs['Emission Color'].default_value = (*colour, 1)
    p.inputs['Emission Strength'].default_value = glow
    return mat

gold = material('Champagne alloy', (0.62, 0.38, 0.13), 0.85)
navy = material('Obsidian ceramic', (0.025, 0.045, 0.075), 0.65)
silver = material('Brushed platinum', (0.42, 0.56, 0.65), 0.85)
cyan = material('Arctic plasma', (0.015, 0.62, 1.0), 0.1, 5)
violet = material('Violet plasma', (0.4, 0.05, 0.9), 0.1, 4)
amber = material('Solar plasma', (1.0, 0.22, 0.015), 0.1, 4)

def pivot(name, parent=None, at=(0, 0, 0), tilt=(0, 0, 0), axis='y', speed=0.1):
    obj = bpy.data.objects.new(name, None)
    bpy.context.collection.objects.link(obj)
    obj.parent = parent
    obj.location = at
    obj.rotation_euler = tilt
    obj['cosmix_bg'] = {'version': 1, 'axis': axis, 'radians_per_second': speed}
    return obj

def finish(obj, name, mat, parent=None, smooth=True):
    obj.name = name
    obj.parent = parent
    obj.data.materials.append(mat)
    for face in obj.data.polygons:
        face.use_smooth = smooth
    return obj

def torus(name, radius, tube, mat, parent, at=(0, 0, 0), tilt=(0, 0, 0)):
    bpy.ops.mesh.primitive_torus_add(major_segments=96, minor_segments=8,
        major_radius=radius, minor_radius=tube, location=at, rotation=tilt)
    return finish(bpy.context.object, name, mat, parent)

def sphere(name, radius, mat, parent, at=(0, 0, 0), subdivisions=2):
    bpy.ops.mesh.primitive_ico_sphere_add(subdivisions=subdivisions, radius=radius, location=at)
    return finish(bpy.context.object, name, mat, parent, False)

def box(name, at, scale, mat, parent, angle=0):
    bpy.ops.mesh.primitive_cube_add(size=1, location=at, rotation=(0, 0, angle))
    obj = finish(bpy.context.object, name, mat, parent, False)
    obj.scale = scale
    return obj

def rod(name, a, b, radius, mat, parent):
    a, b = Vector(a), Vector(b)
    delta = b - a
    bpy.ops.mesh.primitive_cylinder_add(vertices=8, radius=radius, depth=delta.length,
        location=(a + b) / 2)
    obj = finish(bpy.context.object, name, mat, parent)
    obj.rotation_euler = delta.to_track_quat('Z', 'Y').to_euler()
    return obj

# Faceted star reactor inside two independently turning open cages.
core = pivot('Reactor heart', tilt=(0.2, 0.3, 0), axis='z', speed=0.23)
sphere('Blue dwarf', 0.65, cyan, core, subdivisions=3)
for i in range(8):
    angle = i * math.tau / 8
    a = (0.95 * math.cos(angle), 0.95 * math.sin(angle), 0)
    for z in (-1.3, 1.3):
        rod('Reactor lattice', a, (0, 0, z), 0.025, gold, core)
    sphere('Reactor vertex', 0.065, amber, core, a, 1)
for i in range(2):
    cage = pivot(f'Gyroscope {i}', tilt=(0.6 + i * 0.8, 0.4, 0), axis='x', speed=(-1)**i * 0.19)
    for j in range(3):
        torus('Gyroscope rail', 1.6 + 0.25 * i, 0.035, silver, cage,
              tilt=(j * math.pi / 3, 0, 0))
    torus('Gyroscope energy belt', 1.63 + 0.25 * i, 0.018, violet if i else cyan, cage)

# Two tilted toothed annuli: offset rails, luminous tick marks, floating teeth.
for i, radius in enumerate((2.65, 3.35)):
    gear = pivot(f'Chronometer annulus {i}', tilt=(0.3 + i * 0.9, i * 0.4, 0),
                 axis='y', speed=0.08 if i == 0 else -0.055)
    torus('Annulus body', radius, 0.07, gold, gear)
    for z in (-0.1, 0.1):
        torus('Annulus light rail', radius, 0.018, amber if i else cyan, gear, at=(0, 0, z))
    for j in range(48):
        angle = j * math.tau / 48
        r = radius + 0.13
        box('Radial escapement tooth', (r * math.cos(angle), r * math.sin(angle), 0),
            (0.22, 0.07, 0.18), gold if j % 4 else silver, gear, angle)
        if j % 4 == 0:
            sphere('Timing light', 0.06, amber if i else cyan, gear,
                   ((radius - 0.1) * math.cos(angle), (radius - 0.1) * math.sin(angle), 0), 1)

# Eight orbiting machines, each with local counter-rotation and moving vanes.
carousel = pivot('Satellite carousel', tilt=(0.25, -0.15, 0), axis='y', speed=0.045)
torus('Outer carrier', 4.45, 0.028, silver, carousel)
torus('Outer carrier plasma', 4.5, 0.018, violet, carousel)
for i in range(8):
    angle = i * math.tau / 8
    centre = (4.45 * math.cos(angle), 4.45 * math.sin(angle), 0)
    pod = pivot(f'Satellite {i}', carousel, centre, (0.2, angle, 0), 'x', 0.14 * (-1)**i)
    sphere('Satellite crystal', 0.24, amber if i % 2 else violet, pod)
    torus('Satellite gimbal', 0.43, 0.035, gold, pod)
    torus('Satellite gimbal cross', 0.43, 0.025, silver, pod, tilt=(math.pi / 2, 0, 0))
    rotor = pivot(f'Satellite {i} turbine', pod, axis='y', speed=-0.35)
    for j in range(3):
        a = j * math.tau / 3
        vane = pivot(f'Satellite {i} petal {j}', rotor,
                     (0.47 * math.cos(a), 0.47 * math.sin(a), 0), (0, 0.35, a), 'z', 0.12)
        box('Ceramic turbine vane', (0.14, 0, 0), (0.48, 0.18, 0.06), navy, vane)
        box('Vane illuminated edge', (0.14, 0.085, 0.01), (0.46, 0.018, 0.018), cyan, vane)

# Opposed polar crowns with rotating radial blades and suspended crystals.
for sign in (-1, 1):
    crown = pivot(f'Polar crown {sign}', at=(0, 0, sign * 2.8), axis='y', speed=sign * 0.11)
    torus('Crown collar', 0.8, 0.06, gold, crown)
    torus('Crown light', 0.82, 0.022, cyan, crown, at=(0, 0, sign * 0.1))
    for j in range(12):
        a = j * math.tau / 12
        rod('Crown rib', (0.8 * math.cos(a), 0.8 * math.sin(a), 0),
            (0.22 * math.cos(a), 0.22 * math.sin(a), sign * 0.9), 0.035, silver, crown)
    crystal = pivot(f'Polar crystal {sign}', crown, (0, 0, sign * 0.7), axis='z', speed=-sign * 0.22)
    obj = sphere('Polar crystal', 0.3, violet, crystal, subdivisions=1)
    obj.scale = (1, 1, 2)

bpy.ops.wm.save_as_mainfile(filepath=str(root / 'source' / 'celestial-engine.blend'))
bpy.ops.export_scene.gltf(filepath=str(root / 'celestial-engine.glb'),
    export_format='GLB', export_extras=True, export_cameras=False,
    export_lights=False, export_animations=False)
data = (root / 'celestial-engine.glb').read_bytes()
magic, version, total = struct.unpack_from('<III', data)
assert magic == 0x46546C67 and version == 2 and total == len(data)
length, kind = struct.unpack_from('<II', data, 12)
assert kind == 0x4E4F534A
document = json.loads(data[20:20 + length])
motions = [n['extras']['cosmix_bg'] for n in document['nodes'] if 'cosmix_bg' in n.get('extras', {})]
assert len(motions) >= 40
assert not document.get('cameras') and not document.get('animations') and not document.get('images')
assert 'KHR_lights_punctual' not in document.get('extensions', {})
print(f'CELESTIAL_VALIDATED nodes={len(document["nodes"])} meshes={len(document["meshes"])} motions={len(motions)} bytes={len(data)}')
