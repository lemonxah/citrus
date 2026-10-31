"""Decode BlenderKit exports and normalize starter weapons for Citrus.

blender --background --factory-startup --disable-autoexec --python tools/prepare_template_weapons.py
Outputs plain glTF (no Draco requirement), metres, -Z forward in Citrus.
"""
from pathlib import Path
import json
import math
import bpy
from mathutils import Matrix, Vector

ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / 'models/weapons/default'
OUTPUT.mkdir(parents=True, exist_ok=True)
manifest = {}
for name in ('rifle', 'pistol', 'blaster'):
    bpy.ops.wm.read_factory_settings(use_empty=True)
    bpy.ops.import_scene.gltf(filepath=str(ROOT / f'models/weapons/blenderkit/{name}/{name}.glb'))
    # Source textures are embedded as WebP. Citrus uses gltf::import, which
    # does not understand EXT_texture_webp (a required extension would make
    # the whole import fail). Force Blender to re-encode every image as PNG
    # before export so the exporter drops the WebP extension entirely.
    for image in bpy.data.images:
        if image.size[0] == 0:
            continue
        # Decode the (already in-memory) pixels and re-save as PNG so the
        # packed data is actually re-encoded, not just relabeled: Blender's
        # AUTO image export otherwise reuses the original WebP bytes verbatim.
        tmp_path = str(Path(bpy.app.tempdir) / f'{image.name}.png')
        image.filepath_raw = tmp_path
        image.file_format = 'PNG'
        image.save()
        image.pack()
    meshes = [o for o in bpy.data.objects if o.type == 'MESH']
    rotation = Matrix.Rotation(-math.pi / 2, 4, 'Y') if name != 'blaster' else Matrix.Identity(4)
    for mesh in meshes:
        world = rotation @ mesh.matrix_world
        mesh.parent = None
        for vertex in mesh.data.vertices:
            vertex.co = world @ vertex.co
        mesh.matrix_world = Matrix.Identity(4)
    points = [v.co.copy() for mesh in meshes for v in mesh.data.vertices]
    lo = Vector(tuple(min(v[k] for v in points) for k in range(3)))
    hi = Vector(tuple(max(v[k] for v in points) for k in range(3)))
    center = (lo + hi) / 2
    for mesh in meshes:
        for vertex in mesh.data.vertices: vertex.co -= center
    bpy.ops.object.select_all(action='DESELECT')
    for mesh in meshes: mesh.select_set(True)
    bpy.context.view_layer.objects.active = meshes[0]
    bpy.ops.object.join()
    mesh = bpy.context.object
    mesh.name = name.title()
    bpy.ops.export_scene.gltf(filepath=str(OUTPUT / f'{name}.glb'), export_format='GLB', use_selection=True,
                              export_animations=False, export_draco_mesh_compression_enable=False,
                              export_image_format='AUTO')
    # Blender +Y becomes glTF -Z. A socket near the barrel tip is editable in
    # the generated scene, independently of the visual mesh.
    extent = (hi - lo) / 2
    manifest[name] = {'size': [extent.x * 2, extent.z * 2, extent.y * 2],
                      'muzzle': [0.0, extent.z * 0.6, -extent.y],
                      'source': f'../blenderkit/{name}/source.json'}
(OUTPUT / 'sockets.json').write_text(json.dumps(manifest, indent=2) + '\n')
