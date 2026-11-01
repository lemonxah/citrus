"""Bake mannequin meshes onto the Quaternius VRM skeleton, plus KayKit strafes.

Run: blender --background --factory-startup --disable-autoexec --python tools/prepare_template_avatars_full.py
"""
from pathlib import Path
import math
import bpy
from bpy_extras import anim_utils
from mathutils import Matrix, Quaternion, Vector

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "animations/quaternius/universal-animation-library/UAL1_Standard.glb"
KAYKIT = ROOT / "animations/kaykit/Rig_Medium/Rig_Medium_MovementAdvanced.glb"
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

CLIPS = {
    "Idle_Loop", "Walk_Loop", "Jog_Fwd_Loop", "Sprint_Loop",
    "Crouch_Idle_Loop", "Crouch_Fwd_Loop", "Jump_Start", "Jump_Loop", "Jump_Land",
    "Pistol_Idle_Loop", "Pistol_Aim_Neutral", "Pistol_Reload", "Pistol_Shoot",
    "Death01", "Hit_Chest", "Interact", "PickUp_Table", "Swim_Fwd_Loop", "Swim_Idle_Loop",
}

KAY_TO_VRM = {
    "hips": "pelvis", "spine": "spine_01", "chest": "spine_02", "head": "Head",
    "upperarm.l": "upperarm_l", "lowerarm.l": "lowerarm_l", "hand.l": "hand_l",
    "upperleg.l": "thigh_l", "lowerleg.l": "calf_l", "foot.l": "foot_l", "toes.l": "ball_l",
    "upperarm.r": "upperarm_r", "lowerarm.r": "lowerarm_r", "hand.r": "hand_r",
    "upperleg.r": "thigh_r", "lowerleg.r": "calf_r", "foot.r": "foot_r", "toes.r": "ball_r",
}
KAY_CLIPS = {"Running_Strafe_Left", "Running_Strafe_Right", "Walking_Backwards"}
# KayKit's chibi rig holds its arms up and out; the mannequin's arms come from
# the matching Quaternius gait at the same phase instead.
ARM_DONOR = {"Running_Strafe_Left": "Jog_Fwd_Loop", "Running_Strafe_Right": "Jog_Fwd_Loop", "Walking_Backwards": "Walk_Loop"}
ARM_PREFIXES = ("clavicle_", "upperarm_", "lowerarm_", "hand_", "thumb_", "index_", "middle_", "ring_", "pinky_")


def donor_rotation(action, bone, frame):
    bag = anim_utils.action_get_channelbag_for_slot(action, action.slots[0])
    path = f'pose.bones["{bone}"].rotation_quaternion'
    curves = [bag.fcurves.find(path, index=i) for i in range(4)]
    if any(c is None for c in curves):
        return None
    return Quaternion([c.evaluate(frame) for c in curves])


def prepare(variant):
    path = ROOT / f"models/avatars/blenderkit/{variant}/mannequin_{variant}.blend"
    bpy.ops.wm.open_mainfile(filepath=str(path), load_ui=False, use_scripts=False)
    original = next(o for o in bpy.data.objects if o.type == "ARMATURE")
    mesh = next(o for o in bpy.data.objects if o.type == "MESH" and o.vertex_groups)
    original.animation_data_clear()
    rest = {b.name: b.head_local.copy() for b in original.data.bones}
    tails = {b.name: b.tail_local.copy() for b in original.data.bones}
    mesh_world = mesh.matrix_world.copy()
    # The BlenderKit mesh models one side and builds the other with a Mirror
    # modifier; apply it (mirroring the .L/.R groups) before the stack is cleared.
    for mod in [m for m in mesh.modifiers if m.type == "MIRROR"]:
        with bpy.context.temp_override(object=mesh, active_object=mesh):
            bpy.ops.object.modifier_apply(modifier=mod.name)
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
            heads[bone.name] = tails[CORE[bone.name.replace("_04_leaf_", "_03_")]]
        elif "ball_leaf_" in bone.name:
            heads[bone.name] = tails[CORE[bone.name.replace("ball_leaf_", "ball_")]]
        else:
            heads[bone.name] = bone.head_local * ratio

    armature = bpy.data.armatures.new("ZestEngine Humanoid")
    target = bpy.data.objects.new("ZestEngine Humanoid", armature)
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
    mesh.name = f"ZestEngine {variant.title()}"
    mesh.data.name = mesh.name
    target.animation_data_create()
    source.animation_data_create()
    fps = 30
    bpy.context.scene.render.fps = fps
    for action in sorted(source_actions, key=lambda a: a.name):
        source.animation_data.action = action
        if action.slots:
            source.animation_data.action_slot = action.slots[0]
        baked = bpy.data.actions.new(action.name + "_ZestEngine")
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

    donor_actions = {a.name: a for a in source_actions}
    kay_old = set(bpy.data.actions)
    bpy.ops.import_scene.gltf(filepath=str(KAYKIT))
    kay_source = next(o for o in bpy.data.objects if o.type == "ARMATURE" and o not in (target, source))
    kay_actions = [a for a in bpy.data.actions if a not in kay_old and a.name in KAY_CLIPS]
    assert len(kay_actions) == len(KAY_CLIPS), [a.name for a in kay_actions]
    kay_source.animation_data_create()
    kay_rest = {b.name: b.matrix_local.to_quaternion() for b in kay_source.data.bones}
    tgt_rest = {b.name: b.bone.matrix_local for b in target.pose.bones}
    kay_ratio = rest["DEF-spine"].z / kay_source.data.bones["hips"].head_local.z
    for action in sorted(kay_actions, key=lambda a: a.name):
        kay_source.animation_data.action = action
        if action.slots:
            kay_source.animation_data.action_slot = action.slots[0]
        baked = bpy.data.actions.new(action.name + "_ZestEngine")
        target.animation_data.action = baked
        start, end = action.frame_range
        length = max(1, math.ceil(end - start))
        for frame in range(length + 1):
            bpy.context.scene.frame_set(int(start + frame), subframe=(start + frame) % 1.0)
            for kay_name, vrm_name in KAY_TO_VRM.items():
                if vrm_name not in target.pose.bones or kay_name not in kay_source.pose.bones:
                    continue
                if vrm_name.startswith(ARM_PREFIXES):
                    continue
                src_b = kay_source.pose.bones[kay_name]
                dst_b = target.pose.bones[vrm_name]
                r_src = src_b.matrix.to_quaternion()
                r_delta = r_src @ kay_rest[kay_name].inverted()
                r_t = r_delta @ tgt_rest[vrm_name].to_quaternion()
                if vrm_name == "pelvis":
                    src_delta = (src_b.matrix.to_translation() - src_b.bone.head_local) * kay_ratio
                    pos = tgt_rest[vrm_name].to_translation() + src_delta
                else:
                    # Rotation only: the bone stays attached where its parent put it.
                    pos = dst_b.matrix.to_translation()
                dst_b.matrix = Matrix.LocRotScale(pos, r_t, Vector((1, 1, 1)))
                # Children are solved against this bone's new pose, not a stale one.
                bpy.context.view_layer.update()
                dst_b.keyframe_insert("rotation_quaternion", frame=frame)
                if vrm_name == "pelvis":
                    dst_b.keyframe_insert("location", frame=frame)
            donor = donor_actions[ARM_DONOR[action.name]]
            d_start, d_end = donor.frame_range
            d_frame = d_start + (d_end - d_start) * frame / length
            for bone in target.pose.bones:
                if bone.name.startswith(ARM_PREFIXES):
                    q = donor_rotation(donor, bone.name, d_frame)
                    if q is not None:
                        bone.rotation_quaternion = q
                        bone.keyframe_insert("rotation_quaternion", frame=frame)
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
    print("PREPARED", variant, len(armature.bones), "bones",
          len(source_actions) + len(kay_actions), "clips", flush=True)


for variant in ("male", "female"):
    prepare(variant)
