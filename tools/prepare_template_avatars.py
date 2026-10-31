"""Bake the downloaded mannequin meshes onto Quaternius's animation skeleton.

Run: blender --background --factory-startup --disable-autoexec --python tools/prepare_template_avatars.py
Original assets remain untouched. Output uses a small humanoid skeleton with
named in-place clips, shared by both mannequin variants. Blender 5.2+.
"""

from pathlib import Path
import json
import math
import bpy
from mathutils import Matrix, Vector

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "animations/quaternius/universal-animation-library/UAL1_Standard.glb"
OUTPUT = ROOT / "models/avatars/default"
OUTPUT.mkdir(parents=True, exist_ok=True)
CORE = {
    "pelvis": "DEF-spine", "spine_01": "DEF-spine.001",
    "spine_02": "DEF-spine.002", "spine_03": "DEF-spine.003",
    "neck_01": "DEF-spine.004", "Head": "DEF-spine.006",
}
for side, suffix in (("l", "L"), ("r", "R")):
    for src, dst in (("clavicle", "shoulder"), ("upperarm", "upper_arm"),
                     ("lowerarm", "forearm"), ("hand", "hand"),
                     ("thigh", "thigh"), ("calf", "shin"),
                     ("foot", "foot"), ("ball", "toe")):
        CORE[f"{src}_{side}"] = f"DEF-{dst}.{suffix}"
    for finger in ("thumb", "index", "middle", "ring", "pinky"):
        prefix = "thumb" if finger == "thumb" else "f_" + finger
        for part in (1, 2, 3):
            CORE[f"{finger}_{part:02}_{side}"] = f"DEF-{prefix}.{part:02}.{suffix}"

# Only clips useful to the current starter projects; keep upstream names.
CLIPS = {
    "Idle_Loop", "Walk_Loop", "Jog_Fwd_Loop", "Sprint_Loop",
    "Crouch_Idle_Loop", "Crouch_Fwd_Loop", "Jump_Start", "Jump_Loop", "Jump_Land",
    "Pistol_Idle_Loop", "Pistol_Aim_Neutral", "Pistol_Reload", "Pistol_Shoot",
    "Death01", "Hit_Chest", "Interact", "PickUp_Table", "Swim_Fwd_Loop", "Swim_Idle_Loop",
}

def prepare(variant):
    path = ROOT / f"models/avatars/blenderkit/{variant}/mannequin_{variant}.blend"
    bpy.ops.wm.open_mainfile(filepath=str(path), load_ui=False, use_scripts=False)
    original = next(o for o in bpy.data.objects if o.type == "ARMATURE")
    mesh = next(o for o in bpy.data.objects if o.type == "MESH" and o.vertex_groups)
    original.animation_data_clear()
    rest = {b.name: b.head_local.copy() for b in original.data.bones}
    tails = {b.name: b.tail_local.copy() for b in original.data.bones}
    mesh_world = mesh.matrix_world.copy()
    # Capture existing weights before removing the Rigify modifier and controls.
    group_names = {g.index: g.name for g in mesh.vertex_groups}
    weights = [[(group_names[g.group], g.weight) for g in v.groups] for v in mesh.data.vertices]
    mesh.modifiers.clear()
    mesh.parent = None
    mesh.matrix_world = mesh_world
    for o in list(bpy.data.objects):
        if o != mesh:
            bpy.data.objects.remove(o, do_unlink=True)
    bpy.context.scene.render.fps = 30
    old_actions = set(bpy.data.actions)
    bpy.ops.import_scene.gltf(filepath=str(SOURCE))
    source = next(o for o in bpy.data.objects if o.type == "ARMATURE")
    source_actions = [a for a in bpy.data.actions if a not in old_actions and a.name in CLIPS]
    assert len(source_actions) == len(CLIPS), [a.name for a in source_actions]
    source.animation_data_clear()
    source_rest = {b.name: b.matrix_local.copy() for b in source.data.bones}
    ratio = rest["DEF-spine"].z / source.data.bones["pelvis"].head_local.z
    heads = {}
    for bone in source.data.bones:
        if bone.name in CORE:
            heads[bone.name] = rest[CORE[bone.name]]
        elif "_04_leaf_" in bone.name:
            mapped = CORE[bone.name.replace("_04_leaf_", "_03_")]
            heads[bone.name] = tails[mapped]
        elif "ball_leaf_" in bone.name:
            heads[bone.name] = tails[CORE[bone.name.replace("ball_leaf_", "ball_")]]
        else:
            heads[bone.name] = bone.head_local * ratio

    armature = bpy.data.armatures.new("Citrus Humanoid")
    target = bpy.data.objects.new("Citrus Humanoid", armature)
    bpy.context.collection.objects.link(target)
    bpy.context.view_layer.objects.active = target
    target.select_set(True)
    bpy.ops.object.mode_set(mode="EDIT")
    for bone in source.data.bones:
        new = armature.edit_bones.new(bone.name)
        # A new EditBone has zero length; give it an axis before assigning a
        # matrix, otherwise Blender loses the rotation and creates Y-up bones.
        new.length = max(bone.length * ratio, 0.005)
        new.matrix = Matrix.LocRotScale(heads[bone.name], source_rest[bone.name].to_quaternion(), Vector((1, 1, 1)))
    for bone in source.data.bones:
        if bone.parent:
            armature.edit_bones[bone.name].parent = armature.edit_bones[bone.parent.name]
    bpy.ops.object.mode_set(mode="OBJECT")
    inverse_map = {dst: src for src, dst in CORE.items()}
    inverse_map["DEF-spine.005"] = "neck_01"
    for side, suffix in (("l", "L"), ("r", "R")):
        for limb, src in (("upper_arm", "upperarm"), ("forearm", "lowerarm"), ("thigh", "thigh"), ("shin", "calf")):
            inverse_map[f"DEF-{limb}.{suffix}.001"] = f"{src}_{side}"
        for n in range(1, 5):
            inverse_map[f"DEF-palm.{n:02}.{suffix}"] = f"hand_{side}"
    mesh.vertex_groups.clear()
    groups = {b.name: mesh.vertex_groups.new(name=b.name) for b in armature.bones}
    for vertex, influences in enumerate(weights):
        merged = {}
        for name, weight in influences:
            assert name in inverse_map, name
            key = inverse_map[name]
            merged[key] = merged.get(key, 0.0) + weight
        total = sum(merged.values())
        for name, weight in merged.items():
            groups[name].add([vertex], weight / total, "REPLACE")
    modifier = mesh.modifiers.new("Humanoid Skin", "ARMATURE")
    modifier.object = target
    mesh.parent = target
    mesh.name = f"Citrus {variant.title()}"
    mesh.data.name = mesh.name
    target.animation_data_create()
    source.animation_data_create()
    fps = 30
    bpy.context.scene.render.fps = fps
    # Bake source pose rotations onto the target's matching rest orientations.
    # Local translations stay at the target proportions; pelvis motion is scaled.
    for action in sorted(source_actions, key=lambda a: a.name):
        source.animation_data.action = action
        if action.slots:
            source.animation_data.action_slot = action.slots[0]
        baked = bpy.data.actions.new(action.name + "_Citrus")
        target.animation_data.action = baked
        start, end = action.frame_range
        length = max(1, math.ceil(end - start))
        for frame in range(length + 1):
            bpy.context.scene.frame_set(int(start + frame), subframe=(start + frame) % 1.0)
            for bone in target.pose.bones:
                bone.rotation_mode = "QUATERNION"
                src = source.pose.bones[bone.name]
                bone.rotation_quaternion = src.matrix_basis.to_quaternion()
                bone.location = src.location * ratio if bone.name == "pelvis" else (0, 0, 0)
                bone.scale = (1, 1, 1)
                bone.keyframe_insert("rotation_quaternion", frame=frame)
                if bone.name == "pelvis":
                    bone.keyframe_insert("location", frame=frame)
        target.animation_data.action = None
        track = target.animation_data.nla_tracks.new()
        track.name = action.name
        strip = track.strips.new(action.name, 0, baked)
        strip.name = action.name
        strip.action_frame_start = 0
        strip.action_frame_end = length
        track.mute = True
    for o in list(bpy.data.objects):
        if o not in (target, mesh):
            bpy.data.objects.remove(o, do_unlink=True)
    target.animation_data.action = None
    for bone in target.pose.bones:
        bone.matrix_basis = Matrix.Identity(4)
    bpy.context.scene.frame_set(0)
    bpy.ops.object.select_all(action="DESELECT")
    target.select_set(True)
    mesh.select_set(True)
    bpy.context.view_layer.objects.active = target
    for track in target.animation_data.nla_tracks:
        track.mute = False
    bpy.ops.export_scene.gltf(
        filepath=str(OUTPUT / f"{variant}.glb"), export_format="GLB",
        use_selection=True, export_animations=True, export_animation_mode="NLA_TRACKS",
        export_force_sampling=True, export_frame_range=False, export_anim_slide_to_zero=True,
        export_skins=True, export_yup=True, export_cameras=False, export_lights=False,
    )
    print("PREPARED", variant, len(armature.bones), "bones", len(source_actions), "clips", flush=True)

for variant in ("male", "female"):
    prepare(variant)
