"""Retarget KayKit strafe/backward clips onto the Citrus mannequins (test run).

Run: blender --background --factory-startup --disable-autoexec --python tools/retarget_kaykit.py
Outputs to /tmp/kaykit_retarget so the working mannequin GLBs are untouched.
"""
from pathlib import Path
import math
import bpy
from mathutils import Matrix, Vector

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "animations/kaykit/Rig_Medium/Rig_Medium_MovementAdvanced.glb"
OUTPUT = Path("/tmp/kaykit_retarget")
OUTPUT.mkdir(parents=True, exist_ok=True)

CORE = {
    "hips": "DEF-spine",
    "spine": "DEF-spine.001",
    "chest": "DEF-spine.002",
    "head": "DEF-spine.006",
}
for side, suffix in (("l", "L"), ("r", "R")):
    CORE.update({
        f"upperarm.{side}": f"DEF-upper_arm.{suffix}",
        f"lowerarm.{side}": f"DEF-forearm.{suffix}",
        f"wrist.{side}": f"DEF-hand.{suffix}",
        f"hand.{side}": f"DEF-hand.{suffix}",
        f"upperleg.{side}": f"DEF-thigh.{suffix}",
        f"lowerleg.{side}": f"DEF-shin.{suffix}",
        f"foot.{side}": f"DEF-foot.{suffix}",
        f"toes.{side}": f"DEF-toe.{suffix}",
    })

CLIPS = {"Walking_Backwards", "Running_Strafe_Left", "Running_Strafe_Right", "T-Pose"}


def inverse_map():
    out = {dst: src for src, dst in CORE.items()}
    out["DEF-shoulder.L"] = "chest"
    out["DEF-shoulder.R"] = "chest"
    out["DEF-spine.003"] = "chest"
    out["DEF-spine.004"] = "head"
    out["DEF-spine.005"] = "head"
    for side, suffix in (("l", "L"), ("r", "R")):
        for limb, src in (("upper_arm", "upperarm"), ("forearm", "lowerarm"), ("thigh", "upperleg"), ("shin", "lowerleg")):
            out[f"DEF-{limb}.{suffix}.001"] = f"{src}.{side}"
        for n in range(1, 5):
            out[f"DEF-palm.{n:02}.{suffix}"] = f"hand.{side}"
        for finger in ("thumb", "f_index", "f_middle", "f_ring", "f_pinky"):
            for n in range(1, 4):
                out[f"DEF-{finger}.{n:02}.{suffix}"] = f"hand.{side}"
    return out


def prepare(variant):
    path = ROOT / f"models/avatars/blenderkit/{variant}/mannequin_{variant}.blend"
    bpy.ops.wm.open_mainfile(filepath=str(path), load_ui=False, use_scripts=False)
    original = next(o for o in bpy.data.objects if o.type == "ARMATURE")
    mesh = next(o for o in bpy.data.objects if o.type == "MESH" and o.vertex_groups)
    original.animation_data_clear()
    rest = {b.name: b.head_local.copy() for b in original.data.bones}
    tails = {b.name: b.tail_local.copy() for b in original.data.bones}
    mesh_world = mesh.matrix_world.copy()
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
    ratio = rest["DEF-spine"].z / source.data.bones["hips"].head_local.z
    heads = {}
    for bone in source.data.bones:
        if bone.name in CORE:
            heads[bone.name] = rest[CORE[bone.name]]
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
        new.length = max(bone.length * ratio, 0.005)
        new.matrix = Matrix.LocRotScale(heads[bone.name], source_rest[bone.name].to_quaternion(), Vector((1, 1, 1)))
    for bone in source.data.bones:
        if bone.parent:
            armature.edit_bones[bone.name].parent = armature.edit_bones[bone.parent.name]
    bpy.ops.object.mode_set(mode="OBJECT")
    inv = inverse_map()
    mesh.vertex_groups.clear()
    groups = {b.name: mesh.vertex_groups.new(name=b.name) for b in armature.bones}
    for vertex, influences in enumerate(weights):
        merged = {}
        for name, weight in influences:
            assert name in inv, name
            key = inv[name]
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
    bpy.context.scene.render.fps = 30
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
                bone.location = src.location * ratio if bone.name == "hips" else (0, 0, 0)
                bone.scale = (1, 1, 1)
                bone.keyframe_insert("rotation_quaternion", frame=frame)
                if bone.name == "hips":
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
