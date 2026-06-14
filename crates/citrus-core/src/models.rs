//! Egui-free data models shared by the runtime (scene/shader handling) and the
//! editor inspector. These describe material and shader state as plain data;
//! the editor builds its egui UI on top of them, the runtime reads them when
//! loading scenes — neither needs the other's crate.

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
    /// Draw-order priority (Unity render queue): Geometry 2000, AlphaTest
    /// 2450, Transparent 3000, Overlay 4000.
    pub render_queue: i32,
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
