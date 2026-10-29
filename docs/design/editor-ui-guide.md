# Editor UI guide

How the Citrus editor implements the [Editor UI Design Spec](editor-ui-spec.md),
how to build new UI that matches it, and the plan (with status) for converting every
existing panel. The spec owns the values; this guide owns the code.

## 1. Where things live

| Module | What it holds |
|---|---|
| `crates/citrus-editor/src/theme.rs` | Every colour, radius, size and font from spec §2 as named tokens; the runtime accent; `theme::apply(ctx)` builds the egui style; `ObjectKind` (type family colour + icon) |
| `crates/citrus-editor/src/icons.rs` | The spec §11 icon set, drawn from its SVG path data (stroke 1.4–1.5, round caps); the logo |
| `crates/citrus-editor/src/widgets.rs` | The widget kit: buttons, troughs and segments, icon toggles, badges and chips, type tile, vector field, property rows, section headers, info notes, dashed add button, search field |
| `crates/citrus-editor/assets/fonts/` | IBM Plex Sans 400/500/600 and Plex Mono 400/500 (SIL OFL, same files as ZestBay) |

`citrus_editor::install_editor_fonts(ctx)` and `theme::apply(ctx)` run once at
startup (`editor_app::start_load`). The Phosphor icon font stays installed behind
Plex for icons the spec set doesn't cover.

## 2. Rules for new UI

1. **No literal colours.** Use `theme::*` tokens. If a colour you need has no token,
   add the token to `theme.rs` (and to the spec if it's a new design decision) rather
   than writing a hex in panel code. Light theme (spec §12) will swap the token table.
2. **Yellow means action, selection, active or playing.** Use `theme::accent()`,
   `accent_faint()` or `accent_soft()`, never the `#F5C84B` literal: the accent is a
   runtime setting (`theme::set_accent`). Decoration is never yellow.
3. **Use the kit before raw egui.** `widgets::primary_button` for the one main action
   in a panel, `secondary_button` for the rest, `icon_button` (always with a tooltip)
   for icon-only controls, `segmented` instead of radio rows, `property_row` for every
   label + control pair, `section` for collapsible groups.
4. **Icons.** Use `icons::Icon` for anything in the spec set (object types, tools,
   transport, eye, caret, reset, snap, files). Fall back to `egui_phosphor` for other
   glyphs. Never Unicode symbols or emoji: egui's fonts lack many of them and they
   render as empty boxes.
5. **Object types are recognisable.** Anything that names an object (hierarchy rows,
   inspector header, component headers, pickers, viewport gizmos) uses
   `ObjectKind::color()` and `ObjectKind::icon()`.
6. **Type.** Body text is Plex Sans 12 px (`TextStyle::Body`). Numbers, values,
   shortcuts and logs are Plex Mono 11 px (`theme::mono(11.0)`). Titles 13 px semibold
   (`theme::semibold(13.0)`). Labels under fields and meta text 11 px. Never below
   10 px.
7. **Sizes.** Fields and selects 26 px tall, small buttons 24, menu and toolbar buttons
   28, primary panel buttons 32, icon toggles in strips 22. Panel padding 12, field gap
   4, row gap 6–8, group gap 12–14. The constants are in `theme.rs` (`H_FIELD`,
   `H_BUTTON_SMALL`, ...).
8. **Icon-only controls need a tooltip** that reads like an accessible name ("Move
   (W)", "Hide Sphere").

## 3. Building a panel: an example

```rust
use citrus_editor::{icons::Icon, theme, widgets};

fn my_panel(ui: &mut egui::Ui, s: &mut MyState) {
    widgets::eyebrow(ui, "Sky");
    widgets::property_row(ui, "Source", |ui| widgets::segmented(ui, &mut s.source, &[
        (Source::Hdri, "HDRI"), (Source::Procedural, "Procedural"), (Source::Solid, "Solid"),
    ]));
    widgets::property_row(ui, "Exposure", |ui| ui.add(egui::Slider::new(&mut s.exposure, -4.0..=4.0)));
    widgets::info_note(ui, "Changes apply to the whole scene.");
    if widgets::primary_button(ui, Some(Icon::Play), "Bake lighting").clicked() { /* ... */ }
}
```

Component inspectors (`components.rs`) get their section header (caret, type icon,
name, enable checkbox, kebab) from the inspector list; inside `inspector_ui` they only
lay out rows with `property_row` / `vec2_row` / `rgba_row` / `enum_row`, which are
themed, so a new component inspector looks right without extra work.

## 4. Conversion plan

Status: `[x]` done · `[~]` partly · `[ ]` not started. Phases are ordered so each one
builds on the previous; the shell (A–B) is what every later panel relies on.

### Phase A — foundation
- [x] `theme.rs` tokens (spec §2), runtime accent with faint/soft variants, `apply()`
  replacing `apply_editor_style` (egui visuals, widget states, spacing, text styles)
- [x] IBM Plex fonts embedded and installed ahead of egui's defaults and Phosphor
- [x] `icons.rs`: SVG path renderer (M/L/H/V/C/S/A/Z, absolute + relative) and the
  §11 set + logo
- [x] `widgets.rs` kit (see §1)
- [x] egui_dock styled per spec: 36 px tab strips on bg.panel, active tab text.strong
  with 2 px accent underline, inactive text.muted, 1 px line.panel separators

### Phase B — shell
- [x] Menu bar (§4): logo + "Citrus", menu buttons, centred transport trough (Play/Stop
  primary, Pause, Step), breadcrumb `project / scene`, command search button (Ctrl K),
  preferences gear
- [x] Status bar (§9): status dot + Ready/Playing, scene, object count, selection,
  fps/ms in mono on the right
- [x] Default layout sizes (hierarchy 264, inspector 324, bottom dock 288) and tab
  order (Viewport, Camera, UI Editor | Inspector, Environment, FluxBaker | Files, Log)
- [x] Command palette (Ctrl K): fuzzy list of editor actions (new)
- [x] Preferences window: accent presets (§2.3) + custom colour (new)

### Phase C — scene hierarchy (`scene_panel.rs`)
- [x] Header strip: "Scene" + count badge, Add (secondary, plus) menu, kebab menu
- [x] Filter field "Filter by name or type" (name and object-type matching)
- [x] 26 px rows: caret (kept on leaves), type icon in type colour, name, collapsed
  child-count badge, eye toggle (quiet, brighter on selection); selected row
  accent.faint; hidden rows dimmed including descendants
- [~] Duplicate naming `Name (2)`, `Name (3)`; "Duplicate into group" with zero-padded
  numbered children (spec §5)

### Phase D — inspector (`inspector.rs`, `components.rs`, `sections.rs`)
- [x] Object header: 32 px type tile, borderless editable name, type line ("UI toggle ·
  child of Canvas"), Active checkbox; Layer / Tag / Static row
- [x] Transform section: reset + kebab in header, vector fields with axis tiles,
  drag-to-scrub on the tile, 2 / 1 decimals
- [~] Component section header: caret, type-coloured icon, name, enable checkbox,
  kebab (Remove, Reset, Copy / Paste values)
- [x] Row helpers (`property_row`, `vec2_row`, `rgba_row`, `enum_row`) themed: 104 px
  label column, 26 px controls, colour swatch + hex
- [ ] Spec §8.3 per-type layouts: UI Canvas, UI Rect (3×3 anchor grid), UI Toggle, UI
  Text, Light, Mesh Renderer (asset pickers), Camera, Flux Volume ("Open in FluxBaker"),
  Reflection Probe
- [~] All other component inspectors on the themed helpers: Volume, LOD Group, Spin,
  Bob, Rigid Body, Character Controller, Particle Emitter, Scene Portal, NavMesh
  Surface, Nav Agent, Physics Joint, Animator, UI Panel / Image / Button / Slider /
  Progress Bar / Pause Menu / Text Field, Light Probe Volume, Planar Reflection,
  FluxVoxel Light, Audio Source / Listener, Box / Sphere / Mesh Collider, Pawn, Spawn
  Point, Sync
- [ ] Material editor (`sections.rs`), file material / model import inspectors
- [x] Info notes and the dashed full-width Add component button
- [ ] Multi-select header (mixed values) — follow-up (spec §12)

### Phase E — viewport (`editor_app::viewport_ui`)
- [~] Viewport tab strip extras: Shading select, Gizmos / Grid / Stats icon toggles,
  Projection select
- [x] Tool trough (Select Q, Move W, Rotate E, Scale R, Rect T) with the spec icons
- [x] Transform options trough: Pivot | Center, Global | Local, Snap toggle (magnet) +
  increment field
- [x] Playing chip and 2 px inset accent play outline
- [x] Orientation widget (axis lollipops, click to snap view, Persp/Ortho label)
- [x] Stats pill bottom-left (fps in accent, ms dim; expands with Stats toggle)
- [ ] Billboard icons in object-type colours; transform gizmo colours per §2.5
- [~] UI selection overlay: dashed accent.soft canvas frame + chip, accent outline on
  the selected element (the 2D handles already replace the 3D gizmo)
- [ ] Grid floor axis lines at 45% alpha

### Phase F — bottom dock
- [x] Files (`file_browser.rs`): folder tree (188 px), toolbar (up, folder name, item
  count, search, Size slider, Create primary), 92×88 tiles with 34 px type icons,
  selected tile accent.faint + accent border
- [x] Log (`editor_app::log_ui`): segmented All / Warnings / Errors with counts, Clear
  on play, Clear; mono rows with coloured levels, warn tint, empty state; warning count
  badge on the Log tab

### Phase G — other tabs
- [~] Environment: Sky / Ambient and fog / Post groups with eyebrows (spec §8.4);
  existing Flux GI, reflections and shadow settings regrouped under eyebrows
- [x] FluxBaker: "In this scene" card, Quality segmented, Bounces, Denoise, Bake
  (primary) + Clear
- [ ] UI Editor tab toolbar on the kit (canvas select, 2D/3D segmented, resolution,
  Add, Templates, Snap)
- [ ] Camera tab
- [ ] Code editor tab (`code_editor.rs`): mono 11 px, diagnostics colours from status
  tokens

### Phase H — windows and modals
- [x] Shared window frame: bg.panel, line.panel border, 8 px radius, 12 px padding,
  semibold 13 px title
- [ ] Project Settings, New Project, Open Project, Unsaved changes, Layers, Input
  Bindings, Audio Mixer, Network, VR Tools, Controls, Generate Lightmap UVs, Component
  build failed, Background tasks
- [ ] Busy modal and bake modal cards
- [ ] Profiler window (separate OS window) on the theme
- [ ] Splash screen status line in Plex
- [ ] VR headset editor panels

### Behaviour changes the spec asks for (outside pure styling)
- [~] Duplicate naming and duplicate-into-group (Phase C)
- [x] Select tool (Q) and Rect tool (T)
- [ ] Shading modes Wireframe / Lighting only / Overdraw; projection presets
- [x] Pause and Step in the transport (Pause exists; Step advances one fixed frame)
- [x] Command palette, Preferences (accent)
- [ ] Log "Clear on play"

### Notes on the current state
- Duplicate-into-group (spec §5) is not built yet; plain duplicates use `Name (2)`.
- Component headers have no enable checkbox: components have no enabled flag yet.
- Hierarchy eye toggles the object's Active flag (there is no separate editor-only visibility).
- Tag (inspector header) is not shown: objects have no tags yet.
- Viewport tab strip: Shading, Gizmos (right-click or the eye button for billboard widgets), Stats and a
  view menu (Top / Front / Right snaps). Grid toggle, Wireframe / Overdraw shading and an orthographic
  projection need renderer support.
- Accent presets + custom colour live in Project Settings > Appearance (saved as
  `settings.accent` in project.citrus).
- Environment: every row is on the themed row label and sections use eyebrows; the spec's regrouping
  into Sky / Ambient and fog / Post (and Tonemapper / Bloom / SSAO controls) is still to do.
- FluxBaker: Quality as a segmented control and a Denoise checkbox are still to do (current settings
  keep their own controls).
- UI Rect has the 3x3 anchor grid + Stretch; the other §8.3 per-type layouts (Light colour + Kelvin,
  Mesh Renderer asset pickers, Camera clear-with) are still to do.

