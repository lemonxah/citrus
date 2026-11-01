# ZestEngine Engine — Editor UI Design Spec

Status: design approved for implementation · Owner: Ryno · Reference mockup:
"ZestEngine Engine editor redesign" Design canvas (artboard `Main.dc.html`, interactive)

This document describes the redesigned ZestEngine Engine editor shell: layout, visual
tokens, every panel, and the interaction states. It is framework-agnostic. Map the
tokens and components onto whatever UI layer the editor uses; do not change the
values without checking with Ryno.

ZestEngine is part of a suite with ZestPaint and ZestBay. It must read as the same
family: same graphite surfaces, same IBM Plex type, same yellow accent. When in
doubt, match ZestPaint.

How the editor implements this spec (tokens in code, widget kit, how to add new UI,
conversion status) is in [editor-ui-guide.md](editor-ui-guide.md).

## 1. Goals

- Modern, calm, dark editor that matches ZestPaint/ZestBay.
- Yellow is the action and selection colour only, never decoration.
- Every object type is recognisable at a glance by icon and colour.
- Kill the "Spot Light Copy Copy Copy…" problem: sane duplicate naming and groups.
- Context-sensitive inspector: show only the components the selected object has.

Non-goals for this pass: light theme (planned later, see §12), docking/rearrangeable
panels, a real Camera (game) view.

## 2. Design tokens

### 2.1 Colour: surfaces and lines

| Token | Hex | Use |
|---|---|---|
| bg.chrome | `#0B0C0E` | Menu bar, status bar |
| bg.app | `#111214` | App background, inset notes |
| bg.viewport | `#0C0D0F` | 3D viewport clear colour around the floor |
| bg.panel | `#17181C` | Hierarchy, inspector, bottom dock, tab strips |
| bg.field | `#0F1012` | Inputs, selects, segmented-control troughs |
| bg.button | `#26282D` | Secondary buttons |
| bg.segment.active | `#2E3036` | Active segment / toggled icon button |
| line.panel | `#24262B` | Panel borders, section dividers |
| line.field | `#2A2C31` | Input and overlay borders |
| line.button | `#33363C` | Secondary button borders |
| line.dashed | `#3A3C42` | Dashed "Add component" button |
| overlay | `rgba(17,18,20,0.92)` | Floating viewport toolbars |

### 2.2 Colour: text

| Token | Hex | Use |
|---|---|---|
| text.strong | `#F2F3F5` | Titles, active tab, selected row |
| text.body | `#E4E5E9` | Default text, field values |
| text.secondary | `#C9CBD1` | Menu items, unselected rows, secondary buttons |
| text.muted | `#A3A6AF` | Field labels, inactive tabs |
| text.dim | `#8D909A` | Placeholders, meta text, carets |
| text.hidden | `#858892` | Rows whose object is hidden |
| text.onAccent | `#141413` | Text/icons on a yellow fill |

All text pairs above pass 4.5:1 on bg.panel.

### 2.3 Colour: accent (the yellow)

| Token | Value | Use |
|---|---|---|
| accent | `#F5C84B` | Primary buttons, active tool, active tab underline, focus, slider/checkbox tint, selection outline |
| accent.faint | accent @ 14% alpha | Selected row/tile background, status chips |
| accent.soft | accent @ 55% alpha | Dashed UI-canvas frame in viewport |
| accent.hover | `#FFE08A` | Links / hover on accent text |

The accent must be a single runtime setting. Everything yellow derives from it
(faint/soft are alpha variants), so a user-chosen accent recolours the whole UI.
Suggested presets: `#F5C84B` (default), `#FFB547`, `#C9E265` (ZestBay green), `#7CC4FA`.

Rule: if it is not clickable, selected, active, or "playing", it is not yellow.

### 2.4 Colour: object types

Used for hierarchy icons, inspector header tile (icon on a 16% alpha tile of the same
colour), component header icons, and viewport gizmos.

| Type family | Hex | Types |
|---|---|---|
| Light | `#F0A040` | Directional, Spot, Point |
| Mesh / model | `#7CC4FA` | Model (imported asset root), Mesh |
| Camera | `#A78BFA` | Camera |
| UI | `#39C6B0` | UI Canvas, Toggle, Text (and future UI widgets) |
| GI / probes | `#E879B5` | Reflection Probe, Flux Volume |
| Group | `#A3A6AF` | Empty / group nodes |

### 2.5 Colour: axes and status

| Token | Hex | Use |
|---|---|---|
| axis.x | `#F06A6A` | X labels, gizmo, floor X line (45% alpha) |
| axis.y | `#8BD46A` | Y |
| axis.z | `#6AA8F0` | Z |
| Axis label tile | axis @ 12% alpha | Background of the X/Y/Z letter inside vector fields |
| status.ok | `#8BD46A` | "Ready" dot |
| status.warn.fg / .bg | `#FFB366` / `#3A2612` | Warning badge, warn log level |
| Warn row tint | `rgba(255,179,102,0.08)` | Warning log rows |
| status.info | `#7CC4FA` | INFO log level |
| status.error | `#FF8A8A` | ERROR log level |

### 2.6 Typography

- UI: IBM Plex Sans 400/500/600. Base size 12 px, line-height 1.3.
- Numbers, shortcuts, values, logs: IBM Plex Mono 400/500, 11 px.
- Sizes: 13 px/600 (app name, inspector object name) · 12 px (body) · 11 px (meta,
  labels under fields, badges) · 10 px (axis letters, count badges).
- Section eyebrows (Environment tab): 11 px/600, uppercase, letter-spacing 0.06em,
  text.muted.
- Never below 10 px.

### 2.7 Shape and spacing

- Radii: 3 px (tiny swatches) · 4–5 px (fields, small buttons, badges) · 6 px
  (buttons, rows, segments) · 7–8 px (toolbars, tiles, overlays, primary buttons).
- Control heights: 22 px (icon toggles in strips) · 24 px (small buttons, segments) ·
  26 px (fields, selects, secondary buttons) · 28 px (menu items, viewport toolbar
  buttons, search) · 32 px (primary panel buttons).
- Hierarchy row: 26 px. Indent: 14 px per depth level.
- Panel padding: 12 px. Gaps: 4 px between fields, 6–8 px between rows, 12–14 px
  between groups.
- Icons: 16×16 grid, stroke 1.4–1.5, round caps and joins. Drawn at 13–15 px in lists
  and toolbars, 34 px in file tiles (stroke 0.9).
- No gradients on chrome, no drop-shadow cards, no emoji.

## 3. Layout

Reference frame 1600×960. Fixed regions, fluid centre.

```
┌──────────────────────────── Menu bar 40 ────────────────────────────┐
├─ Hierarchy 264 ─┬──────── Viewport tabs 36 ────────┬─ Inspector 324 ─┤
│                 │                                   │                 │
│                 │            Viewport (fluid)       │                 │
│                 │                                   │                 │
│                 ├──────── Bottom dock 288 ──────────┤                 │
│                 │  Files | Log                      │                 │
├─────────────────┴───────────────────────────────────┴─────────────────┤
└──────────────────────────── Status bar 26 ──────────────────────────┘
```

- Hierarchy and inspector run the full body height; the bottom dock spans only the
  centre column.
- Panels are separated by 1 px line.panel borders, no gaps.
- All panel tab strips are 36 px tall on bg.panel with a bottom border.

## 4. Menu bar (40 px, bg.chrome)

Three zones: left and right are flex 1, centre is fixed so the transport is always
centred.

Left: logo mark (20 px zestengine slice: yellow disc, dark ring and 6 segment lines, dark
centre dot) + "ZestEngine" (13 px/600). Then menu buttons: File, Edit, Object, Tools,
View, Windows, Help (28 px tall, 9 px horizontal padding, transparent,
text.secondary, 5 px radius).

Centre — transport: a trough (bg.panel, line.field border, 8 px radius, 2 px padding)
containing:

- Play — primary: accent fill, text.onAccent, 600 weight, play-triangle icon + "Play".
  While playing it becomes a stop-square + "Stop" (still yellow).
- Pause and Step — icon-only, 28×26. text.dim when stopped, text.body while playing.

Right: breadcrumb "zestengine / main.scene" (project dim, slash darker, scene
text.secondary, ellipsis on overflow) · command search button (220 px, "Search
commands" + Ctrl K in mono) · preferences gear (icon button).

## 5. Scene hierarchy (left, 264 px)

Header strip: "Scene" (600) + object count badge (mono 11 px, `#24262B` pill) ·
spacer · "Add" secondary button with plus · kebab menu.

Filter: 28 px search field, placeholder "Filter by name or type", inside 8 px padding.

Tree rows (26 px):

```
[caret 16 px] [type icon 14 px] [name, ellipsis] [count badge if collapsed group] … [eye button 22 px]
```

- Caret only on nodes with children; rotates 90° when open. Keep its space on leaf
  rows so icons align.
- Clicking the name area selects; clicking the caret only expands/collapses.
- Selected: row background accent.faint, name `#F5F6F8`. No left border bar.
- Hidden: eye shows the crossed-out icon in text.secondary; name goes to text.hidden
  and icon to 45% opacity. Hiding a parent dims all descendants (their own eye state is
  untouched).
- Eye on visible rows is very quiet (`#5D6068`), a bit brighter (text.muted) on the
  selected row.
- Collapsed groups show a child-count badge (e.g. "18").

Naming rules (behaviour change):

- Duplicating Spot Light produces Spot Light (2), Spot Light (3)… never "Copy Copy".
- "Duplicate into group" / multi-duplicate should offer to create a group (e.g.
  "Ceiling Spots") with zero-padded numbered children (Spot Light 01…18).

## 6. Viewport

### 6.1 Tab strip (36 px)

- Tabs: Viewport (active) and Camera. Active tab: text.strong, 500 weight, 2 px accent
  underline. Inactive: text.muted, transparent underline. Each tab has a 13 px icon.
- Right side: Shading select (Lit / Unlit / Wireframe / Lighting only / Overdraw) ·
  icon toggle group in a trough (Gizmos, Grid, Stats; active = bg.segment.active +
  text.strong) · Projection select (Perspective / Orthographic / Top / Front / Right).

### 6.2 Floating overlays (12 px from edges)

- Top-left, tools: trough with 5 tool buttons (30×28, icon only, tooltip "Move (W)"
  etc.): Select Q, Move W, Rotate E, Scale R, Rect T. Active tool = accent fill +
  text.onAccent icon.
- Next to it, transform options: trough with Pivot | Center segment, divider, Global |
  Local segment, divider, Snap toggle (magnet icon; when on: bg.segment.active bg and
  accent icon/text) and a 56 px snap increment field ("0.50 m", mono).
- Playing chip: when playing, append a chip "Playing — edits revert when you stop"
  (accent.faint bg, accent text, dot).
- The tool overlays wrap onto a second line if the viewport is narrow; they must not
  overlap the axis widget (reserve 96 px on the right).
- Top-right, orientation widget: 64 px disc (rgba(17,18,20,0.75), line.field stroke)
  with X/Y/Z lollipops in axis colours, letters in text.onAccent, hollow circles for
  negative axes. "Persp" label below (11 px, muted). Clicking an axis snaps the view.
- Bottom-left, stats: pill with "579 fps" (number in accent) and "1.73 ms" (dim), mono
  11 px. Expands to draw calls / tris when the Stats toggle is on.

### 6.3 Scene rendering conventions

- Grid floor; X axis line red and Z axis line blue at 45% alpha.
- Gizmo icons use the object-type colours (lights amber, camera violet, probes pink).
- Transform gizmo: X red, Y green, Z blue arrows, white centre handle with dark ring.
- UI selection: when a UI Canvas or any UI child is selected, hide the 3D transform
  gizmo and draw the canvas frame as a 1.5 px dashed accent.soft rectangle at the
  canvas aspect ratio, with a chip above its top-left corner: "Canvas · 1920 × 1080 ·
  Screen overlay" (accent.faint / accent). The selected UI element inside it gets a
  1.5 px solid accent outline, 2 px offset.
- Play mode: 2 px inset accent outline around the whole viewport.

## 7. Bottom dock (288 px)

Tab strip: Files · Log (Log tab carries a warn-coloured count badge when there are
warnings).

### 7.1 Files

- Left folder tree (188 px, divider on the right): "Project" root row selected
  (accent.faint, accent folder icon, open caret), child folders indented 22 px with
  muted folder icons.
- Toolbar (40 px): up-folder button · current folder name (500) · "N items" (dim) ·
  spacer · search field (180 px) · "Size" slider (accent tint) · Create primary button
  (accent fill, plus icon).
- Grid: auto-fill tiles, min 92 px wide, 88 px tall, 4 px gap. Tile = 34 px icon over
  an 11 px name (ellipsis). Selected tile: accent.faint fill + 1 px border at accent
  45%. Unselected: no border.
- File-type icon colours: folders `#A3A6AF`, scenes `#7CC4FA` (cube), project file
  accent (gear), .toml `#F0A040`, .md text.secondary (doc with lines).

### 7.2 Log

- Toolbar: segmented filter "All N / Warnings N / Errors N" · spacer · "Clear on play"
  checkbox · Clear button.
- Rows (mono 11 px, 5×8 px padding, 5 px radius): time (dim) · level (40 px, coloured:
  INFO blue, WARN orange, ERROR red, 500 weight) · source (56 px, muted) · message
  (body).
- Warning rows get the warn tint background.
- Empty filter state: "No errors since the scene loaded." in Plex Sans, dim.

## 8. Inspector (right, 324 px)

Tab strip: Inspector · Environment · FluxBaker (same tab style as the viewport). The
body scrolls vertically.

### 8.1 Object header

32 px type tile (type colour icon on 16% alpha tile, 8 px radius) · editable name
(13 px/600, borderless until focused) with the type line under it ("UI canvas", or "UI
toggle · child of Canvas") · "Active" checkbox on the right.

Row below, 3 columns: Layer select · Tag select · Static checkbox, each with an 11 px
muted label above.

### 8.2 Transform section

Header: open caret · "Transform" (600) · reset icon button · kebab.

Three rows: Position, Rotation, Scale. Label column 64 px, then three equal vector
fields.

Vector field: 26 px, bg.field, 5 px radius, clipped; a 16 px axis tile at the left
(letter in axis colour on 12% tint), then the mono value. Position/scale to 2
decimals, rotation to 1 decimal.

Fields support drag-to-scrub on the axis tile (cursor ew-resize) and type-to-edit.

### 8.3 Component sections

Each section: 34 px header (caret · type-coloured icon · name 600 · enable checkbox ·
kebab), then 12 px padded body. Rows use a two-column grid (label 96–112 px, control
fluid). Sections are separated by line.panel.

Which sections show depends on the selection:

| Selected type | Sections |
|---|---|
| UI Canvas | UI Canvas: Render mode (Screen overlay / Screen camera / World space), Scale mode (Scale with screen / Constant pixel size / Constant physical size), Reference size W/H, Match slider labelled Width ↔ Height with value, Sort order, info note "Children with UI components are laid out inside this canvas." |
| UI Toggle | UI Rect (3×3 anchor preset grid, active anchor filled accent; Pos X/Y; Size W/H) + Toggle (Is on, Transition: Colour tint / Sprite swap / None, On changed → dashed "Add handler" button) |
| UI Text | UI Rect + Text (multi-line text area, Font size, Colour swatch + hex) |
| Directional / Spot / Point light | Light: Type, Colour (swatch + hex + Kelvin hint), Intensity slider + value, Range slider + value, Shadows (Soft / Hard / None), Mode (Realtime / Baked (FluxBaker) / Mixed) |
| Model / Mesh | Mesh Renderer: Mesh asset picker, Material asset picker (round swatch), Cast shadows (On / Off / Two sided / Shadows only), Receive shadows, Contribute to FluxBaker GI |
| Camera | Camera: Projection, Field of view slider (°), Clip planes near/far, Clear with (Environment sky / Solid colour / Depth only) |
| Flux Volume | Flux Volume: Cell size, Fit bounds to (Static geometry / Manual), "Open in FluxBaker ↗" button that switches to the FluxBaker tab |
| Reflection Probe | Reflection Probe: Resolution (128–1024), Update (On bake / Every frame / Via script), Box projection |
| Group | Transform only |

Asset picker fields: 26 px, type icon, name with ellipsis, target icon on the right
(opens picker).

Info notes: bg.app fill, 6 px radius, 11 px muted text, info icon.

Always last: full-width 32 px Add component button, dashed line.dashed border,
transparent.

### 8.4 Environment tab

Three groups with uppercase eyebrows, divided by 1 px lines:

- Sky: Source (HDRI / Procedural / Solid colour), HDRI picker (empty state "None —
  choose a file"), Exposure slider.
- Ambient and fog: Ambient slider, Distance fog checkbox.
- Post: Tonemapper (AgX / ACES / Reinhard / None), Bloom, Screen-space AO.

### 8.5 FluxBaker tab

- "In this scene" card (bg.app, bordered, 8 px radius) listing the scene's Flux
  Volumes and Reflection Probes with their type icons.
- Quality (Preview / Medium / Production), Bounces slider, Denoise result checkbox.
- Bake lighting (primary, flex 1) + Clear (secondary).

## 9. Status bar (26 px, bg.chrome, 11 px muted)

Left: status dot + "Ready" (green) or "Playing" (accent) · scene name · object count ·
"Selected: <name>". Right: "579 fps · 1.73 ms" in mono.

## 10. Interaction and state summary

| Action | Result |
|---|---|
| Click hierarchy row | Select object; inspector switches to Inspector tab and rebuilds sections for that type; status bar updates |
| Click caret | Expand/collapse only |
| Click eye | Toggle visibility; descendants dim |
| Q / W / E / R / T | Select tool; active tool button goes yellow |
| Play / Stop | Toggle play mode: button label/icon, viewport outline, playing chip, status dot/text, Pause/Step enabled |
| Snap toggle | Snap on = segment bg + yellow icon/text |
| Select UI node | 3D gizmo hidden, canvas frame + element outline shown |
| "Open in FluxBaker" | Switch inspector to FluxBaker tab |
| Log filter | Filters rows; empty state when none |
| Ctrl K | Opens command search |

Focus: every interactive element shows a 2 px accent focus ring (offset 1–2 px).
Every icon-only button needs a tooltip and accessible label (e.g. "Move (W)", "Hide
Sphere").

## 11. Icon set (16×16 grid, stroke, round caps/joins)

Use these path definitions so icons match the mockup exactly.

| Name | Path data |
|---|---|
| camera | `M2 5h8v6H2z M10 7.2l4-2v5.6l-4-2` |
| light.directional | `M8 5.2a2.8 2.8 0 1 0 0 5.6a2.8 2.8 0 1 0 0-5.6z M8 1.5v1.6 M8 12.9v1.6 M1.5 8h1.6 M12.9 8h1.6 M3.4 3.4l1.1 1.1 M11.5 11.5l1.1 1.1 M3.4 12.6l1.1-1.1 M11.5 4.5l1.1-1.1` |
| light.spot | `M6 2h4l3 8.5H3z M5.5 13.5h5` |
| light.point | `M6.2 12h3.6 M6.6 14h2.8 M8 2a4 4 0 0 0-2.5 7.1c.5.4.6 1 .6 1.9h3.8c0-.9.1-1.5.6-1.9A4 4 0 0 0 8 2z` |
| model / scene file | `M8 1.8l5.5 3v6.4L8 14.2l-5.5-3V4.8z M2.5 4.8L8 7.8l5.5-3 M8 7.8v6.4` |
| mesh | `M8 2.5l5.5 10h-11z` |
| group / folder | `M1.5 4.5a1 1 0 0 1 1-1h3.4l1.4 1.5h6.2a1 1 0 0 1 1 1v6.5a1 1 0 0 1-1 1h-11a1 1 0 0 1-1-1z` |
| reflection probe | `M8 2.5a5.5 5.5 0 1 0 0 11a5.5 5.5 0 1 0 0-11z M2.5 8c0 1.4 2.5 2.4 5.5 2.4s5.5-1 5.5-2.4` |
| flux volume | `M2.5 2.5h11v11h-11z M2.5 8h11 M8 2.5v11` |
| ui.canvas | `M2 3h12v10H2z M2 6h12` |
| ui.toggle | `M5 4.5h6a3.5 3.5 0 0 1 0 7H5a3.5 3.5 0 0 1 0-7z M11 6.3a1.7 1.7 0 1 0 0 3.4a1.7 1.7 0 1 0 0-3.4z` |
| ui.text | `M3 3.5h10 M8 3.5v9 M6 12.5h4` |
| file | `M4 1.5h5.5l3 3v10h-8.5z M9.5 1.5v3h3` |
| file.doc | `M4 1.5h5.5l3 3v10h-8.5z M9.5 1.5v3h3 M6 8h4.5 M6 10.5h4.5` |
| gear | `M8 5.8a2.2 2.2 0 1 0 0 4.4a2.2 2.2 0 1 0 0-4.4z M8 1.8v1.7M8 12.5v1.7M1.8 8h1.7M12.5 8h1.7M3.6 3.6l1.2 1.2M11.2 11.2l1.2 1.2M3.6 12.4l1.2-1.2M11.2 4.8l1.2-1.2` |
| tool.select | `M3.5 2l9 5-4 1.2-1.6 4.3z` |
| tool.move | `M8 1.5v13 M1.5 8h13 M8 1.5L6.2 3.3 M8 1.5l1.8 1.8 M8 14.5l-1.8-1.8 M8 14.5l1.8-1.8 M1.5 8l1.8-1.8 M1.5 8l1.8 1.8 M14.5 8l-1.8-1.8 M14.5 8l-1.8 1.8` |
| tool.rotate | `M13 8a5 5 0 1 1-1.5-3.6 M13 2.5v2.4h-2.4` |
| tool.scale | `M2.5 9.5v4h4 M2.5 13.5L7 9 M9.5 2.5h4v4 M13.5 2.5L9 7` |
| tool.rect | `M3.5 3.5h9v9h-9z M3.5 1.5v2 M1.5 3.5h2 M12.5 14.5v-2 M14.5 12.5h-2` |
| eye | `M1.5 8s2.4-4.5 6.5-4.5S14.5 8 14.5 8s-2.4 4.5-6.5 4.5S1.5 8 1.5 8z M8 6.2a1.8 1.8 0 1 0 0 3.6a1.8 1.8 0 1 0 0-3.6z` |
| eye.off | `M2 2l12 12 M6.5 3.7A6.8 6.8 0 0 1 8 3.5c4.1 0 6.5 4.5 6.5 4.5a11 11 0 0 1-1.8 2.3 M4.2 4.9C2.5 6.1 1.5 8 1.5 8s2.4 4.5 6.5 4.5c.9 0 1.7-.2 2.4-.5` |
| play (filled) | `M4.5 2.8v10.4L13 8z` |
| stop (filled) | `M4 4h8v8H4z` |
| pause (filled) | `M4 3h3v10H4zM9 3h3v10H9z` |
| step (filled) | `M3 3l7 5-7 5zM11 3h2v10h-2z` |
| caret (10×10) | `M3.5 2l3 3-3 3` |
| reset | `M3 8a5 5 0 1 0 1.5-3.6 M3 2.5v2.4h2.4` |
| snap / magnet | `M4 2v6a4 4 0 0 0 8 0V2 M4 5h3 M9 5h3` |

Logo (20×20): disc r=9 in accent; ring r=6.6 stroke 1.2 in bg.chrome; segment lines
`M10 3.4V16.6 M4.3 6.7L15.7 13.3 M4.3 13.3L15.7 6.7` stroke 1.2 in bg.chrome; centre
dot r=1.6 in bg.chrome.

## 12. Out of scope / follow-ups

- Light theme: ZestBay ships dark and light; ZestEngine should too. Not designed yet. Keep
  every colour behind a token so a light palette can be swapped in.
- Camera (game view) tab content.
- Panel docking, undocking and saved layouts.
- Multi-select inspector (mixed values).
- Context menus for hierarchy and files.

## 13. Placeholders in the mockup

These came from the mockup, not real engine data. Replace them with live values:

- Scene name main.scene, object counts, fps/ms figures.
- Log lines and timestamps.
- Light colour/intensity/range, camera FOV/clip, Flux cell size and other default
  values shown in the inspector.

## 14. Acceptance checklist

- [ ] All colours come from tokens; changing accent recolours every yellow element
  including faint/soft variants.
- [ ] Yellow appears only on: Play, primary buttons, active tool, active tab
  underlines, selection backgrounds/outlines, toggled snap, slider/checkbox tint,
  play-mode outline/chip, status "Playing".
- [ ] Hierarchy: type icons and colours match §2.4/§11; carets align; hide dims
  descendants; duplicate naming per §5.
- [ ] Inspector shows exactly the sections in §8.3 for each type.
- [ ] UI selection swaps the 3D gizmo for the canvas frame overlay.
- [ ] Play mode updates button, viewport outline, chip, status bar, Pause/Step.
- [ ] Fonts are IBM Plex Sans / Mono; no text under 10 px; values in mono.
- [ ] Every icon-only control has a tooltip and accessible name; visible accent focus
  ring.
