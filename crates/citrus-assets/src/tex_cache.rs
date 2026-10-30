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
    let mult4 = data.width.is_multiple_of(4) && data.height.is_multiple_of(4) && data.width >= 4 && data.height >= 4;
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
    let has_alpha = data.pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 255);
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

const MEAN_MAGIC: &[u8; 8] = b"CITRMN01";

/// Mean linear RGB of a texture (sRGB sources decoded to linear; HDR and
/// unreadable files give `[1,1,1]`). Cached as `<filename>.<srgb>.mean` beside
/// the BC cache, so only the first load ever decodes the source for it.
pub fn texture_mean(path: impl AsRef<Path>, srgb: bool) -> [f32; 3] {
    let path = path.as_ref();
    let meta = std::fs::metadata(path).ok();
    let src_mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let src_len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let cache = cache_path(path, srgb).map(|p| p.with_extension("mean"));
    if let Some(cache) = &cache
        && let Some(mean) = read_mean(cache, src_mtime, src_len)
    {
        return mean;
    }
    let mean = match load_texture_file(path, srgb) {
        Ok(data) => texture_mean_linear(&data),
        Err(_) => return [1.0; 3],
    };
    if let Some(cache) = &cache {
        let mut buf = Vec::with_capacity(36);
        buf.extend_from_slice(MEAN_MAGIC);
        buf.extend_from_slice(&src_mtime.to_le_bytes());
        buf.extend_from_slice(&src_len.to_le_bytes());
        for c in mean {
            buf.extend_from_slice(&c.to_le_bytes());
        }
        let written = cache
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(cache, &buf));
        if let Err(e) = written {
            tracing::debug!("texture mean cache write {}: {e:#}", cache.display());
        }
    }
    mean
}

fn read_mean(path: &Path, src_mtime: u64, src_len: u64) -> Option<[f32; 3]> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() != 36 || &bytes[..8] != MEAN_MAGIC {
        return None;
    }
    let u64_at = |o: usize| u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap());
    if u64_at(8) != src_mtime || u64_at(16) != src_len {
        return None;
    }
    let f = |o: usize| f32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    Some([f(24), f(28), f(32)])
}

/// Mean RGB in linear space. HDR maps return `[1,1,1]`: their values exceed 1
/// and aren't a simple coverage factor.
pub fn texture_mean_linear(data: &citrus_render::TextureData) -> [f32; 3] {
    if data.hdr || data.pixels.len() < 4 {
        return [1.0; 3];
    }
    let mut acc = [0.0f64; 3];
    let mut n = 0u64;
    for px in data.pixels.as_chunks::<4>().0 {
        for c in 0..3 {
            let s = px[c] as f32 / 255.0;
            acc[c] += if data.srgb { srgb_to_linear(s) } else { s } as f64;
        }
        n += 1;
    }
    if n == 0 {
        return [1.0; 3];
    }
    [
        (acc[0] / n as f64) as f32,
        (acc[1] / n as f64) as f32,
        (acc[2] / n as f64) as f32,
    ]
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// The import-cache blob for a texture (encoding it first when stale), with
/// the source mtime/size fields zeroed so it can travel inside a content
/// package and be re-bound to the extracted file by [`seed_texture_cache`].
pub fn texture_cache_blob(path: impl AsRef<Path>, srgb: bool) -> Result<Vec<u8>> {
    let path = path.as_ref();
    let tex = load_texture_bc(path, srgb)?;
    let mut buf = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.push(format_byte(tex.format));
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes());
    buf.extend_from_slice(&tex.width.to_le_bytes());
    buf.extend_from_slice(&tex.height.to_le_bytes());
    buf.extend_from_slice(&(tex.mips.len() as u32).to_le_bytes());
    for mip in &tex.mips {
        buf.extend_from_slice(&(mip.len() as u32).to_le_bytes());
        buf.extend_from_slice(mip);
    }
    Ok(buf)
}

/// Install a packaged cache blob for `path` (already on disk) so the next
/// `load_texture_bc` skips the decode + encode. The blob is fully validated
/// (format, dimensions, per-mip byte counts) since it may come from untrusted
/// content; BC blobs are skipped on devices without BC support. Returns the
/// texture's GPU byte size when installed, None when skipped.
pub fn seed_texture_cache(path: impl AsRef<Path>, srgb: bool, blob: &[u8], max_dim: u32) -> Result<Option<u64>> {
    let path = path.as_ref();
    if blob.len() < 9 + 16 || &blob[..8] != MAGIC {
        anyhow::bail!("not a texture cache blob");
    }
    let tex = parse_blob(blob).context("malformed texture cache blob")?;
    validate_compressed(&tex, max_dim)?;
    let is_bc = matches!(tex.format, TexFormat::Bc7Srgb | TexFormat::Bc7Unorm | TexFormat::Bc6h);
    if is_bc && !citrus_render::bc_supported() {
        return Ok(None);
    }
    let meta = std::fs::metadata(path).with_context(|| format!("seeding cache for {}", path.display()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cache = cache_path(path, srgb).context("texture has no parent directory")?;
    write_cache(&cache, &tex, mtime, meta.len())?;
    Ok(Some(tex.mips.iter().map(|m| m.len() as u64).sum()))
}

fn parse_blob(bytes: &[u8]) -> Option<CompressedTexture> {
    let mut p = 8usize;
    let take = |p: &mut usize, n: usize| -> Option<&[u8]> {
        let s = bytes.get(*p..p.checked_add(n)?)?;
        *p += n;
        Some(s)
    };
    let format = byte_format(take(&mut p, 1)?[0])?;
    take(&mut p, 16)?;
    let u32_at = |p: &mut usize| -> Option<u32> { Some(u32::from_le_bytes(take(p, 4)?.try_into().ok()?)) };
    let width = u32_at(&mut p)?;
    let height = u32_at(&mut p)?;
    let mip_count = u32_at(&mut p)? as usize;
    if mip_count == 0 || mip_count > 16 {
        return None;
    }
    let mut mips = Vec::with_capacity(mip_count);
    for _ in 0..mip_count {
        let len = u32_at(&mut p)? as usize;
        mips.push(take(&mut p, len)?.to_vec());
    }
    (p == bytes.len()).then_some(CompressedTexture {
        format,
        width,
        height,
        mips,
    })
}

/// Check a compressed texture's mip byte counts match its format and size, so
/// a crafted cache can't make the GPU upload read past a buffer.
pub fn validate_compressed(tex: &CompressedTexture, max_dim: u32) -> Result<()> {
    if tex.width == 0 || tex.height == 0 || tex.width > max_dim || tex.height > max_dim {
        anyhow::bail!("texture size {}x{} outside 1..={max_dim}", tex.width, tex.height);
    }
    for (i, mip) in tex.mips.iter().enumerate() {
        let (w, h) = ((tex.width >> i).max(1) as usize, (tex.height >> i).max(1) as usize);
        let expected = match tex.format {
            TexFormat::Bc7Srgb | TexFormat::Bc7Unorm | TexFormat::Bc6h => w.div_ceil(4) * h.div_ceil(4) * 16,
            TexFormat::RgbaSrgb | TexFormat::RgbaUnorm => w * h * 4,
            TexFormat::RgbaF16 => w * h * 8,
        };
        if mip.len() != expected {
            anyhow::bail!("mip {i} holds {} bytes, expected {expected}", mip.len());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texture_mean_is_cached_and_invalidated() {
        let dir = std::env::temp_dir().join(format!("citrus_mean_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("grey.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 128, 255, 255])).save(&png).unwrap();

        let mean = texture_mean(&png, false);
        assert!((mean[0] - 0.0).abs() < 1e-4 && (mean[1] - 128.0 / 255.0).abs() < 1e-4 && (mean[2] - 1.0).abs() < 1e-4);
        let sidecar = dir.join(".citrus_texcache/grey.png.0.mean");
        assert!(sidecar.exists());
        assert_eq!(texture_mean(&png, false), mean);

        // A different source (size changes) must not reuse the stale mean.
        image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 255, 255, 255])).save(&png).unwrap();
        assert_eq!(texture_mean(&png, false), [1.0; 3]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
