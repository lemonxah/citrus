//! Custom shader files: user-authored GLSL fragment shaders (`.frag`),
//! compiled to SPIR-V at runtime via `glslc` and reflected into inspector
//! properties.
//!
//! A custom shader is a fragment-stage *body* — the engine prepends a
//! preamble declaring the frame UBO, the material's four texture slots,
//! vertex inputs, the output, and a 16-float push-constant block that
//! properties pack into. Properties are declared in `//!` pragma comments:
//!
//! ```glsl
//! //! shader "Wobble"
//! //! prop tint color default(1, 0.5, 0.1, 1)
//! //! prop speed float range(0, 10) default(2)
//!
//! void main() {
//!     vec3 base = texture(t_albedo, v_uv).rgb * tint.rgb;
//!     o_color = vec4(base * (0.7 + 0.3 * sin(u_time * speed)), 1.0);
//! }
//! ```
//!
//! Kinds: `float` (optional `range(min, max)`), `toggle`, `color` (rgba),
//! `color3` (rgb). Each property becomes a `#define` onto the push-constant
//! block, so the body uses property names directly. Do NOT write a
//! `#version` line — the preamble provides it.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

pub const SHADER_EXTENSION: &str = "frag";

/// Total push-constant floats available to properties (4 × vec4).
pub const SHADER_PROP_FLOATS: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ShaderPropKind {
    Float {
        min: f32,
        max: f32,
    },
    Toggle,
    /// RGBA.
    Color,
    /// RGB.
    Color3,
}

impl ShaderPropKind {
    pub fn size(self) -> usize {
        match self {
            Self::Float { .. } | Self::Toggle => 1,
            Self::Color3 => 3,
            Self::Color => 4,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ShaderProp {
    /// GLSL identifier (also the key in saved material files).
    pub name: String,
    /// Inspector label, derived from the name (snake_case → Title Case).
    pub label: String,
    pub kind: ShaderPropKind,
    pub default: Vec<f32>,
    /// Flat float offset into the 16-float push block.
    pub offset: usize,
}

#[derive(Clone, Debug)]
pub struct ShaderSource {
    pub display_name: String,
    pub props: Vec<ShaderProp>,
    pub body: String,
}

impl ShaderSource {
    /// Property defaults packed into the push block.
    pub fn defaults(&self) -> [f32; SHADER_PROP_FLOATS] {
        let mut out = [0.0; SHADER_PROP_FLOATS];
        for prop in &self.props {
            out[prop.offset..prop.offset + prop.kind.size()].copy_from_slice(&prop.default);
        }
        out
    }

    /// Defaults overlaid with named values (saved material data).
    pub fn pack(&self, named: &BTreeMap<String, Vec<f32>>) -> [f32; SHADER_PROP_FLOATS] {
        let mut out = self.defaults();
        for prop in &self.props {
            if let Some(values) = named.get(&prop.name)
                && values.len() == prop.kind.size()
            {
                out[prop.offset..prop.offset + values.len()].copy_from_slice(values);
            }
        }
        out
    }

    /// Packed push-block values → named map for saving.
    pub fn unpack(&self, values: &[f32]) -> BTreeMap<String, Vec<f32>> {
        let mut out = BTreeMap::new();
        for prop in &self.props {
            let end = prop.offset + prop.kind.size();
            if end <= values.len() {
                out.insert(prop.name.clone(), values[prop.offset..end].to_vec());
            }
        }
        out
    }
}

/// Parse pragma declarations and allocate push-block offsets.
pub fn parse_shader_source(text: &str, fallback_name: &str) -> Result<ShaderSource> {
    let mut display_name = fallback_name.to_owned();
    let mut props: Vec<ShaderProp> = Vec::new();
    let mut offset = 0usize;

    for (line_no, line) in text.lines().enumerate() {
        let Some(pragma) = line.trim().strip_prefix("//!") else {
            continue;
        };
        let pragma = pragma.trim();
        if let Some(name) = pragma.strip_prefix("shader") {
            display_name = name.trim().trim_matches('"').to_owned();
        } else if let Some(decl) = pragma.strip_prefix("prop") {
            let prop = parse_prop(decl.trim(), &mut offset)
                .with_context(|| format!("shader pragma on line {}", line_no + 1))?;
            if props.iter().any(|p| p.name == prop.name) {
                bail!("duplicate property {:?} (line {})", prop.name, line_no + 1);
            }
            props.push(prop);
        }
        // Unknown pragmas are ignored (forward compatibility).
    }

    Ok(ShaderSource {
        display_name,
        props,
        body: text.to_owned(),
    })
}

fn parse_prop(decl: &str, offset: &mut usize) -> Result<ShaderProp> {
    let mut parts = decl.split_whitespace();
    let name = parts.next().context("missing property name")?.to_owned();
    if !name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        bail!("property name {name:?} is not a valid GLSL identifier");
    }
    let kind_word = parts.next().context("missing property kind")?;
    let rest: String = parts.collect::<Vec<_>>().join(" ");

    let range = parse_call(&rest, "range")?;
    let default = parse_call(&rest, "default")?;

    let kind = match kind_word {
        "float" => {
            let (min, max) = match range.as_deref() {
                Some([min, max]) => (*min, *max),
                Some(_) => bail!("range() takes two values"),
                None => (0.0, 1.0),
            };
            ShaderPropKind::Float { min, max }
        }
        "toggle" => ShaderPropKind::Toggle,
        "color" => ShaderPropKind::Color,
        "color3" => ShaderPropKind::Color3,
        other => bail!("unknown property kind {other:?} (float, toggle, color, color3)"),
    };

    let size = kind.size();
    let default = match default {
        Some(values) if values.len() == size => values,
        Some(values) => bail!(
            "default() for {name:?} needs {size} value(s), got {}",
            values.len()
        ),
        None => match kind {
            ShaderPropKind::Float { min, .. } => vec![min],
            ShaderPropKind::Toggle => vec![0.0],
            ShaderPropKind::Color => vec![1.0, 1.0, 1.0, 1.0],
            ShaderPropKind::Color3 => vec![1.0, 1.0, 1.0],
        },
    };

    // Allocate the offset; vec-valued properties must not straddle a vec4
    // boundary (their GLSL define is a swizzle of a single vec4).
    let mut at = *offset;
    if (at % 4) + size > 4 {
        at = at.div_ceil(4) * 4;
    }
    // color3 from component 2+ has no contiguous swizzle: align it too.
    if kind == ShaderPropKind::Color3 && at % 4 > 1 {
        at = at.div_ceil(4) * 4;
    }
    if at + size > SHADER_PROP_FLOATS {
        bail!("too many properties: the push block holds {SHADER_PROP_FLOATS} floats");
    }
    *offset = at + size;

    Ok(ShaderProp {
        label: title_case(&name),
        name,
        kind,
        default,
        offset: at,
    })
}

/// Extract `name(a, b, …)` arguments from a pragma tail.
fn parse_call(text: &str, name: &str) -> Result<Option<Vec<f32>>> {
    let Some(start) = text.find(name) else {
        return Ok(None);
    };
    let after = &text[start + name.len()..];
    let Some(open) = after.trim_start().strip_prefix('(') else {
        return Ok(None);
    };
    let close = open
        .find(')')
        .with_context(|| format!("unclosed {name}("))?;
    open[..close]
        .split(',')
        .map(|v| {
            v.trim()
                .parse::<f32>()
                .with_context(|| format!("bad number {:?} in {name}()", v.trim()))
        })
        .collect::<Result<Vec<f32>>>()
        .map(Some)
}

fn title_case(name: &str) -> String {
    name.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The fixed interface every custom shader compiles against. Must match the
/// standard pipeline layout (set 0 frame UBO, set 1 textures, 128-byte push
/// constants) and standard.vert's outputs.
const PREAMBLE: &str = r#"#version 450
layout(set = 0, binding = 0) uniform FrameData {
    mat4 view;
    mat4 proj;
    mat4 view_proj;
    vec4 camera_pos;
    vec4 light_dir;
    vec4 light_color;
    vec4 ambient;
    vec4 misc;
} frame;
layout(set = 1, binding = 0) uniform sampler2D t_albedo;
layout(set = 1, binding = 1) uniform sampler2D t_normal;
layout(set = 1, binding = 2) uniform sampler2D t_orm;
layout(set = 1, binding = 3) uniform sampler2D t_emission;
layout(push_constant) uniform Push {
    mat4 model;
    vec4 d0;
    vec4 d1;
    vec4 d2;
    vec4 d3;
} pc;
layout(location = 0) in vec3 v_world_pos;
layout(location = 1) in vec3 v_normal;
layout(location = 2) in vec2 v_uv;
layout(location = 3) in vec4 v_color;
layout(location = 4) in vec4 v_tangent;
layout(location = 0) out vec4 o_color;
#define u_time (frame.misc.x)
#define u_camera_pos (frame.camera_pos.xyz)
#define u_light_dir (frame.light_dir.xyz)
#define u_light_color (frame.light_color.rgb)
#define u_ambient (frame.ambient.rgb)
"#;

fn prop_define(prop: &ShaderProp) -> String {
    let vec = prop.offset / 4;
    let comp = prop.offset % 4;
    let access = match prop.kind {
        ShaderPropKind::Float { .. } | ShaderPropKind::Toggle => {
            format!("pc.d{vec}.{}", ["x", "y", "z", "w"][comp])
        }
        ShaderPropKind::Color3 => format!("pc.d{vec}.{}", ["xyz", "yzw"][comp]),
        ShaderPropKind::Color => format!("pc.d{vec}"),
    };
    format!("#define {} ({access})\n", prop.name)
}

/// Assemble preamble + body and compile via `glslc`. `label` names the
/// shader in error messages.
pub fn compile_shader(source: &ShaderSource, label: &str) -> Result<Vec<u8>> {
    let mut glsl = String::with_capacity(PREAMBLE.len() + source.body.len() + 256);
    glsl.push_str(PREAMBLE);
    for prop in &source.props {
        glsl.push_str(&prop_define(prop));
    }
    // Reset diagnostics to the user's own line numbers.
    glsl.push_str("#line 1\n");
    glsl.push_str(&source.body);

    let mut child = Command::new("glslc")
        .args([
            "--target-env=vulkan1.3",
            "-fshader-stage=fragment",
            "-O",
            "-",
            "-o",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running glslc — is shaderc installed?")?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(glsl.as_bytes())
        .context("feeding glslc")?;
    let output = child.wait_with_output().context("waiting for glslc")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).replace("<stdin>", label);
        bail!("{}", stderr.trim());
    }
    Ok(output.stdout)
}

/// Read, parse, and compile a shader file.
pub fn load_shader_file(path: impl AsRef<Path>) -> Result<(ShaderSource, Vec<u8>)> {
    let path = path.as_ref();
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "shader".into());
    let source =
        parse_shader_source(&text, &name).with_context(|| format!("parsing {}", path.display()))?;
    let spirv = compile_shader(&source, &path.display().to_string())?;
    Ok((source, spirv))
}

/// Starter shader written by Files → Create → New Shader.
pub const SHADER_TEMPLATE: &str = r#"//! shader "My Shader"
//! prop tint color default(1, 0.6, 0.2, 1)
//! prop glow float range(0, 4) default(1)
//! prop speed float range(0, 10) default(2)

// Custom citrus shader (fragment stage). The engine provides:
//   textures   t_albedo, t_normal, t_orm, t_emission
//   varyings   v_world_pos, v_normal, v_uv, v_color, v_tangent
//   uniforms   u_time, u_camera_pos, u_light_dir, u_light_color, u_ambient
//   output     o_color
// Properties declared above appear in the Inspector automatically.
// Do not add a #version line.

void main() {
    vec4 albedo = texture(t_albedo, v_uv) * v_color;
    vec3 n = normalize(v_normal);
    float light = max(dot(n, -u_light_dir), 0.0);
    float pulse = 0.5 + 0.5 * sin(u_time * speed);
    vec3 color = albedo.rgb * tint.rgb * (u_ambient + u_light_color * light);
    color += tint.rgb * glow * pulse * 0.2;
    o_color = vec4(color, 1.0);
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_and_compiles() {
        let source = parse_shader_source(SHADER_TEMPLATE, "template").unwrap();
        assert_eq!(source.display_name, "My Shader");
        assert_eq!(source.props.len(), 3);
        // tint: color at 0, glow: float at 4 (aligned), speed at 5.
        assert_eq!(source.props[0].offset, 0);
        assert_eq!(source.props[1].offset, 4);
        assert_eq!(source.props[2].offset, 5);
        let spirv = compile_shader(&source, "template").unwrap();
        assert!(spirv.len() > 4 && spirv[0..4] == [0x03, 0x02, 0x23, 0x07]);
    }

    #[test]
    fn packing_round_trip() {
        let source = parse_shader_source(SHADER_TEMPLATE, "t").unwrap();
        let mut values = source.defaults();
        values[4] = 3.5; // glow
        let named = source.unpack(&values);
        assert_eq!(named["glow"], vec![3.5]);
        let packed = source.pack(&named);
        assert_eq!(packed, values);
    }
}
