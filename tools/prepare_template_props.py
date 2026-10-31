"""Convert BlenderKit prop exports to Citrus-ready GLBs.

blender --background --factory-startup --disable-autoexec --python tools/prepare_template_props.py

Re-encodes WebP textures to PNG (Citrus's gltf::import rejects EXT_texture_webp)
and exports plain glTF without Draco.
"""
from pathlib import Path
import json
import bpy

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / 'models/props/default'
OUT.mkdir(parents=True, exist_ok=True)
PROPS = [
    ('crate', 'wooden_crate.glb'),
    ('tree', 'tree.glb'),
    ('base', 'base_building.glb'),
    ('lamp', 'lamp_post.glb'),
    ('metal_crate', 'metal_crate.glb'),
    ('treasure', 'treasure_chest.glb'),
    ('table', 'table.glb'),
    ('wall_light', 'wall_light.glb'),
    ('ball', 'ball.glb'),
]
manifest = {}
for name, out_name in PROPS:
    src = ROOT / f'models/props/blenderkit/{name}/{name}.glb'
    bpy.ops.wm.read_factory_settings(use_empty=True)
    bpy.ops.import_scene.gltf(filepath=str(src))
    for image in bpy.data.images:
        if image.size[0] == 0:
            continue
        w, h = image.size
        maxdim = 1024
        if max(w, h) > maxdim:
            s = maxdim / max(w, h)
            image.scale(int(w * s), int(h * s))
        tmp = str(Path(bpy.app.tempdir) / f'{image.name}.png')
        image.filepath_raw = tmp
        image.file_format = 'PNG'
        image.save()
        image.pack()
    bpy.ops.object.select_all(action='SELECT')
    bpy.ops.export_scene.gltf(
        filepath=str(OUT / out_name),
        export_format='GLB',
        use_selection=True,
        export_animations=False,
        export_draco_mesh_compression_enable=False,
        export_image_format='AUTO',
    )
    manifest[name] = {'source': f'../blenderkit/{name}/source.json'}
(OUT / 'props.json').write_text(json.dumps(manifest, indent=2) + '\n')
