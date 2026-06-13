# citrus — Bugs & stability

Open bugs, verify-after-rebuild items, and crash/stability tracking. Feature status
and goals live in [Features.md](Features.md); working list + design notes in
[TODO.md](TODO.md).

Legend: `[ ]` open · `[verify]` believed fixed, confirm after a rebuild · `[x]` fixed

## Open / verify

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
