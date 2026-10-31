# Default template assets

Working inventory and replacement checklist, checked 2026-10-31 against
`crates/citrus-engine/src/templates/`. BlenderKit catalogue candidates below
have not yet passed visual, deformation or in-engine material validation.

**Status update (2026-10-31, later same day):** mannequins and weapons are now
integrated in code (see sections below) and pass `cargo test -p citrus-engine
templates::tests`. Prepared weapon GLBs were re-exported with PNG textures
instead of embedded WebP, which `EXT_texture_webp` made `gltf::import` (used by
Citrus) refuse to load; a `weapon_models_import_in_citrus` test now imports all
three through the real Citrus loader. Muzzle position and forward sign were
visually confirmed with an offline Blender render (all three weapons' barrels
point in the engine's -Z forward convention). Hand-to-grip attachment is still
not implemented: weapons are positioned near the body/camera but are not
parented to or posed by a hand bone, so they visibly float. This remains open
work, not a regression from this pass.

## Templates and required art

| Template | Required assets |
| --- | --- |
| Empty | Keep a deliberate camera/cube/ground starter; no decorative models required. |
| FPS | Male/female player body, first-person hands/arms, rifle, muzzle/impact effects, target, crates, cover, arena floor/walls/ramp. |
| TPS | Same player bodies and rifle; locomotion, jumping, turning and aiming; shared shooter arena kit. |
| Top-down | Both animated mannequins, walking/running/turning, gate, lever/switch, lamp post, rocks, treasure, courtyard walls and ground. |
| RTS | Both animated mannequins as units, walking/running/turning, team tint, base building, tree variants, ore/resource node, ground. |
| VR Shooter | Both inhabitable avatars, tracked hands, pistol, blaster, bench, targets, throwable props, range/backstop. |
| VR Social | Both inhabitable avatars, hands, table, lights, throwable props, helmet/display prop, room kit; optional sofa. |

The existing template objects are primarily **Rust-authored primitive placeholders**.
There is no reliable provenance record proving that every old imported asset is
AI-generated. Track known replacements individually rather than deleting all
existing art. The DamagedHelmet sample and existing texture files need their own
provenance checks before deciding to replace them.

## BlenderKit shortlist

P0 = player/weapon essentials; P1 = current template scenery; P2 = optional polish.
Counts below are **catalogue-reported faces**, not measured exported triangles.
Materials/procedural geometry require baking to portable PBR textures/meshes.
Every row remains a candidate until checked; neither a paid subscription nor a
catalogue listing proves human authorship or game-ready export quality.

| Asset | Templates | Candidate | Faces | Access | License |
| --- | --- | --- | ---: | --- | --- |
| P0 Rifle | FPS, TPS | [Low poly TR21 assault rifle](https://www.blenderkit.com/asset-gallery-detail/d859aa94-2e31-4784-aa16-a307a936e2bc/) | 4787 | Full plan | Royalty Free |
| P0 Pistol | VR Shooter | [P320 Handgun](https://www.blenderkit.com/asset-gallery-detail/78f8c399-3f03-4ea8-b58f-da40b42c6b7f/) | 1998 | Free | Royalty Free |
| P1 Blaster | VR Shooter | [Sci-Fi Energy Blaster Gun](https://www.blenderkit.com/asset-gallery-detail/fd4a9a84-ed5c-4220-ae37-79f8a684fce7/) | 3332 | Free | Royalty Free |
| P0 Rigged hands | FPS, VR | [Rigged Hand For CGI](https://www.blenderkit.com/asset-gallery-detail/cf4b4bbb-486a-4cc6-8e7b-41f58e27a76f/) | 1772 | Full plan | Royalty Free |
| P0 Wooden crate | FPS, TPS | [Wooden Military Crate](https://www.blenderkit.com/asset-gallery-detail/ffae7880-9e4f-42e0-a61e-d0a5e1a1c766/) | 11541 | Free | CC0 |
| P1 Metal crate | FPS, TPS | [Old Military Crate](https://www.blenderkit.com/asset-gallery-detail/10fa7d45-e4b0-465b-9e0c-aca47991f00c/) | 15936 | Free | CC0 |
| P0 Concrete cover | FPS, TPS | [Damaged Concrete Barrier](https://www.blenderkit.com/asset-gallery-detail/93bfa213-a008-40ad-a805-504914e5b24a/) | 687 | Full plan | Royalty Free |
| P1 Gun bench | VR Shooter | [Work Bench](https://www.blenderkit.com/asset-gallery-detail/17754f9b-0fa5-4124-8375-d3e4d066c648/) | 876 | Full plan | Royalty Free |
| P1 Target stand | Shooters; adapt to firearm target | [Archery straw target](https://www.blenderkit.com/asset-gallery-detail/40c57767-107b-4b80-96a9-dbca2dd9d627/) | 6468 | Free | Royalty Free |
| P1 Lever | Top-down | [Factory lever switch](https://www.blenderkit.com/asset-gallery-detail/6451e732-fde3-4493-ab56-299f3b0c892a/) | 8427 | Full plan | Royalty Free |
| P1 Gate | Top-down | [Modern Metal Gate](https://www.blenderkit.com/asset-gallery-detail/66e3bb3c-14d6-4d1a-a853-f80ada095201/) | 9786 | Full plan | Royalty Free |
| P1 Lamp post | Top-down | [Classic Single Street Lamp](https://www.blenderkit.com/asset-gallery-detail/59a65814-9c15-4595-adab-e69e31396fe7/) | 717 | Free | Royalty Free |
| P1 Rock variants | Top-down, RTS | [Low Poly Rock](https://www.blenderkit.com/asset-gallery-detail/d4bc0e5f-4628-4ff2-865b-ce6737fb8e96/) | 4964 | Full plan | Royalty Free |
| P1 Treasure chest | Top-down | [Wooden Treasure Chest](https://www.blenderkit.com/asset-gallery-detail/9b31aaae-e983-4b0e-954b-9b2ef880fd14/) | 8729 | Free | Royalty Free |
| P1 Tree | RTS, top-down | [Low poly tree](https://www.blenderkit.com/asset-gallery-detail/346d99d8-36fb-4f9d-a4a1-2f9ad9233e79/) | 537 | Free | Royalty Free |
| P1 Base building | RTS | [Low poly house](https://www.blenderkit.com/asset-gallery-detail/21c30fc8-4bf2-421b-aaf5-45329a7aea67/) | 4446 | Free | Royalty Free |
| P1 Table | VR Social | [Bistro Wood Table](https://www.blenderkit.com/asset-gallery-detail/a156684f-3298-4548-b36d-c1795893bd20/) | 8354 | Free | Royalty Free |
| P2 Sofa | VR Social, scene demos | [Low - Poly Sofa 1](https://www.blenderkit.com/asset-gallery-detail/da429d01-94f6-4866-b7e6-18a9c99b23b0/) | 2240 | Full plan | Royalty Free |
| P1 Wall light | VR Social | [Modern Facade Sconce](https://www.blenderkit.com/asset-gallery-detail/7a9fff83-b2f4-4aeb-ba6c-de5ac3d6c0ef/) | 1185 | Free | Royalty Free |
| P1 Throwable ball | VR templates | [Football (Soccer Ball)](https://www.blenderkit.com/asset-gallery-detail/bd288ef9-e19b-40a7-adc5-47aca6f18d6d/) | 2160 | Free | Royalty Free |
| P2 Barrel | Shooters | [Wasteland Oil Barrel](https://www.blenderkit.com/asset-gallery-detail/b749913f-7d22-4e79-bc52-50b75e341ea0/) | 2573 | Full plan | Royalty Free |
| P1 Room walls | VR Social | [Modular office walls](https://www.blenderkit.com/asset-gallery-detail/d6defc31-e193-4134-976b-f348392d784f/) | 888 | Full plan | Royalty Free |
| P1 Concrete PBR | Shooters | [Concrete floor or wall](https://www.blenderkit.com/asset-gallery-detail/8c46499f-fa02-4f51-9e9d-346fae88e2d3/) | Check | Free | Royalty Free |
| P1 Wood PBR | VR Social | [Wood Floor](https://www.blenderkit.com/asset-gallery-detail/16e334c2-9af0-425b-bfac-1032c7cc2e94/) | Check | Free | Royalty Free |
| P1 Ground PBR | Top-down, RTS | [Grass Ground](https://www.blenderkit.com/asset-gallery-detail/d31de4c9-c5de-483f-8744-e8aa69740fc4/) | Check | Free | Royalty Free |
| P1 Sky HDRI | Outdoor templates | [Cloudy Sky HDRI](https://www.blenderkit.com/asset-gallery-detail/34fd406a-9f84-4e6f-913f-9cef65f99c70/) | Check | Free | Royalty Free |

Still to source: a proper firearm target with a base pivot (the straw target
above is only a fallback), ore/resource node, a consistent wall/ramp/platform
kit, first-person arms that match the mannequins, and a lightweight helmet.
Prefer variants from the same creator/material family for a consistent style.

## Animation sources and required clips

Primary source: [Quaternius Universal Animation Library](https://quaternius.itch.io/universal-animation-library),
CC0, including an in-place export. The free **Standard** archive is downloaded
under `animations/quaternius/universal-animation-library/`; it contains **43**
clips, not all clips advertised across the complete product tiers.

| Use | Required clips / behavior |
| --- | --- |
| Shared minimum | Idle, walk forward, jog/run, sprint, walk-to-idle blending. |
| Top-down / RTS | Smooth turn while moving; left/right direction reversal; turn-in-place or pivot when stopped. |
| TPS | Forward/backward/left/right locomotion, diagonals, aim locomotion, jump start/air/land, crouch idle/walk, left/right turns and 180-degree reversal. |
| Combat | Rifle/pistol idle, fire, reload; hit reaction and death (where supported). |
| VR | Head/hand tracking + humanoid IK takes priority over canned upper-body animation; optional lower-body locomotion. |

The Standard pack has `Idle_Loop`, `Walk_Loop`, `Jog_Fwd_Loop`, `Sprint_Loop`,
`Crouch_Idle_Loop`, `Crouch_Fwd_Loop`, `Jump_Start`, `Jump_Loop`, `Jump_Land`,
pistol clips, interactions and death/hit reactions. It does **not** include
separate strafing or turn-in-place clips. Do not relabel ordinary walking as a
captured turn animation. See [Library 2](https://quaternius.itch.io/universal-animation-library-2)
for additional movement, and [Mixamo](https://www.mixamo.com/) for individual
walk, strafe and turning clips. Adobe describes Mixamo as free with an Adobe ID
and royalty-free for games in its [FAQ](https://helpx.adobe.com/creative-cloud/faq/mixamo-faq.html).
Review editable-asset redistribution separately before bundling Mixamo downloads.

**Decision (2026-10-31): kept the free Standard pack, did not acquire a new
animation source this pass.** `TemplateAvatar` already computes and exposes
`Forward`/`Right`/`Turn` Animator parameters from actual root motion
(`crates/citrus-core/src/kit/avatar.rs`). The default `.animctrl`
(`crates/citrus-engine/src/templates/avatar.rs::controller`) now blends the
Locomotion and Crouch states on a genuine 2D blend tree keyed on `Right`
(x) and `Forward` (y) instead of raw planar speed, using the Animator's
existing 2D blend-tree support (`BlendTree::parameter_y`, previously unused).
All motions sit on the `Right = 0` axis because only forward-facing loops
exist, so this change does not add strafing — it fixes a real bug where pure
sideways movement used to drive a forward walk/jog/sprint pose off total
planar speed, visibly sliding the feet forward while the character moved
sideways. Now lateral-only motion blends toward Idle instead. `Turn` is still
computed and exposed but **not consumed by any motion**: there is no
turn-in-place, pivot or strafe clip in either downloaded Quaternius library to
blend to, and reusing an unrelated clip (e.g. speeding up/slowing the forward
walk cycle) would misrepresent actual turning as a captured animation. Fixing
this for real needs either a licensed/Mixamo clip set retargeted to both
mannequins, or a deliberate procedural turn-in-place/pivot system built on top
of the existing Animator (e.g. an additive spine-lean layer) — both are
nontrivial follow-up work, not done here.

Mannequins and library animations use different skeletons. Retarget to **both**
characters, bake deform-bone keys, preserve foot contact and clip timing, and
export named clips. Use in-place locomotion: CharacterController/NavAgent owns
translation; turning must follow the actual facing changes, including sudden
left/right commands. Animation availability alone does not prove integration.

## Acquisition and replacement checklist

- [x] Download male and female mannequins as `.blend` and `.glb`, with provenance/hashes.
- [x] Download and verify Universal Animation Library Standard; retain its CC0 license.
- [x] Inventory all seven templates and identify candidate props/materials.
- [x] Inspect/re-export mannequins, remove unused control bones and retarget motion.
- [x] Provide one editable male/female selection shared across gameplay templates.
- [x] Make the chosen avatar inhabit the FPS and VR rigs, preserving tracking and owner visibility.
- [x] Replace gun meshes while preserving muzzle sockets and gameplay roots. VR
      Shooter already attaches the pistol/blaster to the VR hand correctly via
      the existing `Grabbable { snap_to_hand: true }` + hand-IK system (no new
      code needed there). FPS/TPS still just offset the rifle from the
      camera/player root with no hand bone or IK driving a grip pose, so it
      visibly floats beside the body; this needs either per-clip static hand
      offsets or real two-bone arm IK onto the weapon grip, not done this pass.
- [ ] Add shared locomotion/turning animation setup for TPS, top-down and RTS (Forward/Right directional blend done; dedicated turning/strafe clips not sourced, see Animation sources above).
- [ ] Replace current scenery in small groups, retaining colliders and interactions.
- [x] Verify each template scaffolds, loads and runs (`cargo test -p citrus-engine templates::tests`); desktop/VR fallback and multiplayer avatar selection still need manual/headset verification.
- [ ] Visually check both genders, foot sliding, reversals, weapon grips, mirrors and VR body fit (muzzle position/forward sign checked by offline render; hand-grip attachment, in-headset VR fit and live reversal/sliding checks are not done). A real first-person bug was found and fixed by code review: FPS and VR Shooter's head cameras sat inside the avatar's own head model with no layer exclusion, so the player would see the inside of their own head mesh; both cameras now exclude `CHARACTER_LAYER` via a new `Scene::hide_own_body_from_camera`, covered by `first_person_cameras_exclude_the_avatar_body_layer`. TPS intentionally keeps the body layer visible (third person). This is a headless-test-verified fix, not an in-headset/GPU visual confirmation.

Keep stable gameplay roots and put replaceable visuals under them: changing a
model must not change movement, targeting, health, grabbing or networking logic.
First-person owner visibility must still allow other players and mirrors to see
an avatar where intended. Keep UI, markers and collision primitives engine-native.

## Asset acceptance

Target glTF 2.0 binary with embedded textures, metre units, explicit forward/up
orientation, meaningful pivots, portable metallic/roughness PBR and reasonable
material/triangle counts. Check exported triangles instead of trusting “low poly”
titles; several search results had huge render subdivision counts. Record source,
author, upstream asset version, license, hashes, conversion steps and measured
export statistics beside every acquired asset. Verify authorship from creator
information/process where available; do not treat AI-checked description metadata
as proof of how a mesh was made.

BlenderKit **Royalty Free** candidates can be evaluated locally. Shipping editable
models with engine templates needs suitable redistribution rights. CC0 candidates
are simpler for default downloadable template content. See BlenderKit's
[license](https://www.blenderkit.com/docs/licenses/) and
[licensing FAQ](https://www.blenderkit.com/docs/licenses/licensing-faq/).
