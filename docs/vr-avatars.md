# VR avatars: height, scaling and full-body calibration

A VR player's avatar should stand at the player's height, put its eyes where
the headset is, and have each tracker drive the right body part. ZestEngine
measures the player once, keeps the measurements per user, and fits whatever
avatar they wear to them. The code is in `crates/zestengine-core/src/body.rs` (data
and math) and `crates/zestengine-engine/src/body.rs` (devices, the calibration
flow, the fit and the body panel).

## The player's body

The engine keeps one `BodyProfile` per user in `ZestEngine/body.ron` in the user's
config folder (`%APPDATA%`, `~/Library/Application Support`,
`$XDG_CONFIG_HOME` or `~/.config`; `ZESTENGINE_CONFIG_DIR` overrides it; on Android
the app's internal storage). Every ZestEngine game and the editor share it, so a
player measures once. It holds:

- `measured_eye_height`: the headset's height above the floor while standing
  straight, averaged over one second. Samples further than three median
  absolute deviations (at least 1 cm) from the median are dropped; the
  measurement is refused if the headset moved more than 6 cm.
- `height_override_cm`: the player's own entry (top of the head), which wins
  over the measurement. Standing height = eye height + 11 cm
  (`EYE_TO_CROWN`).
- `arm_span`: controller-to-controller distance in the T-pose.
- `floor_offset`: a play-area floor correction from the floor check, taken off
  every tracked height.
- `roles`: device id -> tracker role (or ignored). Devices not listed keep the
  role they report.
- `tpose`: the stage-space tracker poses of the last T-pose. Tracker offsets are
  solved from it for whichever avatar is worn, so switching avatars needs no
  new T-pose.

## Tracker devices

Head and hands always come from the headset and controllers. Body trackers
come from two places:

- The OpenXR runtime's trackers (`XR_HTCX_vive_tracker_interaction`), one
  device per role path: `xr:waist`, `xr:left_foot`, ... The runtime decides
  which tracker sits on which path (SteamVR's tracker roles); ZestEngine can move a
  path to another body role or ignore it.
- SlimeVR, through the server's driver bridge (`slimevr.rs`), one device per
  server tracker keyed by its serial: `slimevr:<serial>`.

Each frame `BodyTracking::assemble` puts every device in its role's slot (the
player's choice, else the device's own; the first device on a slot wins) and
applies the floor offset.

## Calibration

The body menu runs three guided steps, each with a prompt and a three second
countdown:

1. Stand straight: one second of headset samples gives the eye height.
2. T-pose: arms out at shoulder height. Held for 0.6 s, then captured. In a
   full calibration with "Assign roles from the T-pose" on, every tracker is
   matched to the body role whose T-pose position it is nearest to (relative to
   the headset's position and facing, scaled by the eye height and arm span;
   each role used once, solved exactly over all trackers at once, so left and
   right never swap; a tracker far from every role stays unassigned). The arm
   span is taken from the controllers.
3. Floor check (optional): put a controller on the floor and pull its trigger.
   The floor moves so that controller sits 3.5 cm up; the measurements taken
   against the old floor move with it. A trigger pulled higher than 35 cm is
   ignored (that was a click on the panel). Skip leaves the floor as it is.

Quick recalibrate runs only the T-pose and keeps the roles. Height runs only the
first step. `ctx.calibrate_trackers()` captures the T-pose on the spot.

The offsets come from `humanoid::calibrate_tpose`: the avatar's rest pose, at
the size the player stands in it (see `model_to_stage` below), is lined up under
the headset and turned to face where it faces, and each tracker stores its
rigid offset to its bone. That offset carries the difference between where the
player's hips and feet are and where the avatar's are, so standing straight puts
the avatar's hips and feet at its own rest heights. Before any T-pose,
`default_calibration` maps an ideal headset and controllers onto the bones.

## Fitting the avatar

`VR Player` has `avatar_scaling` (default Scale avatar) and `match_arm_span`:

- Scale avatar: the avatar is scaled uniformly so its eyes sit at the
  player's eyes. Its arms scale with it.
- Scale player: the avatar keeps its size and the VR Player rig (the tracking
  space) is scaled instead, so the player sees from the avatar's eyes, as a
  giant or a child would. The headset view, hands and locomotion all follow the
  rig's scale. An avatar parented under the rig is scaled back so it keeps its
  size.
- Off: nothing is scaled.

The avatar's eye height comes from its eye bones, else its head bone plus a
proportional offset, measured from the floor it stands on (`measure_avatar`);
its arm span is wrist to wrist with the arms straight. `fit_avatar` returns the
avatar factor, the tracking factor and `model_to_stage` (avatar model units to
tracking-space meters, the same in both scaling modes: the avatar is
player-sized as the player stands in it).

With Match arm span, hand targets move `avatar span / player span` as far from
the shoulders (`BodyIkSettings::arm_reach`), so long or short avatar arms reach
naturally without non-uniform scaling.

The fit scales the avatar's Avatar Descriptor root (a worn package), else the
Humanoid IK object, and writes only scales it has not been told about: if
gameplay changes the avatar's scale, that becomes its new authored size. Fits
run only in Play (the editor never saves a fitted scale) and only while a
headset is running.

## Networking

The owner's Network Avatar (Drive From VR or Follow Local Player) sends the
fitted avatar scale and the measured eye height as the synced variables
`zestengine.avatar_scale` and `zestengine.eye_height` (reliable, late joiners catch
up). Remote copies expose them as `avatar_scale` / `player_eye_height` and the
avatar under a remote Network Avatar is scaled by `avatar_scale`, so everyone
sees the same size. The replicated tracker targets are already calibrated
bone targets in world space (in Scale player mode they are at the avatar's
size), so remote IK needs nothing else. A worn avatar under a remote player's
Network Avatar follows that player's replicated targets.

## The body menu

In a headset, the VR Player's Body menu action (`Menu`, the menu button in the
VR Controllers scheme) opens a panel in front of the player: Calibrate, Quick
recalibrate, Height and Done, the height with +/- 1 cm and "Use measured", the
detected trackers with their roles (cycle with < and >; duplicates show red),
and a live front view of the posed avatar with the trackers drawn on it.
Point at it with either controller and pull the trigger, or poke it with a
controller. During calibration it shows the step, the prompt and the
countdown. Set the action empty to keep the button for the game.

In the editor, Windows > Body Tracking shows the same panel as a window (roles
can be set there without a headset; trackers seen before are listed), and the
VR Tools window's Body Calibration opens the in-headset panel in the editor's
VR mode.

## Gameplay API

```rust
let h = ctx.player_height();        // m, to the top of the head
let s = ctx.avatar_scale();         // the local avatar's fit
let body = ctx.body();              // eye height, arm span, measured, calibrated, ...
ctx.request_height_calibration();   // stand straight for a moment
ctx.start_body_calibration();       // the full guided flow
ctx.show_body_menu(true);
ctx.set_player_height_cm(Some(172.0));
ctx.calibrate_trackers();           // capture the T-pose now
```

`ctx.screen.body` is the same `BodyInfo`.

## Limits

- Runtime trackers are identified by their OpenXR role path, not their
  serial: `XR_HTCX_vive_tracker_interaction` binds actions per role path, and
  `XR_MNDX_xdev_space` is not used yet. Moving a physical tracker to another
  SteamVR role moves its saved ZestEngine role with it.
- Fingers are not tracked yet.
