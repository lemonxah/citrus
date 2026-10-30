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
    /// Split AO / Roughness / Metallic maps (slots 12/13/14).
    pub ao: Option<std::path::PathBuf>,
    pub roughness: Option<std::path::PathBuf>,
    pub metallic: Option<std::path::PathBuf>,
    /// Height / displacement map (slot 15) for parallax occlusion mapping.
    pub displacement: Option<std::path::PathBuf>,
    /// Custom-shader texture properties (slots 16..), in the shader's texture
    /// declaration order: (property name, path). Synced to the shader layout
    /// by the engine; saved by name.
    pub shader: Vec<(String, Option<std::path::PathBuf>)>,
}

/// Display labels for the 15 texture slots, in binding order. Index matches
/// [`MaterialTexturePaths::slot_mut`].
pub const TEXTURE_SLOT_LABELS: [&str; 16] = [
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
    "Ambient Occlusion",
    "Roughness",
    "Metallic",
    "Displacement",
];

impl MaterialTexturePaths {
    /// Mutable access to slot `i` (0..15) in binding order; `None` if out of range.
    pub fn slot_mut(&mut self, i: usize) -> Option<&mut Option<std::path::PathBuf>> {
        if i >= 16 {
            return self.shader.get_mut(i - 16).map(|(_, p)| p);
        }
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
            12 => &mut self.ao,
            13 => &mut self.roughness,
            14 => &mut self.metallic,
            15 => &mut self.displacement,
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
    /// Invert a split AO / Roughness / Metallic map (e.g. smoothness→roughness).
    pub ao_invert: bool,
    pub roughness_invert: bool,
    pub metallic_invert: bool,
    /// Parallax occlusion mapping strength for the displacement map (0 = off).
    pub displacement_scale: f32,
    /// Per-material reflection strength (1 = full, 0 = matte/no reflections).
    /// Scales BOTH the environment-cube probe reflection and the screen-space /
    /// ray-traced reflection for this material, so reflection technique + look can
    /// be mixed and matched per material without changing the global model.
    pub reflection_intensity: f32,
    /// Per-material screen-space / ray-traced reflection toggle. When false this
    /// material still gets the environment / reflection-probe CUBE reflection but
    /// the deferred SSR/RT resolve skips it (reflectivity 0 in the G-buffer) — so
    /// reflection TECHNIQUE can be mixed per material (cube-only vs cube + SSR/RT).
    pub screen_reflections: bool,
    /// Per-texture UV tiling (xy scale, default 1) + offset (default 0) for the
    /// main maps; masks follow their parent map.
    pub albedo_tiling: [f32; 2],
    pub albedo_offset: [f32; 2],
    pub normal_tiling: [f32; 2],
    pub normal_offset: [f32; 2],
    pub orm_tiling: [f32; 2],
    pub orm_offset: [f32; 2],
    pub emission_tiling: [f32; 2],
    pub emission_offset: [f32; 2],
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

/// Reflected custom-shader property kinds (mirrors the declarations parsed
/// by citrus-assets; the engine converts between the two).
#[derive(Clone, Debug, PartialEq)]
pub enum ShaderPropKindUi {
    /// Slider.
    Float { min: f32, max: f32 },
    /// Unbounded drag value.
    Drag,
    /// Integer; `min == max` = unbounded.
    Int { min: i32, max: i32 },
    Toggle,
    Enum(Vec<String>),
    Color { hdr: bool },
    Color3 { hdr: bool },
    Vector(u8),
    /// Texture picker for `MaterialTexturePaths` slot `slot`; the property's
    /// four values are its tiling (xy) and offset (zw).
    Texture { slot: usize, default: &'static str },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShaderCmpUi {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// Visibility condition: OR of AND-groups of (value offset, op, value).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShaderCondUi(pub Vec<Vec<(usize, ShaderCmpUi, f32)>>);

impl ShaderCondUi {
    pub fn eval(&self, values: &[f32]) -> bool {
        self.0.is_empty()
            || self.0.iter().any(|all| {
                all.iter().all(|&(at, op, b)| {
                    let a = values.get(at).copied().unwrap_or(0.0);
                    match op {
                        ShaderCmpUi::Eq => (a - b).abs() < 0.5,
                        ShaderCmpUi::Ne => (a - b).abs() >= 0.5,
                        ShaderCmpUi::Lt => a < b,
                        ShaderCmpUi::Le => a <= b,
                        ShaderCmpUi::Gt => a > b,
                        ShaderCmpUi::Ge => a >= b,
                    }
                })
            })
    }
}

#[derive(Clone, Debug)]
pub struct ShaderPropUi {
    pub name: String,
    pub label: String,
    pub kind: ShaderPropKindUi,
    /// Flat float offset into `MaterialModel::custom_values`.
    pub offset: usize,
    /// Specialization-constant keyword (changing it selects another variant).
    pub keyword: bool,
    /// Index into `ShaderUiInfo::groups`.
    pub group: Option<usize>,
    pub header: Option<String>,
    pub show_if: ShaderCondUi,
    pub tooltip: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ShaderGroupUi {
    pub label: String,
    /// Header checkbox: index into `ShaderUiInfo::props`.
    pub toggle: Option<usize>,
    pub show_if: ShaderCondUi,
}

#[derive(Clone, Debug)]
pub struct ShaderPresetUi {
    pub name: String,
    /// (value offset, values).
    pub values: Vec<(usize, Vec<f32>)>,
}

/// Everything the inspector needs to draw a custom shader's material UI.
#[derive(Clone, Debug, Default)]
pub struct ShaderUiInfo {
    pub display_name: String,
    /// "GLSL" or "HLSL".
    pub language: &'static str,
    pub props: Vec<ShaderPropUi>,
    pub groups: Vec<ShaderGroupUi>,
    pub presets: Vec<ShaderPresetUi>,
    /// Compile/parse error; shown instead of properties.
    pub error: Option<String>,
}

impl ShaderUiInfo {
    /// Apply preset `i` to a material's values.
    pub fn apply_preset(&self, i: usize, values: &mut [f32]) {
        if let Some(p) = self.presets.get(i) {
            for (at, v) in &p.values {
                if at + v.len() <= values.len() {
                    values[*at..at + v.len()].copy_from_slice(v);
                }
            }
        }
    }

    /// Index of the preset whose assignments all match `values`.
    pub fn matching_preset(&self, values: &[f32]) -> Option<usize> {
        self.presets.iter().position(|p| {
            p.values
                .iter()
                .all(|(at, v)| values.get(*at..at + v.len()).is_some_and(|cur| cur == v.as_slice()))
        })
    }

    /// Enabled keyword count (variant summary).
    pub fn active_keywords(&self, values: &[f32]) -> usize {
        self.props
            .iter()
            .filter(|p| p.keyword && values.get(p.offset).is_some_and(|v| *v != 0.0))
            .count()
    }
}
