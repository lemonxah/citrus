# citrus — Bugs & stability

Open bugs, verify-after-rebuild items, and crash/stability tracking. Feature status
and goals live in [Features.md](Features.md); working list + design notes in
[TODO.md](TODO.md).

Legend: `[ ]` open · `[verify]` believed fixed, confirm after a rebuild · `[x]` fixed

## Open / verify

- [x] **Flux: emissive object showed a coloured band from the emitter behind it** — emissive
  surfaces were excluded from the Flux depth prepass, so their pixels reconstructed the surface
  BEHIND them and sampled that (a magenta floor behind a green box bled onto the box face).
  Fixed by putting emissive back in the depth prepass (they sample their own GI; received GI is
  additive on top of emission). Confirmed.
- [x] **Flux: emitter glow worked off-screen but vanished when the emitter came on-screen** —
  the screen-space NEE occlusion trace false-shadowed at grazing angles (ray skims the receiver
  floor) only while the emitter was on-screen. Removed the screen-space occlusion entirely; the
  emitter NEE now has NO occlusion (a soft cosine terminator handles back-faces, emissive-in-
  depth-prepass handles the behind-GI). Trade-off: an emitter's direct glow can pass through a
  solid (proper fix = surface/radiance cache, tracked). Confirmed gone.
- [known] **Firefly speckles on the floor when a light is near the transparent window** — the
  static transparent window sits in the GDF as a solid occluder, so bounce rays hit it right
  next to the point light → inverse-square blowup → fireflies. Fix (pending): exclude transparent
  from the GDF (glass shouldn't occlude GI) and/or clamp the near-light attenuation in the Flux
  direct term.
- [x] **Screen-space GI: fixed-pattern blotches that never converged** — the trace RNG seed
  (`screen_gi.comp`, `hash(... + pc.misc.x * 26699u)`) had `misc.x` hardcoded to `0`, so every
  frame traced the identical random directions per pixel → the Monte-Carlo noise was a fixed
  pattern and temporal accumulation was blending a value against itself (no convergence).
  Fixed: a monotonic `sgi_frame` counter increments each trace and feeds `misc.x`, so each frame
  samples different directions and the noise averages away within a few frames (and reprojects
  cleanly while moving). Confirmed visually.
- [x] **Screen-space GI: square halo + voxel banding around static emissive objects** — static
  emitters were baked into the GDF, so the hemisphere bounce hit their coarse padded SDF box
  (square edge) and the 128³ voxel lattice (banding); dynamic emitters dodged it by being
  excluded from the field. Fixed: emissive instances are excluded from the GDF regardless of the
  static flag (`realtime_gi.rs` gdf_insts filter) — their light still reaches the scene via the
  analytic NEE spheres. A static emitter now looks identical to a dynamic one. Confirmed visually.
- [verify] **Realtime GI tanked framerate in Play mode (≈1200→150 fps, same 7 draw calls)** —
  the editor runs GI every frame too but a static scene *settles* and stops tracing; in Play,
  physics nudges/jitters bodies every frame so the GI input hash changed continuously and it
  re-traced (march + 6-iter blur + probe upload) forever. Three fixes:
  1. The GI input hash (`hash_inputs`) now **ignores dynamic, non-emissive objects** — a prop
     falling/resting under physics no longer forces a re-trace (it's not in the static field and
     casts no light; it still *receives* GI via the fixed probe volume). Static geometry and
     moving emitters still drive re-traces.
  2. Transforms in the hash are **quantized to ~1mm** so a resting body's solver jitter doesn't
     re-trace.
  3. The 64³ Global Distance Field (`build_gdf`) is built from **static geometry only**
     (`SdfInstance::static_geometry`), so dynamic motion never triggers the (heavy) rebuild.
  Net: a Play-mode scene settles and idles like the editor. Mark level geometry "Contribute GI"
  (static) so it occludes/bounces in the cached field.

  **Confirmed via per-section CPU log** (`play CPU: gi … components … physics … audio …`):
  components/physics/audio were ~0; the cost was **GI re-tracing in RayQuery (hardware) mode**
  — a 32³ = 32768-probe grid re-baked **synchronously on the main thread (~9 ms)** on most
  frames, because a GI contributor (a light, emissive object, or static-marked mesh) was being
  animated by a component every frame, so the GI hash never settled. The probe volume is
  scene-centered, so camera/player motion is *not* the trigger. Two more fixes:
  4. RayQuery's trace cadence is **floored to ~10 Hz** (`interval_floor` in `update`) so a
     continuously-animated GI input can't block every frame (software march stays floor-0 / async).
  5. The RayQuery realtime grid is **capped at 24³** (~13k probes) instead of 32³ — the grid
     blur keeps the soft look at lower res, each trace is ~2.4× cheaper.
  Recommendation: use **Software** GI mode for realtime (cheap GPU march, async, cascaded);
  RayQuery is best reserved for the offline bake.

- [verify] **Material inspector had no texture inputs** — the shader binds 12 sampler
  slots and imports show embedded textures, but `MaterialModel` carried no texture
  references so the inspector couldn't expose them. Added `MaterialTexturePaths` (12
  project-relative slots) to the model, a **Textures** collapsing section in the
  material editor with per-slot drag-drop (drop an image from the file browser) and
  clear buttons, a renderer `set_material_textures` that rebinds an existing
  material's descriptor set, and `apply_material` now resolves model paths (sRGB for
  colour slots, linear for data) falling back to import-embedded handles. Round-trips
  through `.material` files (`MaterialTextures`) and inline scene materials
  (`MaterialRef::Inline { textures }`, `#[serde(default)]` so old scenes still load).
  Imported materials stay read-only until extracted. Confirm assigning/clearing a
  texture updates the viewport after a rebuild.

- [verify] **Switching GI mode (Hardware ↔ Software) didn't update the viewport** —
  `hash_inputs` folded in lights/geometry/materials/settings but not `gi.mode`, so
  the software-SDF and hardware-ray-query backends hashed identically and the toggle
  never triggered a re-trace. `GiMode` now derives `Hash` and is hashed. Confirm the
  viewport re-traces when flipping the mode after a rebuild.

- [verify] **Realtime GI didn't re-trace on a material change** — editing/resetting a
  material, assigning a material, or editing a `.material` file (and the matching
  undo/redo) now calls `RealtimeGiState::invalidate()`, forcing the next trace even
  when the input hash doesn't catch the change. Emission/albedo are also folded into
  `hash_inputs` so emissive edits re-trace on their own. Confirm bounce light updates
  live after a rebuild.

- [x] **Scroll wheel dollied the camera through floating windows over the viewport** —
  fixed. Scroll-dolly was gated only on `pointer_in_viewport` (a winit rect test), which
  is true even when a floating egui window sits over the cursor inside the viewport rect,
  so the wheel bled through (clicks were already blocked via the occlusion-aware
  `response.hovered()`). Now scroll-dolly also requires `response.contains_pointer()`
  (occlusion-aware), so a window over the cursor swallows the wheel like it does clicks.

- [verify] Gizmo floated away from the selected object's pivot — fixed by projecting
  the gizmo against the full window rect (the swapchain) instead of the viewport tab
  rect; confirm anchored correctly after the rebuild.
- [verify] Right click in viewport sometimes did nothing instead of starting camera
  look — was gated on `egui_ctx.is_using_pointer()` at press time, which could be true
  spuriously; now the look starts from the viewport tab's own widget
  (`button_pressed(Secondary) && hovered`), so egui's hit-testing decides.

- [x] **First left interaction dead after mouse-look ends** (orbit, gizmo/widget drags;
  object *select* still worked) — fixed. While looking, window events are withheld from
  egui (`window_event` skips `on_window_event` when `looking`), so egui never saw the
  right-button RELEASE and still believed Secondary was held. With a button stuck down,
  egui never cleared `press_origin`, so the next left press didn't refresh it: a click
  (press+release at one spot) still registered — hence select worked — but drag detection
  measured from the stale origin and the `drag_started` edge never fired, killing orbit,
  gizmo handles, and widget drags until a second click. Scroll dolly was unaffected (it
  reads the fresh winit `pointer_in_viewport`). Fix: on `set_looking(false)`, set
  `look_just_ended`; the next frame injects an `egui::Event::PointerMoved(cursor)` plus a
  Secondary `PointerButton{pressed:false}` into raw input, clearing the stuck button and
  resyncing position so the next press starts a clean drag. (First attempt injected only
  the move; the stuck button was the real cause.)

- [x] **Add Component popup resized while typing in search** — fixed. The menu popup
  auto-sized to its content, so filtering (fewer/narrower matching buttons) shrank it.
  Fix: pinned the popup to a fixed, user-resizable size persisted in egui data
  (`set_min_width`/`set_max_width` + a constant-height `ScrollArea` via
  `auto_shrink([false; 2])` + `max_height`), so match count no longer changes the size.
  Added a bottom-right drag grip (clamped 180–600 x 120–600) to resize manually; rows are
  now full-width hit targets.

- [x] **Code editor didn't fill the dock when text was shorter than the viewport** —
  fixed. The TextEdit sized to its content. Now it's sized to at least the visible height
  (`available_height / row_height` rows), so the editable area reaches the bottom; longer
  files grow past it and scroll as before.
- [x] **Code editor selection hid the selected text** — fixed twice. First the
  selection rendered near-white (washed out the code); switching to a muted colour
  helped but an *opaque* fill still painted over the highlighted galley, so selected
  lines showed as a solid block with no glyphs. Now `visuals.selection.bg_fill` is a
  translucent purple (`rgba 130,100,215,110`), so the selected text stays visible
  through the highlight regardless of paint order.
- [x] **Orbit orbited a point "higher and offset" from its target (parented orbiter)**
  — fixed. `Orbit::update` set `*ctx.translation = target_world + offset`, but
  `ctx.translation` is the object's *local* TRS; for an orbiter nested under a parent
  the rendered position became `parent_world * (target_world + offset)` — displaced
  from the target. Added `ComponentCtx::set_world_position` (converts a world point to
  local via the parent's inverse world matrix, exposed through a new
  `ComponentCtx::parent_world` snapshot) and Orbit now uses it, so it circles the
  target whether the orbiter is a root or nested.
- [x] **No visual feedback when dragging a scene-tree object** — fixed. Dragging a row
  (to reparent or to drop into an ObjectRef box) showed nothing following the cursor.
  Now a floating "ghost" chip with the object's name is painted at the pointer on a
  top (Tooltip) layer while `row.dragged()` — no egui dnd context, so it's safe across
  the plugin egui boundary.
- [x] **calloop "Received an event for non-existence source" WARN spam during mouse-look**
  — benign winit/Wayland quirk (event sources churn under cursor-grab). Silenced by
  capping the `calloop` target at error level in the tracing subscriber (stays quiet even
  when RUST_LOG raises the global level).

- [x] **Code editor caret blinked away while being moved** — fixed. egui only resets the
  caret blink on its own edits, not on vim's programmatic cursor moves, so a motion could
  land mid-blink-off and the caret vanished. Now any vim keystroke stamps an activity time
  and the caret is forced solid (`visuals.text_cursor.blink = false`) for ~0.6s after, so
  it stays visible while moving; egui resumes blinking when idle.

- [x] **Escape in the code editor deselected / dropped focus instead of going to vim
  Normal** — fixed. egui's `TextEdit` surrenders focus on Escape using a value computed at
  frame start (so swallowing the event doesn't stop it); the field lost focus and Escape
  bounced to the global deselect. Now any vim keystroke re-grabs focus on the text box, so
  while the editor is focused Escape only switches Insert->Normal and never leaves the
  editor. Escape outside the editor still deselects (the global handler is gated on
  `wants_keyboard_input()`). Hardened: the editor counts as focused if it was focused last
  frame too (egui can surrender focus on the Escape press before our intercept runs, which
  would otherwise skip the re-grab), and the global deselect is additionally skipped while
  a code tab is the active dock tab (`code_tab_focused`).
- [x] **Code editor text box drew a focus/selection outline** — fixed. The editor's
  `selection.stroke` and the widget `bg_stroke`s are set to `NONE`, so the black text box
  has no border in any state.

- [x] **Code editor status line not visible** — fixed. The fill-the-dock scroll area
  consumed the height reserved for the status line, pushing it off-screen. The scroll
  viewport is now capped (`max_height`) so the bottom status line always shows.
- [x] **Unreadably small UI text** (problems list, file-browser path, etc.) — fixed.
  Enforced a global minimum text size equal to the folder-explorer text (13px): any
  smaller egui text style is bumped up at startup; larger styles (headings) are left
  alone. The grid tile names and a few explicit labels were raised to match.

- [x] **Abort adding a plugin component** ("Plugin of type LabelSelectionState not
  found") — fixed. Plugin crates are `cdylib`s that statically link their own copy of
  `egui`, so its types have different `TypeId`s than the host's. egui's selectable-label
  state is a context plugin keyed by `TypeId`; a *selectable* label drawn from a plugin's
  egui looks up its own (unregistered) `TypeId` and panics → SIGABRT (foreign exception,
  uncatchable). Fix: disable selectable labels for the whole component inspector
  (`components_ui` sets `interaction.selectable_labels = false`) — non-selectable labels
  skip that path, safe across the dylib boundary. (Proper long-term fix: share one egui
  instead of static-linking it into each plugin.)

- [x] **Abort rendering an ObjectRef drop box** ("Plugin of type DragAndDrop not found")
  — fixed. Same cdylib-boundary class as the LabelSelectionState crash: egui's drag-and-
  drop is a context plugin keyed by `TypeId`, so `dnd_drop_zone`/`dnd_*` called from a
  plugin's separately-linked egui looks up *its* `DragAndDrop` TypeId, unregistered in the
  shared Context → SIGABRT (and it fired every frame the Orbit inspector rendered, drag or
  not). Fix: `InspectCtx::object_ref` no longer uses egui dnd. The Scene tree publishes the
  dragged object index into egui memory as a plain `usize` (std type → slot shared across
  the boundary); the drop box reads it and detects the drop with raw pointer state
  (`rect_contains_pointer` + `pointer.any_released()`). The engine clears the slot at end of
  frame once the pointer is up. (General rule: don't call egui's dnd/selection context-
  plugin APIs from plugin inspector/gizmo code — use memory + pointer state.)

- [x] **Baked scene loaded black until a manual re-bake** — fixed. In `EngineApp::init`,
  `load_bake()` ran *before* `self.renderer = Some(renderer)`, so `upload_baked_probes`
  saw no renderer and skipped pushing the lightmaps/probes to the GPU. The bake still
  loaded into `scene.baked` (so baked-mode lights + the env sun dropped from the realtime
  pass), but static objects sampled the empty 1×1 default lightmap → everything black until
  the user re-baked (which uploads with the renderer present). This also looked like "the
  ground plane gets no light data" (the gbuffer pass is `cull_mode = NONE`, so the plane
  bakes fine — it just wasn't uploaded). Fix: moved `load_bake()` to after the renderer +
  window are stored in `init`.

- [x] **SIGSEGV in a running editor when a second editor opens the same project** — fixed.
  The plugin hot-loader copies each built `.so` to `target/citrus-plugins/<name>-<gen>.so`
  and `dlopen`s the copy, but `<gen>` is a per-process counter starting at 0 — so two
  editor instances on the same project both write `<name>-0.so`, and the second's
  `fs::copy` overwrote the file the first had `mmap`ped, faulting the first editor when it
  next called into the plugin. Fix: the copy filename now includes the process PID
  (`<name>-<pid>-<gen>.so`), so instances never collide. (Explains the "random" crash —
  it happened whenever a second `citrus` process started, e.g. during dev.)

- [x] **Ctrl+Z stopped triggering undo in the viewport (menu Undo still worked)** —
  fixed. Editor keyboard shortcuts are handled in the winit `KeyboardInput` path, gated
  on `!egui_wants` (the egui `on_window_event` `consumed` flag). The code editor clings to
  keyboard focus (vim re-grabs it; the "focused last frame" heuristic keeps it sticky), so
  a lingering `TextEdit` made egui consume Ctrl+Z as its own text-undo shortcut → the
  viewport handler was skipped. Two fixes: (1) clicking/looking the 3D viewport now calls
  `memory.stop_text_input()` so a stale code-editor field releases focus; (2) modifier
  keys (Ctrl/Shift/Alt) are tracked in `self.keys` even when egui consumed the press, so a
  Ctrl press swallowed by a focused field can't desync the set and leave `ctrl == false`
  for a later Z press. (Ctrl+S/Ctrl+Y weren't egui text shortcuts, so they slipped through
  — which is why save and the menu kept working.)

- [x] **GPU memory leak at shutdown ("lightmap array" + "probe sh")** — fixed.
  `gpu_allocator` reported two leaked chunks on exit: the baked-lightmap 2D array
  (`self.lightmaps`) and the probe-SH SSBO (`self.probe_buffer`). Both were swapped
  correctly on each re-bake (the replaced resource was destroyed), but the final live
  ones were never freed — `Renderer::drop` destroyed the shadow atlas, meshes, textures,
  UBOs, etc. but not these two. Added `self.lightmaps.destroy()` + `self.probe_buffer
  .destroy()` in `Drop`, inside the same allocator lock, after `device_wait_idle`.

- [x] **Lightmap seam lines on baked cubes/sphere + blown-out highlights** — addressed.
  Two separate causes. (1) Straight lines along cube edges and a vertical seam down the
  sphere were **UV-chart seams**: the cube packs its 6 faces into a 3×2 atlas and the sphere
  uses a lat-long unwrap with a meridian seam, so co-located 3D points land in different
  charts and get different Monte-Carlo results → a line at the boundary. Added a post-bake
  `stitch_seams` pass that buckets valid texels by world position (from the gbuffer readback)
  and averages co-located texels whose normals agree, so smooth seams merge while genuine
  hard-edge discontinuities are preserved. (2) The "white-washed" cube face / "brighter when
  baked" was **no tonemapping** — values >1.0 clipped to white after the sRGB write, and a
  static object's indirect term jumps from flat ambient (un-baked preview) to full baked GI.
  Added ACES tonemapping to the standard + skybox shaders. (Remaining: the lat-long sphere
  still wastes texels at the poles — a lower-distortion unwrap is a tracked follow-up.)

- [x] **Scene failed to parse after Realtime GI landed** ("Expected struct `RealtimeGi` but
  found `false`") — fixed. The `realtime_gi` field changed type from `bool` to a settings
  struct, so scenes saved with the old `realtime_gi: false` no longer matched. Added a
  `deserialize_with` shim (untagged `bool | RealtimeGi`) that maps a legacy bool to
  `RealtimeGi { enabled, ..default }`; both forms now load (unit-tested in `scene_file.rs`).

- [x] **Seam lines on baked primitives (cube edges, atlas borders)** — addressed (re-bake
  needed). Three causes: (1) the baked lightmap was sampled with the **REPEAT** material
  sampler, so bilinear filtering at the atlas border wrapped to the opposite edge — added a
  dedicated **CLAMP_TO_EDGE** lightmap sampler. (2) The cube packs 6 faces in a 3×2 atlas
  with a 4% gutter, which is **sub-texel at low lightmap sizes**, so bilinear bled between
  faces — widened the gutter (0.04→0.06), raised the minimum lightmap size to 64 (so a
  multi-chart atlas keeps ≥2-texel gutters), and dilate 6 rings instead of 4. (3) `gather_bake`
  wasn't applying the per-object `lightmap_scale`, so "Scale In Lightmap" only affected the
  preview, not the bake — now applied in both. Existing cube bakes must be re-baked (the
  cube's lightmap UVs changed).

- [x] **Realtime GI produced zero light (`avg L0 luminance 0.000`, no visible difference)**
  — fixed. `gather_realtime_gi` only collected lights whose mode was `Realtime`, skipping
  `Baked`/`Mixed` lights. But realtime GI only runs when there's *no* bake, and in that state
  every light renders in realtime regardless of mode (see `gather_lights`). So a scene whose
  point lights had been set to Baked/Mixed (common after experimenting with the baker) handed
  the GI march *no lights* → probes came out all-zero → no bounce, no difference toggling GI.
  Fix: `gather_realtime_gi` now includes all active lights (+ the env sun). Confirmed the
  march itself was correct with a new unit test (`sw_gi::probe_gathers_bounced_light` — a
  probe over a lit cube gathers non-zero bounce).

- [x] **Panic: "attempt to add with overflow" in the software-GI march** (`sw_gi.rs`) —
  fixed. The per-probe RNG seed was `pi as u32 * 9277 + seed.wrapping_mul(26699)` with a
  non-wrapping `*`/`+`; the realtime-GI driver's per-trace seed accumulates (`wrapping_add`
  0x9E3779B9 each trace) so it grows large, and the plain add overflowed `u32` → debug-build
  panic across the parallel march threads. Fixed with `wrapping_mul`/`wrapping_add`.

## Crash / stability

- [x] **SIGSEGV on editor close** — fixed. `Renderer` declared `window: Arc<Window>`
  *before* `ctx: GpuContext`, so after `Renderer::drop`'s body the fields dropped
  window-first. `EngineApp` had already released its own `Arc<Window>` clone, so the
  renderer's field was the last strong ref, the winit window was destroyed, then
  `GpuContext::drop` called `destroy_surface` on a surface whose window was already gone
  (use-after-free; NVIDIA/Wayland faulted at exit, the egui frame in the backtrace was
  nearest-symbol noise). Fix: moved `window` to the last field so it outlives
  `ctx`/surface teardown.
- A crash handler (`crash.rs`) installs SIGSEGV/SIGBUS/SIGILL/SIGABRT handlers and
  prints a symbolized Rust/native backtrace before re-raising for the core dump. Keep it
  for future crashes; grab the "==== citrus crashed ====" backtrace when one happens.
