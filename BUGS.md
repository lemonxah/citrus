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
