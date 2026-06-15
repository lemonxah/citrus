//! Egui-free data models shared by the runtime (scene/shader handling) and the
//! editor inspector. These describe material and shader state as plain data;
//! the editor builds its egui UI on top of them, the runtime reads them when
//! loading scenes. Neither needs the other's crate.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AlphaModeModel {
    Opaque,
    Cutout,
    Blend,
}

impl AlphaModeModel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Opaque => "Opaque",
            Self::Cutout => "Cutout",
            Self::Blend => "Transparent",
        }
    }

    /// Default render queue for this alpha mode (Unity-style breakpoints).
    pub fn default_render_queue(self) -> i32 {
        match self {
            Self::Opaque => 2000, // Geometry
            Self::Cutout => 2450, // AlphaTest
            Self::Blend => 3000,  // Transparent
        }
    }
}

/// Project-relative texture paths for a material's 12 sampler slots. Mirrors
/// `citrus_assets::MaterialTextures`; kept here (egui-free) so the inspector can
/// edit texture assignments as plain data and the engine can resolve them to
/// GPU handles at apply time. `None` = use the material's import-embedded
/// texture for that slot (or the 1×1 default if there is none).
#[derive(Clone, PartialEq, Eq, Default, Debug)]
pub struct MaterialTexturePaths {
    pub albedo: Option<std::path::PathBuf>,
    pub normal: Option<std::path::PathBuf>,
    pub orm: Option<std::path::PathBuf>,
    pub emission: Option<std::path::PathBuf>,
    pub opacity: Option<std::path::PathBuf>,
    pub emission_mask: Option<std::path::PathBuf>,
    pub matcap: [Option<std::path::PathBuf>; 3],
    pub matcap_mask: [Option<std::path::PathBuf>; 3],
}

/// Display labels for the 12 texture slots, in binding order. Index matches
/// [`MaterialTexturePaths::slot_mut`].
pub const TEXTURE_SLOT_LABELS: [&str; 12] = [
    "Albedo",
    "Normal Map",
    "ORM (Occl/Rough/Metal)",
    "Emission",
    "Opacity",
    "Emission Mask",
    "Matcap 1",
    "Matcap 1 Mask",
    "Matcap 2",
    "Matcap 2 Mask",
    "Matcap 3",
    "Matcap 3 Mask",
];

impl MaterialTexturePaths {
    /// Mutable access to slot `i` (0..12) in binding order; `None` if out of range.
    pub fn slot_mut(&mut self, i: usize) -> Option<&mut Option<std::path::PathBuf>> {
        Some(match i {
            0 => &mut self.albedo,
            1 => &mut self.normal,
            2 => &mut self.orm,
            3 => &mut self.emission,
            4 => &mut self.opacity,
            5 => &mut self.emission_mask,
            6 => &mut self.matcap[0],
            7 => &mut self.matcap_mask[0],
            8 => &mut self.matcap[1],
            9 => &mut self.matcap_mask[1],
            10 => &mut self.matcap[2],
            11 => &mut self.matcap_mask[2],
            _ => return None,
        })
    }
}

/// How a matcap layer combines with the shaded colour beneath it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum MatcapBlend {
    /// Add the matcap on top (classic sphere-map highlight).
    #[default]
    Add,
    /// Multiply with the base colour (tint / shading).
    Multiply,
    /// Replace the base colour (masked overlay).
    Replace,
}

impl MatcapBlend {
    pub fn label(self) -> &'static str {
        match self {
            Self::Add => "Add",
            Self::Multiply => "Multiply",
            Self::Replace => "Replace",
        }
    }

    pub const ALL: [MatcapBlend; 3] = [Self::Add, Self::Multiply, Self::Replace];

    pub fn to_f32(self) -> f32 {
        match self {
            Self::Add => 0.0,
            Self::Multiply => 1.0,
            Self::Replace => 2.0,
        }
    }

    pub fn from_f32(v: f32) -> Self {
        match v as i32 {
            1 => Self::Multiply,
            2 => Self::Replace,
            _ => Self::Add,
        }
    }
}

/// View of one material (standard or custom shader) as plain values.
#[derive(Clone, PartialEq)]
pub struct MaterialModel {
    pub name: String,
    /// "standard" or a project-relative `.frag` custom shader path.
    pub shader: String,
    /// Custom-shader property values, packed by property offset (16 floats
    /// once initialized; empty = take the shader's defaults).
    pub custom_values: Vec<f32>,
    pub base_color: [f32; 4],
    pub metallic: f32,
    pub roughness: f32,
    pub occlusion_strength: f32,
    pub toon_enabled: bool,
    pub toon_steps: f32,
    pub pbr_toon_blend: f32,
    pub emission_enabled: bool,
    pub emission_color: [f32; 3],
    pub emission_intensity: f32,
    pub alpha_mode: AlphaModeModel,
    pub alpha_cutoff: f32,
    pub has_normal_texture: bool,
    pub normal_map_enabled: bool,
    pub normal_strength: f32,
    pub double_sided: bool,
    // Extended "FX" params (per-material UBO): Toon rim + animated emission.
    pub rim_color: [f32; 3],
    pub rim_power: f32,
    pub rim_strength: f32,
    pub ramp_smoothness: f32,
    pub emission_scroll: [f32; 2],
    pub emission_pulse: f32,
    /// Additive matcap layer strengths (0 = off). Matcap textures are assigned
    /// via the `.material` file's texture slots.
    pub matcap_strength: [f32; 3],
    /// Per-layer matcap blend mode (how each layer combines with the colour).
    pub matcap_blend: [MatcapBlend; 3],
    /// Draw-order priority (Unity render queue): Geometry 2000, AlphaTest
    /// 2450, Transparent 3000, Overlay 4000.
    pub render_queue: i32,
    /// Per-slot texture assignments (project-relative). Editable in the
    /// inspector; resolved to GPU handles when the material is applied.
    pub textures: MaterialTexturePaths,
}

/// Reflected custom-shader property kinds (mirrors the pragma metadata parsed
/// by citrus-assets; the engine converts between the two).
#[derive(Clone, Copy, Debug)]
pub enum ShaderPropKindUi {
    Float { min: f32, max: f32 },
    Toggle,
    Color,
    Color3,
}

#[derive(Clone, Debug)]
pub struct ShaderPropUi {
    pub label: String,
    pub kind: ShaderPropKindUi,
    /// Flat float offset into `MaterialModel::custom_values`.
    pub offset: usize,
}

/// Everything the inspector needs to draw a custom shader's material UI.
#[derive(Clone, Debug, Default)]
pub struct ShaderUiInfo {
    pub display_name: String,
    pub props: Vec<ShaderPropUi>,
    /// Compile/parse error; shown instead of properties.
    pub error: Option<String>,
}
