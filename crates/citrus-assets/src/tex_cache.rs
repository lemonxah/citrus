//! Texture import cache: decode a source image once, generate a mip chain, and
//! BC-compress it (BC7 for LDR color/data, BC6H for HDR), persisting the result
//! to disk so later loads skip the expensive decode + recompress entirely.
//!
//! The cache lives in a sibling `.citrus_texcache/` next to the source and is
//! keyed by (filename, srgb); it is invalidated when the source's mtime or size
//! changes, or when the format version bumps. All work here is CPU-only and runs
//! on the loader thread; the GPU upload (`GpuTexture::upload_compressed`) just
//! stages the cached bytes.

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use citrus_render::{CompressedTexture, TexFormat};

use crate::material_file::load_texture_file;

const MAGIC: &[u8; 8] = b"CITRBC02";

/// Load a texture as a GPU-ready compressed (or raw-fallback) mip chain, using
/// the on-disk cache when fresh and encoding + caching otherwise. `srgb` selects
/// the colour space for LDR sources (ignored for HDR/EXR, which is always
/// linear). Never fails the load over a cache I/O error — it just re-encodes.
pub fn load_texture_bc(path: impl AsRef<Path>, srgb: bool) -> Result<CompressedTexture> {
    let path = path.as_ref();
    let meta = std::fs::metadata(path).ok();
    let src_mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let src_len = meta.as_ref().map(|m| m.len()).unwrap_or(0);

    let cache = cache_path(path, srgb);
    if let Some(cache) = &cache
        && let Some(tex) = read_cache(cache, src_mtime, src_len)
    {
        return Ok(tex);
    }

    let data = load_texture_file(path, srgb)?;
    let tex = encode_texture(&data);
    if let Some(cache) = &cache
        && let Err(e) = write_cache(cache, &tex, src_mtime, src_len)
    {
        tracing::debug!("texture cache write {}: {e:#}", cache.display());
    }
    Ok(tex)
}

/// `<dir>/.citrus_texcache/<filename>.<srgb>.cbc`.
fn cache_path(src: &Path, srgb: bool) -> Option<PathBuf> {
    let dir = src.parent()?.join(".citrus_texcache");
    let name = src.file_name()?.to_string_lossy();
    Some(dir.join(format!("{name}.{}.cbc", srgb as u8)))
}

/// Encode decoded pixels into a compressed (or raw-fallback) mip chain. BC needs
/// each mip's dimensions to be multiples of 4; sources that aren't fall back to
/// a single uncompressed mip (still cached so the decode isn't repeated).
fn encode_texture(data: &citrus_render::TextureData) -> CompressedTexture {
    // BC needs multiple-of-4 dims and device support; otherwise keep raw (still
    // cached so the source decode isn't repeated).
    let mult4 = data.width % 4 == 0 && data.height % 4 == 0 && data.width >= 4 && data.height >= 4;
    let mult4 = mult4 && citrus_render::bc_supported();
    if data.hdr {
        if mult4 {
            return encode_bc6h(data);
        }
        return raw(data, TexFormat::RgbaF16);
    }
    if mult4 {
        let format = if data.srgb {
            TexFormat::Bc7Srgb
        } else {
            TexFormat::Bc7Unorm
        };
        return encode_bc7(data, format);
    }
    raw(
        data,
        if data.srgb {
            TexFormat::RgbaSrgb
        } else {
            TexFormat::RgbaUnorm
        },
    )
}

/// A single uncompressed mip (the fallback for odd dimensions).
fn raw(data: &citrus_render::TextureData, format: TexFormat) -> CompressedTexture {
    CompressedTexture {
        format,
        width: data.width,
        height: data.height,
        mips: vec![data.pixels.clone()],
    }
}

fn encode_bc7(data: &citrus_render::TextureData, format: TexFormat) -> CompressedTexture {
    // Opaque mode is much faster and most maps (normal/ORM/AO/rough/metal, and
    // usually albedo) have no real alpha. Only pay for alpha when a texel is
    // actually non-opaque. Ultra-fast tier: this is a one-time encode and the
    // quality is still good for material maps.
    let has_alpha = data.pixels.chunks_exact(4).any(|p| p[3] != 255);
    let settings = if has_alpha {
        intel_tex_2::bc7::alpha_ultra_fast_settings()
    } else {
        intel_tex_2::bc7::opaque_ultra_fast_settings()
    };
    let mips = mip_chain_rgba8(&data.pixels, data.width, data.height)
        .into_iter()
        .map(|(px, w, h)| {
            let surface = intel_tex_2::RgbaSurface {
                data: &px,
                width: w,
                height: h,
                stride: w * 4,
            };
            intel_tex_2::bc7::compress_blocks(&settings, &surface)
        })
        .collect();
    CompressedTexture {
        format,
        width: data.width,
        height: data.height,
        mips,
    }
}

fn encode_bc6h(data: &citrus_render::TextureData) -> CompressedTexture {
    let settings = intel_tex_2::bc6h::very_fast_settings();
    let mips = mip_chain_rgba_f16(&data.pixels, data.width, data.height)
        .into_iter()
        .map(|(px, w, h)| {
            let surface = intel_tex_2::RgbaSurface {
                data: &px,
                width: w,
                height: h,
                stride: w * 8,
            };
            intel_tex_2::bc6h::compress_blocks(&settings, &surface)
        })
        .collect();
    CompressedTexture {
        format: TexFormat::Bc6h,
        width: data.width,
        height: data.height,
        mips,
    }
}

/// Mip chain (largest first) of RGBA8 pixels, halving until the next level would
/// drop below 4 in either dimension (BC's minimum tile). All levels stay a
/// multiple of 4 given a multiple-of-4 source.
fn mip_chain_rgba8(pixels: &[u8], width: u32, height: u32) -> Vec<(Vec<u8>, u32, u32)> {
    let mut out = vec![(pixels.to_vec(), width, height)];
    let (mut w, mut h) = (width, height);
    while w >= 8 && h >= 8 && w % 2 == 0 && h % 2 == 0 {
        let prev = &out.last().unwrap().0;
        let down = downsample_rgba8(prev, w, h);
        w /= 2;
        h /= 2;
        out.push((down, w, h));
    }
    out
}

fn mip_chain_rgba_f16(pixels: &[u8], width: u32, height: u32) -> Vec<(Vec<u8>, u32, u32)> {
    let mut out = vec![(pixels.to_vec(), width, height)];
    let (mut w, mut h) = (width, height);
    while w >= 8 && h >= 8 && w % 2 == 0 && h % 2 == 0 {
        let prev = &out.last().unwrap().0;
        let down = downsample_rgba_f16(prev, w, h);
        w /= 2;
        h /= 2;
        out.push((down, w, h));
    }
    out
}

fn downsample_rgba8(src: &[u8], w: u32, h: u32) -> Vec<u8> {
    let (nw, nh) = (w / 2, h / 2);
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        for x in 0..nw {
            for c in 0..4 {
                let mut sum = 0u32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let sx = x * 2 + dx;
                        let sy = y * 2 + dy;
                        sum += src[((sy * w + sx) * 4 + c) as usize] as u32;
                    }
                }
                out[((y * nw + x) * 4 + c) as usize] = (sum / 4) as u8;
            }
        }
    }
    out
}

fn downsample_rgba_f16(src: &[u8], w: u32, h: u32) -> Vec<u8> {
    let (nw, nh) = (w / 2, h / 2);
    let mut out = vec![0u8; (nw * nh * 8) as usize];
    let read = |i: usize| half::f16::from_le_bytes([src[i], src[i + 1]]).to_f32();
    for y in 0..nh {
        for x in 0..nw {
            for c in 0..4 {
                let mut sum = 0f32;
                for dy in 0..2 {
                    for dx in 0..2 {
                        let sx = x * 2 + dx;
                        let sy = y * 2 + dy;
                        sum += read((((sy * w + sx) * 4 + c) * 2) as usize);
                    }
                }
                let bytes = half::f16::from_f32(sum / 4.0).to_le_bytes();
                let o = (((y * nw + x) * 4 + c) * 2) as usize;
                out[o] = bytes[0];
                out[o + 1] = bytes[1];
            }
        }
    }
    out
}

fn format_byte(f: TexFormat) -> u8 {
    match f {
        TexFormat::Bc7Srgb => 0,
        TexFormat::Bc7Unorm => 1,
        TexFormat::Bc6h => 2,
        TexFormat::RgbaSrgb => 3,
        TexFormat::RgbaUnorm => 4,
        TexFormat::RgbaF16 => 5,
    }
}

fn byte_format(b: u8) -> Option<TexFormat> {
    Some(match b {
        0 => TexFormat::Bc7Srgb,
        1 => TexFormat::Bc7Unorm,
        2 => TexFormat::Bc6h,
        3 => TexFormat::RgbaSrgb,
        4 => TexFormat::RgbaUnorm,
        5 => TexFormat::RgbaF16,
        _ => return None,
    })
}

fn write_cache(
    path: &Path,
    tex: &CompressedTexture,
    src_mtime: u64,
    src_len: u64,
) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut buf = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.push(format_byte(tex.format));
    buf.extend_from_slice(&src_mtime.to_le_bytes());
    buf.extend_from_slice(&src_len.to_le_bytes());
    buf.extend_from_slice(&tex.width.to_le_bytes());
    buf.extend_from_slice(&tex.height.to_le_bytes());
    buf.extend_from_slice(&(tex.mips.len() as u32).to_le_bytes());
    for mip in &tex.mips {
        buf.extend_from_slice(&(mip.len() as u32).to_le_bytes());
        buf.extend_from_slice(mip);
    }
    // Write to a temp then rename so a crash mid-write can't leave a torn cache.
    let tmp = path.with_extension("cbc.tmp");
    std::fs::write(&tmp, &buf).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Read + validate a cache file; None on any mismatch (stale, corrupt, version).
fn read_cache(path: &Path, src_mtime: u64, src_len: u64) -> Option<CompressedTexture> {
    let bytes = std::fs::read(path).ok()?;
    let mut p = 0usize;
    let take = |p: &mut usize, n: usize| -> Option<&[u8]> {
        let s = bytes.get(*p..*p + n)?;
        *p += n;
        Some(s)
    };
    if take(&mut p, 8)? != MAGIC {
        return None;
    }
    let format = byte_format(take(&mut p, 1)?[0])?;
    let u64_at = |p: &mut usize| -> Option<u64> {
        Some(u64::from_le_bytes(take(p, 8)?.try_into().ok()?))
    };
    let u32_at = |p: &mut usize| -> Option<u32> {
        Some(u32::from_le_bytes(take(p, 4)?.try_into().ok()?))
    };
    if u64_at(&mut p)? != src_mtime || u64_at(&mut p)? != src_len {
        return None;
    }
    let width = u32_at(&mut p)?;
    let height = u32_at(&mut p)?;
    let mip_count = u32_at(&mut p)? as usize;
    let mut mips = Vec::with_capacity(mip_count);
    for _ in 0..mip_count {
        let len = u32_at(&mut p)? as usize;
        mips.push(take(&mut p, len)?.to_vec());
    }
    Some(CompressedTexture {
        format,
        width,
        height,
        mips,
    })
}
