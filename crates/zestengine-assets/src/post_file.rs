//! `.postfx` post-processing profile assets (RON), Unity Volume-style: a file
//! holding every effect's settings. A `Volume` component references one; the
//! camera blends the volumes affecting it into an effective profile that drives
//! the post pass.

use std::path::Path;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

pub const POSTFX_EXTENSION: &str = "postfx";

/// Tonemap operator applied at the end of the post stack (HDR → display).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TonemapMode {
    None,
    Reinhard,
    #[default]
    Aces,
}

impl TonemapMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Reinhard => "Reinhard",
            Self::Aces => "ACES",
        }
    }
    pub const ALL: [TonemapMode; 3] = [Self::None, Self::Reinhard, Self::Aces];
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Tonemap {
    /// In a Volume profile: whether this profile overrides the tonemap mode and
    /// exposure underneath it. Ignored for the scene's global (Environment) post.
    #[serde(default)]
    pub enabled: bool,
    pub mode: TonemapMode,
    /// Exposure in stops (EV): final color is multiplied by 2^exposure.
    pub exposure: f32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Bloom {
    pub enabled: bool,
    /// Luminance above which pixels bloom.
    pub threshold: f32,
    /// Strength of the added glow.
    pub intensity: f32,
    /// Blur spread (0..1, fraction of screen).
    pub radius: f32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorGrading {
    pub enabled: bool,
    /// Post-exposure multiplier (separate from the tonemap EV).
    pub exposure: f32,
    /// 1.0 = neutral; >1 increases contrast around mid-grey.
    pub contrast: f32,
    /// 1.0 = neutral; 0 = greyscale.
    pub saturation: f32,
    /// -1 (cool) .. +1 (warm) white-balance shift.
    pub temperature: f32,
    /// -1 (green) .. +1 (magenta) white-balance shift.
    pub tint: f32,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Vignette {
    pub enabled: bool,
    /// Darkening strength at the edges.
    pub intensity: f32,
    /// Edge softness.
    pub smoothness: f32,
    pub color: [f32; 3],
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ChromaticAberration {
    pub enabled: bool,
    /// Channel-split strength toward the edges.
    pub intensity: f32,
}

/// A complete post-processing profile: one of every effect's settings.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct PostFxProfile {
    pub tonemap: Tonemap,
    pub bloom: Bloom,
    pub color_grading: ColorGrading,
    pub vignette: Vignette,
    pub chromatic_aberration: ChromaticAberration,
}

impl Default for Tonemap {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: TonemapMode::Aces,
            exposure: 0.0,
        }
    }
}
impl Default for Bloom {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: 1.0,
            intensity: 0.5,
            radius: 0.5,
        }
    }
}
impl Default for ColorGrading {
    fn default() -> Self {
        Self {
            enabled: false,
            exposure: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            temperature: 0.0,
            tint: 0.0,
        }
    }
}
impl Default for Vignette {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: 0.4,
            smoothness: 0.4,
            color: [0.0, 0.0, 0.0],
        }
    }
}
impl Default for ChromaticAberration {
    fn default() -> Self {
        Self {
            enabled: false,
            intensity: 0.3,
        }
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [lerp(a[0], b[0], t), lerp(a[1], b[1], t), lerp(a[2], b[2], t)]
}

impl PostFxProfile {
    /// Blend `t` of the way from `self` toward `other` (Volume weight blend).
    /// Numeric parameters lerp; the tonemap mode switches to `other` once it's
    /// the dominant contributor (`t >= 0.5`). An effect `other` leaves disabled
    /// (or a tonemap it doesn't override)
    /// doesn't override (Unity's per-effect override): a volume that only sets
    /// up colour grading keeps the bloom underneath it. An effect enabled in
    /// `other` but not in `self` fades in from its neutral setting.
    pub fn lerp(&self, other: &Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let dom = t >= 0.5;
        let bloom = if !other.bloom.enabled {
            self.bloom
        } else {
            let from = if self.bloom.enabled { self.bloom } else { Bloom { intensity: 0.0, ..other.bloom } };
            Bloom {
                enabled: true,
                threshold: lerp(from.threshold, other.bloom.threshold, t),
                intensity: lerp(from.intensity, other.bloom.intensity, t),
                radius: lerp(from.radius, other.bloom.radius, t),
            }
        };
        let color_grading = if !other.color_grading.enabled {
            self.color_grading
        } else {
            let from = if self.color_grading.enabled {
                self.color_grading
            } else {
                ColorGrading { enabled: true, exposure: 0.0, contrast: 1.0, saturation: 1.0, temperature: 0.0, tint: 0.0 }
            };
            let o = &other.color_grading;
            ColorGrading {
                enabled: true,
                exposure: lerp(from.exposure, o.exposure, t),
                contrast: lerp(from.contrast, o.contrast, t),
                saturation: lerp(from.saturation, o.saturation, t),
                temperature: lerp(from.temperature, o.temperature, t),
                tint: lerp(from.tint, o.tint, t),
            }
        };
        let vignette = if !other.vignette.enabled {
            self.vignette
        } else {
            let from = if self.vignette.enabled { self.vignette } else { Vignette { intensity: 0.0, ..other.vignette } };
            Vignette {
                enabled: true,
                intensity: lerp(from.intensity, other.vignette.intensity, t),
                smoothness: lerp(from.smoothness, other.vignette.smoothness, t),
                color: lerp3(from.color, other.vignette.color, t),
            }
        };
        let chromatic_aberration = if !other.chromatic_aberration.enabled {
            self.chromatic_aberration
        } else {
            let from = if self.chromatic_aberration.enabled { self.chromatic_aberration.intensity } else { 0.0 };
            ChromaticAberration { enabled: true, intensity: lerp(from, other.chromatic_aberration.intensity, t) }
        };
        let tonemap = if !other.tonemap.enabled {
            self.tonemap
        } else {
            Tonemap {
                enabled: true,
                mode: if dom { other.tonemap.mode } else { self.tonemap.mode },
                exposure: lerp(self.tonemap.exposure, other.tonemap.exposure, t),
            }
        };
        Self {
            tonemap,
            bloom,
            color_grading,
            vignette,
            chromatic_aberration,
        }
    }
}

/// Blend Volume profiles (sorted by ascending priority, each with a 0..1
/// weight) over `base`, the scene's global post settings, which apply in full.
pub fn blend_over(base: PostFxProfile, stack: &[(PostFxProfile, f32)]) -> PostFxProfile {
    let mut result = base;
    for (profile, weight) in stack {
        result = result.lerp(profile, weight.clamp(0.0, 1.0));
    }
    result
}

/// Blend a stack of (profile, weight) by ascending priority into one effective
/// profile. Caller supplies them already sorted by priority (low → high); each
/// is blended over the running result by its weight (0..1). The base is the
/// default profile so an empty stack yields neutral settings.
pub fn blend_profiles(stack: &[(PostFxProfile, f32)]) -> PostFxProfile {
    let mut result = PostFxProfile::default();
    for (profile, weight) in stack {
        result = result.lerp(profile, weight.clamp(0.0, 1.0));
    }
    result
}

pub fn load_postfx(path: impl AsRef<Path>) -> Result<PostFxProfile> {
    let path = path.as_ref();
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    ron::from_str(&text).with_context(|| format!("parsing postfx {}", path.display()))
}

pub fn save_postfx(path: impl AsRef<Path>, profile: &PostFxProfile) -> Result<()> {
    let path = path.as_ref();
    let text = ron::ser::to_string_pretty(profile, ron::ser::PrettyConfig::default())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_weights_toward_profile() {
        let mut a = PostFxProfile::default();
        a.color_grading.contrast = 1.0;
        let mut b = PostFxProfile::default();
        b.color_grading.enabled = true;
        b.color_grading.contrast = 2.0;

        // Full weight → fully b.
        let full = blend_profiles(&[(b, 1.0)]);
        assert!(full.color_grading.enabled);
        assert!((full.color_grading.contrast - 2.0).abs() < 1e-5);

        // Half weight from a disabled base fades in from neutral contrast.
        let half = a.lerp(&b, 0.5);
        assert!((half.color_grading.contrast - 1.5).abs() < 1e-5);
    }

    #[test]
    fn disabled_effect_does_not_override() {
        let mut env = PostFxProfile::default();
        env.bloom.enabled = true;
        env.bloom.intensity = 2.0;
        let mut vol = PostFxProfile::default();
        vol.bloom.enabled = false;
        vol.color_grading.enabled = true;
        vol.color_grading.saturation = 0.5;
        env.tonemap.exposure = 0.5;
        vol.tonemap.exposure = -2.0;
        let out = blend_over(env, &[(vol, 1.0)]);
        assert!((out.tonemap.exposure - 0.5).abs() < 1e-5);
        assert!(out.bloom.enabled);
        assert!((out.bloom.intensity - 2.0).abs() < 1e-5);
        assert!(out.color_grading.enabled);
        assert!((out.color_grading.saturation - 0.5).abs() < 1e-5);
    }
}
