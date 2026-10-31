//! Texture import cache: decode a source image once, generate a mip chain, and
//! block-compress it, persisting the result to disk so later loads skip the
//! expensive decode + recompress entirely.
//!
//! Which encoding depends on what the device samples: BC (BC7 for LDR
//! color/data, BC6H for HDR) on desktop GPUs, ASTC LDR on mobile / standalone
//! VR GPUs without BC (HDR sources stay raw RGBA16F there), raw RGBA
//! otherwise. The cache lives in a sibling `.citrus_texcache/` next to the
//! source, keyed by (filename, srgb): `<name>.<srgb>.cbc` for the BC / raw
//! tier and `<name>.<srgb>.castc` for the ASTC tier, so both coexist. It is
//! invalidated when the source's mtime or size changes, or when the format
//! version bumps, and a cached format the device can't sample is re-encoded,
//! never uploaded. All work here is CPU-only and runs on the loader thread;
//! the GPU upload (`GpuTexture::upload_compressed`) just stages the cached
//! bytes.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use citrus_render::{CompressedTexture, TexFormat};

use crate::material_file::load_texture_file;

const MAGIC: &[u8; 8] = b"CITRBC02";
/// Extension of the ASTC-tier cache artefact (the BC / raw tier uses `cbc`).
pub const ASTC_CACHE_EXTENSION: &str = "castc";

/// ASTC block footprints for the mobile texture path, as square block edges
/// (4, 5, 6 or 8; larger is smaller and blurrier). `color` applies to sRGB
/// slots (albedo, emission, matcaps), `data` to linear slots (normal, ORM,
/// masks), which need more precision. Default 6x6 / 4x4 (3.56 / 8 bits per
/// texel). `CITRUS_ASTC_BLOCKS=<color>,<data>` (`6x6,4x4` or `6,4`)
/// overrides both the default and [`set_astc_blocks`]. A change affects new
/// encodes only: a device load accepts an ASTC cache of any footprint, while
/// the Android cook ([`astc_cache_blob`]) re-encodes to the current setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AstcBlocks {
    pub color: u32,
    pub data: u32,
}

impl Default for AstcBlocks {
    fn default() -> Self {
        Self { color: 6, data: 4 }
    }
}

impl AstcBlocks {
    /// Parse `<color>,<data>`, each `N` or `NxN` with N in 4, 5, 6, 8.
    pub fn parse(s: &str) -> Option<Self> {
        let edge = |t: &str| -> Option<u32> {
            let t = t.trim().to_ascii_lowercase();
            let n: u32 = match t.split_once('x') {
                Some((a, b)) if a == b => a.parse().ok()?,
                Some(_) => return None,
                None => t.parse().ok()?,
            };
            TexFormat::ASTC_BLOCKS.contains(&n).then_some(n)
        };
        let (color, data) = s.split_once(',')?;
        Some(Self { color: edge(color)?, data: edge(data)? })
    }

    /// Block edge for a slot loaded with `srgb`.
    pub fn for_slot(self, srgb: bool) -> u32 {
        if srgb { self.color } else { self.data }
    }

    fn is_valid(self) -> bool {
        TexFormat::ASTC_BLOCKS.contains(&self.color) && TexFormat::ASTC_BLOCKS.contains(&self.data)
    }
}

/// `color << 8 | data`, 0 = default.
static ASTC_BLOCKS: AtomicU32 = AtomicU32::new(0);

/// Set the ASTC footprints used by later encodes (see [`AstcBlocks`]).
pub fn set_astc_blocks(blocks: AstcBlocks) -> Result<()> {
    anyhow::ensure!(blocks.is_valid(), "ASTC blocks must be 4, 5, 6 or 8 (got {blocks:?})");
    ASTC_BLOCKS.store(blocks.color << 8 | blocks.data, Ordering::Relaxed);
    Ok(())
}

/// The ASTC footprints in effect: `CITRUS_ASTC_BLOCKS`, else the last
/// [`set_astc_blocks`], else the default.
pub fn astc_blocks() -> AstcBlocks {
    static ENV: OnceLock<Option<AstcBlocks>> = OnceLock::new();
    let env = ENV.get_or_init(|| {
        let v = std::env::var("CITRUS_ASTC_BLOCKS").ok()?;
        let parsed = AstcBlocks::parse(&v);
        if parsed.is_none() {
            tracing::warn!("CITRUS_ASTC_BLOCKS={v:?} ignored (expected e.g. 6x6,4x4)");
        }
        parsed
    });
    if let Some(b) = env {
        return *b;
    }
    match ASTC_BLOCKS.load(Ordering::Relaxed) {
        0 => AstcBlocks::default(),
        v => AstcBlocks { color: v >> 8, data: v & 0xff },
    }
}

/// True when this device has no BC but samples ASTC LDR (the `.castc` tier).
fn astc_tier() -> bool {
    !citrus_render::bc_supported() && citrus_render::astc_supported()
}

/// Whether the active device can sample `format`.
fn device_samples(format: TexFormat) -> bool {
    if format.is_bc() {
        citrus_render::bc_supported()
    } else if format.is_astc() {
        citrus_render::astc_supported()
    } else {
        true
    }
}

/// (mtime secs, byte length) of a source file, zeros when unreadable.
fn source_stamp(path: &Path) -> (u64, u64) {
    let meta = std::fs::metadata(path).ok();
    let mtime = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    (mtime, meta.as_ref().map(|m| m.len()).unwrap_or(0))
}

/// Load a texture as a GPU-ready compressed (or raw-fallback) mip chain, using
/// the on-disk cache when fresh and encoding + caching otherwise. `srgb` selects
/// the colour space for LDR sources (ignored for HDR/EXR, which is always
/// linear). Never fails the load over a cache I/O error — it just re-encodes.
pub fn load_texture_bc(path: impl AsRef<Path>, srgb: bool) -> Result<CompressedTexture> {
    let path = path.as_ref();
    let (src_mtime, src_len) = source_stamp(path);
    let astc = astc_tier();

    let cache = if astc { astc_cache_path(path, srgb) } else { cache_path(path, srgb) };
    if let Some(cache) = &cache
        && let Some(tex) = read_cache(cache, src_mtime, src_len)
        && device_samples(tex.format)
    {
        return Ok(tex);
    }

    let data = load_texture_file(path, srgb)?;
    let tex = if astc { encode_astc(&data, astc_blocks()) } else { encode_texture(&data) };
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

/// `<dir>/.citrus_texcache/<filename>.<srgb>.castc`.
fn astc_cache_path(src: &Path, srgb: bool) -> Option<PathBuf> {
    cache_path(src, srgb).map(|p| p.with_extension(ASTC_CACHE_EXTENSION))
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

/// ASTC-encode a decoded texture with a full mip chain down to 1x1 (ASTC has
/// no size restriction; partial edge blocks are padded). HDR sources stay a
/// single raw RGBA16F mip: there is no ASTC HDR encoder here.
fn encode_astc(data: &citrus_render::TextureData, blocks: AstcBlocks) -> CompressedTexture {
    if data.hdr {
        return raw(data, TexFormat::RgbaF16);
    }
    let block = blocks.for_slot(data.srgb);
    let format = TexFormat::astc(block, data.srgb).unwrap_or(if data.srgb {
        TexFormat::Astc6x6Srgb
    } else {
        TexFormat::Astc4x4Unorm
    });
    let block = format.block_dims().0;
    let has_alpha = data.pixels.as_chunks::<4>().0.iter().any(|p| p[3] != 255);
    let mips = full_mip_chain_rgba8(&data.pixels, data.width, data.height)
        .into_iter()
        .map(|(px, w, h)| encode_astc_level(&px, w, h, block, has_alpha))
        .collect();
    CompressedTexture {
        format,
        width: data.width,
        height: data.height,
        mips,
    }
}

/// The ISPC texture compressor's ASTC entry points. They live in the C++
/// archive `intel_tex_2` links (its Rust ASTC wrapper is unimplemented), with
/// the layouts of `ispc_texcomp.h`.
mod ispc_astc {
    #[repr(C)]
    pub struct RgbaSurface {
        pub ptr: *mut u8,
        pub width: i32,
        pub height: i32,
        pub stride: i32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct AstcEncSettings {
        pub block_width: i32,
        pub block_height: i32,
        pub channels: i32,
        pub fast_skip_threshold: i32,
        pub refine_iterations: i32,
    }

    unsafe extern "C" {
        /// `src` dimensions must be multiples of the block size; writes
        /// `(w / bw) * (h / bh)` 16-byte blocks, row-major, to `dst`.
        pub fn CompressBlocksASTC(src: *const RgbaSurface, dst: *mut u8, settings: *mut AstcEncSettings);
        pub fn GetProfile_astc_fast(settings: *mut AstcEncSettings, block_width: i32, block_height: i32);
        pub fn GetProfile_astc_alpha_fast(settings: *mut AstcEncSettings, block_width: i32, block_height: i32);
    }
}

/// ASTC LDR-encode one RGBA8 level of any size with square `block` edges (4,
/// 5, 6 or 8). The level is padded to whole blocks by edge replication;
/// output is `ceil(w/b) * ceil(h/b)` 16-byte blocks, row-major, as Vulkan
/// expects. `alpha` selects the RGBA profile, otherwise alpha decodes as 1.
pub fn encode_astc_level(rgba: &[u8], w: u32, h: u32, block: u32, alpha: bool) -> Vec<u8> {
    assert!(TexFormat::ASTC_BLOCKS.contains(&block), "unsupported ASTC block {block}");
    let (padded, pw, ph) = pad_rgba8(rgba, w, h, block);
    let mut settings = ispc_astc::AstcEncSettings::default();
    let b = block as i32;
    // SAFETY: the profile functions only write the settings struct.
    unsafe {
        if alpha {
            ispc_astc::GetProfile_astc_alpha_fast(&mut settings, b, b);
        } else {
            ispc_astc::GetProfile_astc_fast(&mut settings, b, b);
        }
    }
    let surface = ispc_astc::RgbaSurface {
        ptr: padded.as_ptr().cast_mut(),
        width: pw as i32,
        height: ph as i32,
        stride: (pw * 4) as i32,
    };
    let mut out = vec![0u8; ((pw / block) * (ph / block) * 16) as usize];
    // SAFETY: `padded` holds pw*ph RGBA8 texels at `stride` (only read), both
    // dims are multiples of the <= 8 block (the encoder's asserted contract),
    // and `out` has room for exactly one 16-byte block per footprint.
    unsafe { ispc_astc::CompressBlocksASTC(&surface, out.as_mut_ptr(), &mut settings) };
    out
}

/// Copy an RGBA8 level into a buffer padded up to whole `block`-sized blocks
/// by replicating the last row / column. Returns (pixels, width, height).
fn pad_rgba8(src: &[u8], w: u32, h: u32, block: u32) -> (Vec<u8>, u32, u32) {
    let (w, h) = (w.max(1), h.max(1));
    let (pw, ph) = (w.div_ceil(block) * block, h.div_ceil(block) * block);
    if (pw, ph) == (w, h) && src.len() == (w * h * 4) as usize {
        return (src.to_vec(), w, h);
    }
    let mut out = vec![0u8; (pw * ph * 4) as usize];
    for y in 0..ph {
        let sy = y.min(h - 1);
        for x in 0..pw {
            let si = ((sy * w + x.min(w - 1)) * 4) as usize;
            let di = ((y * pw + x) * 4) as usize;
            if let Some(px) = src.get(si..si + 4) {
                out[di..di + 4].copy_from_slice(px);
            }
        }
    }
    (out, pw, ph)
}

/// Mip chain (largest first) of RGBA8 pixels down to 1x1, for any size: each
/// level is `max(dim >> 1, 1)`, box-filtered with edge clamping.
fn full_mip_chain_rgba8(pixels: &[u8], width: u32, height: u32) -> Vec<(Vec<u8>, u32, u32)> {
    let mut out = vec![(pixels.to_vec(), width, height)];
    let (mut w, mut h) = (width.max(1), height.max(1));
    while w > 1 || h > 1 {
        let prev = &out.last().unwrap().0;
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        let mut down = vec![0u8; (nw * nh * 4) as usize];
        for y in 0..nh {
            for x in 0..nw {
                for c in 0..4 {
                    let mut sum = 0u32;
                    for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                        let (sx, sy) = ((x * 2 + dx).min(w - 1), (y * 2 + dy).min(h - 1));
                        sum += prev[((sy * w + sx) * 4 + c) as usize] as u32;
                    }
                    down[((y * nw + x) * 4 + c) as usize] = ((sum + 2) / 4) as u8;
                }
            }
        }
        (w, h) = (nw, nh);
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
        TexFormat::Astc4x4Srgb => 6,
        TexFormat::Astc4x4Unorm => 7,
        TexFormat::Astc5x5Srgb => 8,
        TexFormat::Astc5x5Unorm => 9,
        TexFormat::Astc6x6Srgb => 10,
        TexFormat::Astc6x6Unorm => 11,
        TexFormat::Astc8x8Srgb => 12,
        TexFormat::Astc8x8Unorm => 13,
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
        6 => TexFormat::Astc4x4Srgb,
        7 => TexFormat::Astc4x4Unorm,
        8 => TexFormat::Astc5x5Srgb,
        9 => TexFormat::Astc5x5Unorm,
        10 => TexFormat::Astc6x6Srgb,
        11 => TexFormat::Astc6x6Unorm,
        12 => TexFormat::Astc8x8Srgb,
        13 => TexFormat::Astc8x8Unorm,
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
    let buf = cache_bytes(tex, src_mtime, src_len);
    // Write to a temp then rename so a crash mid-write can't leave a torn cache.
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, &buf).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The cache file layout: magic, format byte, source mtime + size, width,
/// height, mip count, then per mip (byte length, bytes).
fn cache_bytes(tex: &CompressedTexture, src_mtime: u64, src_len: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(37 + tex.mips.iter().map(|m| m.len() + 4).sum::<usize>());
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
    buf
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
    let (src_mtime, src_len) = source_stamp(path);
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
    let tex = load_texture_bc(path, srgb)?;
    Ok(cache_bytes(&tex, 0, 0))
}

/// Device-independent ASTC-tier cache blob for a texture (the Android /
/// Quest cook): ASTC LDR at `blocks`' footprint for the slot, raw RGBA16F
/// for HDR sources. Reuses the source's own `.castc` cache when it matches
/// (re-encoding and refreshing it otherwise), and zeroes the source stamp
/// like [`texture_cache_blob`] so [`seed_astc_cache`] can re-bind it.
pub fn astc_cache_blob(path: impl AsRef<Path>, srgb: bool, blocks: AstcBlocks) -> Result<Vec<u8>> {
    let path = path.as_ref();
    let (src_mtime, src_len) = source_stamp(path);
    let want = TexFormat::astc(blocks.for_slot(srgb), srgb);
    let cache = astc_cache_path(path, srgb);
    let cached = cache
        .as_ref()
        .and_then(|c| read_cache(c, src_mtime, src_len))
        .filter(|t| Some(t.format) == want || t.format == TexFormat::RgbaF16);
    let tex = match cached {
        Some(t) => t,
        None => {
            let tex = encode_astc(&load_texture_file(path, srgb)?, blocks);
            if let Some(cache) = &cache
                && let Err(e) = write_cache(cache, &tex, src_mtime, src_len)
            {
                tracing::debug!("texture cache write {}: {e:#}", cache.display());
            }
            tex
        }
    };
    Ok(cache_bytes(&tex, 0, 0))
}

/// Install a packaged cache blob for `path` (already on disk) so the next
/// `load_texture_bc` skips the decode + encode. The blob is fully validated
/// (format, dimensions, per-mip byte counts) since it may come from untrusted
/// content. BC blobs are skipped on devices without BC support; ASTC blobs
/// go to the ASTC-tier artefact. Returns the texture's GPU byte size when
/// installed, None when skipped.
pub fn seed_texture_cache(path: impl AsRef<Path>, srgb: bool, blob: &[u8], max_dim: u32) -> Result<Option<u64>> {
    let tex = checked_blob(blob, max_dim)?;
    if tex.format.is_bc() && !citrus_render::bc_supported() {
        return Ok(None);
    }
    let astc = tex.format.is_astc();
    install_seed(path.as_ref(), srgb, &tex, astc).map(Some)
}

/// Install an ASTC-tier blob from [`astc_cache_blob`] (ASTC LDR or raw) as
/// `path`'s `.castc` cache. Independent of the device (it runs before the
/// renderer exists on Android); a device that can't sample it re-encodes at
/// load. Returns the texture's GPU byte size.
pub fn seed_astc_cache(path: impl AsRef<Path>, srgb: bool, blob: &[u8], max_dim: u32) -> Result<u64> {
    let tex = checked_blob(blob, max_dim)?;
    anyhow::ensure!(!tex.format.is_bc(), "{:?} is not an ASTC-tier format", tex.format);
    install_seed(path.as_ref(), srgb, &tex, true)
}

fn checked_blob(blob: &[u8], max_dim: u32) -> Result<CompressedTexture> {
    if blob.len() < 9 + 16 || &blob[..8] != MAGIC {
        anyhow::bail!("not a texture cache blob");
    }
    let tex = parse_blob(blob).context("malformed texture cache blob")?;
    validate_compressed(&tex, max_dim)?;
    Ok(tex)
}

/// Write `tex` as `path`'s cache, stamped with the file's current mtime / size.
fn install_seed(path: &Path, srgb: bool, tex: &CompressedTexture, astc: bool) -> Result<u64> {
    let meta = std::fs::metadata(path).with_context(|| format!("seeding cache for {}", path.display()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cache = if astc { astc_cache_path(path, srgb) } else { cache_path(path, srgb) };
    let cache = cache.context("texture has no parent directory")?;
    write_cache(&cache, tex, mtime, meta.len())?;
    Ok(tex.mips.iter().map(|m| m.len() as u64).sum())
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
    let full_chain = 32 - tex.width.max(tex.height).leading_zeros() as usize;
    if tex.mips.is_empty() || tex.mips.len() > full_chain {
        anyhow::bail!("{} mips for a {}x{} texture", tex.mips.len(), tex.width, tex.height);
    }
    for (i, mip) in tex.mips.iter().enumerate() {
        let expected = tex.format.level_bytes((tex.width >> i).max(1), (tex.height >> i).max(1));
        if mip.len() != expected {
            anyhow::bail!("mip {i} holds {} bytes, expected {expected}", mip.len());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_FORMATS: [TexFormat; 14] = [
        TexFormat::Bc7Srgb,
        TexFormat::Bc7Unorm,
        TexFormat::Bc6h,
        TexFormat::RgbaSrgb,
        TexFormat::RgbaUnorm,
        TexFormat::RgbaF16,
        TexFormat::Astc4x4Srgb,
        TexFormat::Astc4x4Unorm,
        TexFormat::Astc5x5Srgb,
        TexFormat::Astc5x5Unorm,
        TexFormat::Astc6x6Srgb,
        TexFormat::Astc6x6Unorm,
        TexFormat::Astc8x8Srgb,
        TexFormat::Astc8x8Unorm,
    ];

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("citrus_texcache_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// RGBA8 gradient with a hard checker in blue and an alpha ramp.
    fn gradient(w: u32, h: u32, alpha: bool) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[
                    (x * 255 / (w - 1).max(1)) as u8,
                    (y * 255 / (h - 1).max(1)) as u8,
                    if (x / 3 + y / 3) % 2 == 0 { 40 } else { 200 },
                    if alpha { ((x + y) * 255 / (w + h - 2).max(1)) as u8 } else { 255 },
                ]);
            }
        }
        px
    }

    fn ldr(w: u32, h: u32, srgb: bool, alpha: bool) -> citrus_render::TextureData {
        citrus_render::TextureData { width: w, height: h, pixels: gradient(w, h, alpha), srgb, hdr: false }
    }

    #[test]
    fn format_bytes_are_unique_and_round_trip() {
        let mut seen = std::collections::BTreeSet::new();
        for f in ALL_FORMATS {
            assert!(seen.insert(format_byte(f)), "{f:?} reuses a format byte");
            assert_eq!(byte_format(format_byte(f)), Some(f));
        }
        // On-disk values of the original formats never move.
        assert_eq!(ALL_FORMATS[..6].iter().map(|&f| format_byte(f)).collect::<Vec<_>>(), [0, 1, 2, 3, 4, 5]);
        assert_eq!(byte_format(14), None);
        for b in TexFormat::ASTC_BLOCKS {
            for srgb in [false, true] {
                let f = TexFormat::astc(b, srgb).unwrap();
                assert!(f.is_astc() && !f.is_bc() && f.block_dims() == (b, b));
            }
        }
        assert!(TexFormat::astc(7, true).is_none());
        assert!(TexFormat::Bc7Srgb.is_bc() && !TexFormat::Bc7Srgb.is_astc() && !TexFormat::RgbaF16.is_astc());
    }

    #[test]
    fn astc_levels_have_vulkan_block_counts() {
        for block in TexFormat::ASTC_BLOCKS {
            let format = TexFormat::astc(block, false).unwrap();
            for (w, h) in [(1, 1), (3, 5), (13, 7), (64, 32), (30, 30), (block * 3, block * 2)] {
                for alpha in [false, true] {
                    let blocks = encode_astc_level(&gradient(w, h, alpha), w, h, block, alpha);
                    let want = (w.div_ceil(block) * h.div_ceil(block) * 16) as usize;
                    assert_eq!(blocks.len(), want, "{w}x{h} at {block}x{block}");
                    assert_eq!(format.level_bytes(w, h), want);
                    assert!(blocks.iter().any(|&b| b != 0), "encoder wrote nothing");
                }
            }
        }
    }

    /// Decode ASTC LDR blocks with an independent decoder to RGBA8.
    fn decode_astc(blocks: &[u8], w: u32, h: u32, block: u32) -> Vec<u8> {
        let mut argb = vec![0u32; (w * h) as usize];
        texture2ddecoder::decode_astc(blocks, w as usize, h as usize, block as usize, block as usize, &mut argb).unwrap();
        argb.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, (p >> 24) as u8]).collect()
    }

    /// Smooth RGBA8 test card: gradients with a soft bump and an alpha ramp.
    fn smooth(w: u32, h: u32, alpha: bool) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let (u, v) = (x as f32 / (w - 1).max(1) as f32, y as f32 / (h - 1).max(1) as f32);
                let bump = (-((u - 0.5).powi(2) + (v - 0.5).powi(2)) * 8.0).exp();
                px.extend_from_slice(&[
                    (u * 255.0) as u8,
                    (v * 255.0) as u8,
                    (bump * 220.0 + 20.0) as u8,
                    if alpha { ((1.0 - u) * 255.0) as u8 } else { 255 },
                ]);
            }
        }
        px
    }

    #[test]
    fn astc_decodes_back_to_the_source() {
        // Mean / max absolute 8-bit error bounds per block edge (the fast
        // profiles measure ~1.5x below); a block-order or padding bug is far
        // above.
        for (block, mean_max, worst_max) in [(4, 3.0, 32), (5, 4.0, 40), (6, 5.0, 48), (8, 8.0, 64)] {
            // 61x37 has partial edge blocks at every footprint.
            for (w, h) in [(64, 32), (30, 30), (61, 37)] {
                for alpha in [false, true] {
                    let src = smooth(w, h, alpha);
                    let back = decode_astc(&encode_astc_level(&src, w, h, block, alpha), w, h, block);
                    let diffs: Vec<u32> = src.iter().zip(&back).map(|(a, b)| a.abs_diff(*b) as u32).collect();
                    let mean = diffs.iter().sum::<u32>() as f32 / diffs.len() as f32;
                    let worst = *diffs.iter().max().unwrap();
                    assert!(mean < mean_max && worst < worst_max, "{w}x{h} {block}x{block} alpha {alpha}: mean {mean} max {worst}");
                    if !alpha {
                        assert!(back.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "opaque profile keeps alpha 1");
                    }
                }
            }
        }
    }

    #[test]
    fn solid_colour_decodes_exactly() {
        for block in TexFormat::ASTC_BLOCKS {
            for (w, h) in [(24, 24), (13, 11), (1, 1)] {
                let px = [90u8, 160, 30, 255].repeat((w * h) as usize);
                let back = decode_astc(&encode_astc_level(&px, w, h, block, false), w, h, block);
                for p in back.as_chunks::<4>().0 {
                    assert!(p.iter().zip([90u8, 160, 30, 255]).all(|(a, b)| a.abs_diff(b) <= 1), "{w}x{h} {block}x{block}: {p:?}");
                }
            }
        }
    }

    #[test]
    fn astc_encode_is_thread_safe() {
        let (w, h) = (96, 64);
        let px = gradient(w, h, true);
        let single = encode_astc_level(&px, w, h, 6, true);
        let parallel: Vec<Vec<u8>> = std::thread::scope(|s| {
            let jobs: Vec<_> = (0..6).map(|_| s.spawn(|| encode_astc_level(&px, w, h, 6, true))).collect();
            jobs.into_iter().map(|j| j.join().unwrap()).collect()
        });
        assert!(parallel.iter().all(|p| *p == single));
    }

    #[test]
    fn astc_textures_carry_a_full_mip_chain() {
        let blocks = AstcBlocks::default();
        let color = encode_astc(&ldr(40, 24, true, false), blocks);
        assert_eq!(color.format, TexFormat::Astc6x6Srgb);
        // 40x24 -> 20x12 -> 10x6 -> 5x3 -> 2x1 -> 1x1.
        assert_eq!(color.mips.len(), 6);
        validate_compressed(&color, 64).unwrap();
        assert_eq!(color.mips[5].len(), 16);
        let data = encode_astc(&ldr(7, 9, false, true), blocks);
        assert_eq!(data.format, TexFormat::Astc4x4Unorm);
        assert_eq!(data.mips.len(), 4);
        validate_compressed(&data, 64).unwrap();
        let wide = encode_astc(&ldr(32, 32, true, false), AstcBlocks { color: 8, data: 5 });
        assert_eq!(wide.format, TexFormat::Astc8x8Srgb);
        assert_eq!(wide.mips[0].len(), 16 * 16);
        let hdr = citrus_render::TextureData { width: 4, height: 4, pixels: vec![0; 128], srgb: false, hdr: true };
        assert_eq!(encode_astc(&hdr, blocks).format, TexFormat::RgbaF16, "no ASTC HDR encoder");
    }

    #[test]
    fn validation_rejects_bad_mip_tables() {
        let mut tex = encode_astc(&ldr(16, 16, false, false), AstcBlocks::default());
        validate_compressed(&tex, 16).unwrap();
        assert!(validate_compressed(&tex, 8).is_err(), "over max_dim");
        tex.mips.push(vec![0; 16]);
        assert!(validate_compressed(&tex, 16).is_err(), "more mips than the chain has");
        tex.mips.pop();
        tex.mips[1].pop();
        assert!(validate_compressed(&tex, 16).is_err(), "short mip");
    }

    #[test]
    fn astc_blocks_parse() {
        assert_eq!(AstcBlocks::parse("6x6,4x4"), Some(AstcBlocks { color: 6, data: 4 }));
        assert_eq!(AstcBlocks::parse(" 8 , 5X5 "), Some(AstcBlocks { color: 8, data: 5 }));
        for bad in ["6x5,4", "7,4", "6", "", "4,4,4", "12x12,4"] {
            assert_eq!(AstcBlocks::parse(bad), None, "{bad:?}");
        }
        assert!(set_astc_blocks(AstcBlocks { color: 3, data: 4 }).is_err());
        assert_eq!(AstcBlocks::default().for_slot(true), 6);
        assert_eq!(AstcBlocks::default().for_slot(false), 4);
    }

    #[test]
    fn astc_cache_round_trips_and_seeds() {
        let dir = temp_dir("astc");
        let png = dir.join("albedo.png");
        image::RgbaImage::from_fn(20, 12, |x, y| image::Rgba([(x * 12) as u8, (y * 20) as u8, 99, 255])).save(&png).unwrap();
        let blocks = AstcBlocks::default();

        // Cook-side blob: zeroed stamp, refreshes the source's .castc cache.
        let blob = astc_cache_blob(&png, true, blocks).unwrap();
        let castc = dir.join(".citrus_texcache/albedo.png.1.castc");
        assert!(castc.is_file() && !dir.join(".citrus_texcache/albedo.png.1.cbc").exists());
        assert_eq!(&blob[9..25], &[0u8; 16]);
        let tex = parse_blob(&blob).unwrap();
        assert_eq!((tex.format, tex.width, tex.height, tex.mips.len()), (TexFormat::Astc6x6Srgb, 20, 12, 5));

        // The per-file cache reads back identically under the source stamp.
        let (mtime, len) = source_stamp(&png);
        let cached = read_cache(&castc, mtime, len).unwrap();
        assert_eq!((cached.format, cached.mips.clone()), (tex.format, tex.mips.clone()));
        assert!(read_cache(&castc, mtime, len + 1).is_none(), "stale stamp");
        assert_eq!(astc_cache_blob(&png, true, blocks).unwrap(), blob, "cache hit is stable");

        // A different footprint re-encodes for the cook.
        let small = parse_blob(&astc_cache_blob(&png, true, AstcBlocks { color: 4, data: 4 }).unwrap()).unwrap();
        assert_eq!(small.format, TexFormat::Astc4x4Srgb);

        // Seeding a shipped blob next to a freshly extracted copy re-binds it.
        let shipped = temp_dir("astc_seed");
        let copy = shipped.join("albedo.png");
        std::fs::copy(&png, &copy).unwrap();
        let bytes = seed_astc_cache(&copy, true, &blob, 4096).unwrap();
        assert_eq!(bytes, tex.mips.iter().map(|m| m.len() as u64).sum::<u64>());
        let (mtime, len) = source_stamp(&copy);
        let seeded = read_cache(&shipped.join(".citrus_texcache/albedo.png.1.castc"), mtime, len).unwrap();
        assert_eq!(seeded.mips, tex.mips);
        // seed_texture_cache routes ASTC blobs to the same artefact.
        assert_eq!(seed_texture_cache(&copy, true, &blob, 4096).unwrap(), Some(bytes));
        assert!(!shipped.join(".citrus_texcache/albedo.png.1.cbc").exists());
        // Garbage and BC blobs are refused by the ASTC seeder.
        assert!(seed_astc_cache(&copy, true, &blob[..blob.len() - 1], 4096).is_err());
        assert!(seed_astc_cache(&copy, true, &blob, 8).is_err(), "over max_dim");
        let bc = CompressedTexture { format: TexFormat::Bc7Srgb, width: 4, height: 4, mips: vec![vec![0; 16]] };
        assert!(seed_astc_cache(&copy, true, &cache_bytes(&bc, 0, 0), 4096).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&shipped);
    }

    #[test]
    fn unsampleable_cache_is_reencoded_not_returned() {
        // Test devices have BC and no ASTC (the render defaults): an ASTC
        // payload planted in the BC-tier cache must never come back.
        assert!(citrus_render::bc_supported() && !citrus_render::astc_supported());
        let dir = temp_dir("foreign");
        let png = dir.join("orm.png");
        image::RgbaImage::from_pixel(8, 8, image::Rgba([10, 20, 30, 255])).save(&png).unwrap();
        let astc = encode_astc(&ldr(8, 8, false, false), AstcBlocks::default());
        let (mtime, len) = source_stamp(&png);
        let cbc = dir.join(".citrus_texcache/orm.png.0.cbc");
        write_cache(&cbc, &astc, mtime, len).unwrap();
        let tex = load_texture_bc(&png, false).unwrap();
        assert_eq!(tex.format, TexFormat::Bc7Unorm);
        assert_eq!(read_cache(&cbc, mtime, len).unwrap().format, TexFormat::Bc7Unorm, "cache rewritten");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
