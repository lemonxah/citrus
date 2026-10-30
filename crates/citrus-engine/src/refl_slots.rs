//! Two-slot reflection probes. The renderer blends the two highest-priority
//! probes around the camera per pixel (slot 0 = primary, binding 7; slot 1 =
//! secondary, binding 9). This keeps each slot's cube matching its probe as the
//! camera moves: baked probes upload their faces from the scene's
//! `.reflprobe`, realtime probes (and baked ones without a cube yet) capture
//! live, and two probes trading places swap slots instead of reloading.

use std::collections::HashMap;
use std::path::Path;

use citrus_render::{ReflectionProbeBox, Renderer, TextureData};

pub type Faces = [TextureData; 6];

/// Baked probe cubes from a scene's `.reflprobe` sidecar.
#[derive(Default)]
pub struct ReflProbeSet {
    /// One cube per probe object id (format v3).
    pub entries: HashMap<u128, Faces>,
    /// A v1/v2 single-cube file: used by any baked probe without its own entry.
    pub legacy: Option<Faces>,
}

impl ReflProbeSet {
    pub fn faces(&self, id: u128) -> Option<&Faces> {
        self.entries.get(&id).or(self.legacy.as_ref())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.legacy.is_none()
    }
}

fn read_faces(bytes: &[u8], at: &mut usize, size: u32, hdr: bool) -> Option<Faces> {
    let face_len = (size as usize) * (size as usize) * if hdr { 8 } else { 4 };
    if bytes.len() < *at + face_len * 6 {
        return None;
    }
    let faces: Vec<TextureData> = (0..6)
        .map(|i| TextureData {
            width: size,
            height: size,
            pixels: bytes[*at + i * face_len..*at + (i + 1) * face_len].to_vec(),
            srgb: false,
            hdr,
        })
        .collect();
    *at += face_len * 6;
    faces.try_into().ok()
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

/// Parse a `.reflprobe` sidecar. `CITRSRP1` (RGBA8) / `CITRSRP2` (RGBA16F) hold
/// one cube; `CITRSRP3` holds `count` entries of (probe id u128, face size,
/// hdr flag, six faces). Empty when missing or malformed.
pub fn read_reflprobes(path: &Path) -> ReflProbeSet {
    let mut set = ReflProbeSet::default();
    let Ok(bytes) = std::fs::read(path) else {
        return set;
    };
    if bytes.len() < 16 {
        return set;
    }
    match &bytes[..8] {
        magic @ (b"CITRSRP1" | b"CITRSRP2") => {
            let hdr = magic == b"CITRSRP2";
            let (Some(size), Some(count)) = (u32_at(&bytes, 8), u32_at(&bytes, 12)) else {
                return set;
            };
            let mut at = 16;
            if count == 6 {
                set.legacy = read_faces(&bytes, &mut at, size, hdr);
            }
        }
        b"CITRSRP3" => {
            let Some(count) = u32_at(&bytes, 8) else {
                return set;
            };
            let mut at = 12;
            for _ in 0..count {
                let Some(id) = bytes.get(at..at + 16).and_then(|b| b.try_into().ok()).map(u128::from_le_bytes) else {
                    break;
                };
                let (Some(size), Some(hdr)) = (u32_at(&bytes, at + 16), u32_at(&bytes, at + 20)) else {
                    break;
                };
                at += 24;
                match read_faces(&bytes, &mut at, size, hdr != 0) {
                    Some(f) => {
                        set.entries.insert(id, f);
                    }
                    None => break,
                }
            }
        }
        _ => {}
    }
    set
}

/// Serialize `set` as a v3 `.reflprobe` (the legacy cube is dropped once
/// per-probe entries exist).
pub fn write_reflprobes(path: &Path, set: &ReflProbeSet) -> std::io::Result<()> {
    let mut out = Vec::new();
    out.extend_from_slice(b"CITRSRP3");
    let mut ids: Vec<&u128> = set.entries.keys().collect();
    ids.sort();
    out.extend_from_slice(&(ids.len() as u32).to_le_bytes());
    for id in ids {
        let faces = &set.entries[id];
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&faces[0].width.to_le_bytes());
        out.extend_from_slice(&u32::from(faces[0].hdr).to_le_bytes());
        for f in faces {
            out.extend_from_slice(&f.pixels);
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, out)
}

/// Whether probe `a` wins over `b` where their boxes overlap: higher
/// importance, then the smaller box.
fn on_top(a: &ReflectionProbeBox, b: &ReflectionProbeBox) -> bool {
    let volume = |p: &ReflectionProbeBox| p.half_extents.iter().product::<f32>();
    (a.importance, -volume(a)) > (b.importance, -volume(b))
}

/// Which probe's cube each renderer slot holds.
#[derive(Default)]
pub struct ReflectionSlots {
    baked: ReflProbeSet,
    held: [Option<u128>; 2],
    /// Slot waiting for a live capture (realtime probe, or baked without a cube).
    needs_capture: [bool; 2],
    /// Slot's cube belongs to its probe (uploaded, or its capture finished).
    ready: [bool; 2],
    /// Slot whose live capture is in flight.
    capturing: Option<usize>,
    /// Realtime refreshes alternate between the slots.
    next_refresh: usize,
}

impl ReflectionSlots {
    /// New baked cubes (scene load / after a bake); every slot reloads.
    pub fn set_baked(&mut self, baked: ReflProbeSet) {
        self.baked = baked;
        self.reset();
    }

    /// Forget what the slots hold so the next `update` reloads / recaptures.
    pub fn reset(&mut self) {
        self.held = [None; 2];
        self.needs_capture = [false; 2];
        self.ready = [false; 2];
        self.capturing = None;
    }

    /// Per frame: fit the slots to `probes` (best first, from
    /// `LoadedScene::active_reflection_probes`) and return the FrameInput
    /// fields (slot 0 box, slot 1 box, primary slot). `allow_capture` is false
    /// while a bake owns the capture.
    pub fn update(
        &mut self,
        renderer: &mut Renderer,
        probes: [Option<ReflectionProbeBox>; 2],
        allow_capture: bool,
    ) -> (Option<ReflectionProbeBox>, Option<ReflectionProbeBox>, u8) {
        let want = probes.map(|p| p.map(|p| p.id));
        // The camera crossed from one probe's box into the other's: trade slots.
        if want[0].is_some() && want[0] == self.held[1] && self.held[0] != want[0] {
            renderer.swap_reflection_slots();
            self.held.swap(0, 1);
            self.ready.swap(0, 1);
            self.needs_capture.swap(0, 1);
            self.capturing = self.capturing.map(|s| 1 - s);
        }
        for s in 0..2 {
            if want[s] == self.held[s] {
                continue;
            }
            self.held[s] = want[s];
            self.ready[s] = false;
            self.needs_capture[s] = false;
            if self.capturing == Some(s) {
                self.capturing = None;
            }
            let Some(p) = probes[s] else {
                continue;
            };
            match (!p.realtime).then(|| self.baked.faces(p.id)).flatten() {
                Some(faces) => match renderer.set_reflection_faces_slot(s as u8, faces) {
                    Ok(()) => self.ready[s] = true,
                    Err(e) => {
                        tracing::error!("uploading baked reflection probe: {e:#}");
                        self.needs_capture[s] = true;
                    }
                },
                None => self.needs_capture[s] = true,
            }
        }
        if let Some(s) = self.capturing
            && !renderer.reflection_capture_in_flight()
        {
            self.ready[s] = true;
            self.capturing = None;
        }
        if allow_capture && self.capturing.is_none() && !renderer.reflection_capture_in_flight() {
            // A slot still waiting for its first cube goes first; then realtime
            // probes refresh, alternating so both track dynamic lighting.
            let first = (0..2).find(|&s| self.needs_capture[s] && probes[s].is_some());
            let refresh = || {
                [self.next_refresh, 1 - self.next_refresh]
                    .into_iter()
                    .find(|&s| probes[s].is_some_and(|p| p.realtime))
            };
            if let Some(s) = first.or_else(refresh) {
                let p = probes[s].unwrap();
                renderer.request_reflection_capture_slot(
                    glam::Vec3::from(p.capture),
                    p.resolution,
                    s as u8,
                    if p.update_hz > 0.0 { 1.0 / p.update_hz } else { 0.0 },
                    p.time_slicing,
                    p.include_dynamic,
                );
                self.needs_capture[s] = false;
                self.capturing = Some(s);
                self.next_refresh = 1 - s;
            }
        }
        // A secondary whose cube isn't its own yet would blend the wrong
        // reflection in; leave it out until it is.
        let second = probes[1].filter(|_| self.ready[1]);
        (probes[0], second, second.map_or(0, |b| u8::from(probes[0].is_some_and(|a| on_top(&b, &a)))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn faces(size: u32, v: u8) -> Faces {
        std::array::from_fn(|_| TextureData {
            width: size,
            height: size,
            pixels: vec![v; (size * size * 8) as usize],
            srgb: false,
            hdr: true,
        })
    }

    #[test]
    fn reflprobe_v3_round_trips_per_probe_cubes() {
        let dir = std::env::temp_dir().join(format!("citrus-reflprobe-{}", std::process::id()));
        let path = dir.join("scene.reflprobe");
        let mut set = ReflProbeSet::default();
        set.entries.insert(7, faces(4, 1));
        set.entries.insert(u128::MAX - 3, faces(8, 2));
        write_reflprobes(&path, &set).unwrap();
        let back = read_reflprobes(&path);
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.faces(7).unwrap()[0].width, 4);
        assert_eq!(back.faces(u128::MAX - 3).unwrap()[5].pixels[0], 2);
        assert!(back.faces(99).is_none(), "no legacy cube to fall back to");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_single_cube_serves_any_baked_probe() {
        let dir = std::env::temp_dir().join(format!("citrus-reflprobe-legacy-{}", std::process::id()));
        let path = dir.join("scene.reflprobe");
        std::fs::create_dir_all(&dir).unwrap();
        let mut bytes = b"CITRSRP2".to_vec();
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&6u32.to_le_bytes());
        bytes.extend(std::iter::repeat_n(5u8, 2 * 2 * 8 * 6));
        std::fs::write(&path, bytes).unwrap();
        let set = read_reflprobes(&path);
        assert!(set.entries.is_empty());
        assert_eq!(set.faces(12345).unwrap()[0].pixels[0], 5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
