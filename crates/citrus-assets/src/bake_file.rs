//! Baked-lighting asset files (compact little-endian binary; HDR float data
//! is too large for RON).
//!
//! - `.lightmap`: per static object, an HDR irradiance lightmap (the baked
//!   GI sampled by the standard shader / GI pipeline).
//! - `.lightdata`: light-probe volumes + SH-L1 irradiance, used to light
//!   dynamically moving objects (or leave them unlit, per how it was baked).
//!
//! Both sit next to their `.scene` (same stem) and load with it.

use std::io::{Cursor, Read, Write};
use std::path::Path;

use anyhow::{Context as _, Result, bail};

pub const LIGHTMAP_EXTENSION: &str = "lightmap";
pub const LIGHTDATA_EXTENSION: &str = "lightdata";

const LIGHTMAP_MAGIC: &[u8; 8] = b"CITRSLM1";
const LIGHTDATA_MAGIC: &[u8; 8] = b"CITRSLD2";

/// One static object's baked lightmap (size×size RGBA32F).
pub struct LightmapEntry {
    /// Scene object index this lightmap belongs to.
    pub object: u32,
    pub size: u32,
    pub pixels: Vec<f32>,
}

#[derive(Default)]
pub struct LightmapFile {
    pub entries: Vec<LightmapEntry>,
}

/// A probe volume's placement + which SH range it owns.
pub struct ProbeVolumeData {
    /// World → volume-local, column-major (glam `to_cols_array`).
    pub world_to_local: [f32; 16],
    pub size: [f32; 3],
    pub counts: [u32; 3],
    pub sh_base: u32,
    /// True for FluxVoxel voxel volumes (build-time baked from FluxVoxel Lights).
    pub flux_voxel: bool,
}

#[derive(Default)]
pub struct LightDataFile {
    pub volumes: Vec<ProbeVolumeData>,
    /// SH-L1 per probe: 4 coefficients × RGB = 12 floats.
    pub probes: Vec<[f32; 12]>,
}

// ---- little-endian helpers ------------------------------------------------

fn wr_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn wr_f32s(out: &mut Vec<u8>, v: &[f32]) {
    out.extend_from_slice(bytemuck::cast_slice(v));
}

struct Reader<'a>(Cursor<&'a [u8]>);
impl Reader<'_> {
    fn u32(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        self.0.read_exact(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn f32s(&mut self, n: usize) -> Result<Vec<f32>> {
        let mut bytes = vec![0u8; n * 4];
        self.0.read_exact(&mut bytes)?;
        Ok(bytemuck::cast_slice(&bytes).to_vec())
    }
    fn array<const N: usize>(&mut self) -> Result<[f32; N]> {
        let v = self.f32s(N)?;
        let mut out = [0.0; N];
        out.copy_from_slice(&v);
        Ok(out)
    }
}

// ---- .lightmap ------------------------------------------------------------

pub fn save_lightmaps(path: impl AsRef<Path>, file: &LightmapFile) -> Result<()> {
    let path = path.as_ref();
    let mut out = Vec::new();
    out.extend_from_slice(LIGHTMAP_MAGIC);
    wr_u32(&mut out, file.entries.len() as u32);
    for e in &file.entries {
        wr_u32(&mut out, e.object);
        wr_u32(&mut out, e.size);
        wr_f32s(&mut out, &e.pixels);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::File::create(path)
        .and_then(|mut f| f.write_all(&out))
        .with_context(|| format!("writing {}", path.display()))
}

pub fn load_lightmaps(path: impl AsRef<Path>) -> Result<LightmapFile> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut magic = [0u8; 8];
    if bytes.len() < 8 {
        bail!("{} is too short", path.display());
    }
    magic.copy_from_slice(&bytes[..8]);
    if &magic != LIGHTMAP_MAGIC {
        bail!("{} is not a citrus .lightmap", path.display());
    }
    let mut r = Reader(Cursor::new(&bytes[8..]));
    let count = r.u32()? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let object = r.u32()?;
        let size = r.u32()?;
        let pixels = r.f32s((size * size * 4) as usize)?;
        entries.push(LightmapEntry {
            object,
            size,
            pixels,
        });
    }
    Ok(LightmapFile { entries })
}

// ---- .lightdata -----------------------------------------------------------

pub fn save_lightdata(path: impl AsRef<Path>, file: &LightDataFile) -> Result<()> {
    let path = path.as_ref();
    let mut out = Vec::new();
    out.extend_from_slice(LIGHTDATA_MAGIC);
    wr_u32(&mut out, file.volumes.len() as u32);
    for v in &file.volumes {
        wr_f32s(&mut out, &v.world_to_local);
        wr_f32s(&mut out, &v.size);
        for c in v.counts {
            wr_u32(&mut out, c);
        }
        wr_u32(&mut out, v.sh_base);
        wr_u32(&mut out, v.flux_voxel as u32);
    }
    wr_u32(&mut out, file.probes.len() as u32);
    for p in &file.probes {
        wr_f32s(&mut out, p);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::File::create(path)
        .and_then(|mut f| f.write_all(&out))
        .with_context(|| format!("writing {}", path.display()))
}

pub fn load_lightdata(path: impl AsRef<Path>) -> Result<LightDataFile> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if bytes.len() < 8 || &bytes[..8] != LIGHTDATA_MAGIC {
        bail!("{} is not a citrus .lightdata", path.display());
    }
    let mut r = Reader(Cursor::new(&bytes[8..]));
    let volume_count = r.u32()? as usize;
    let mut volumes = Vec::with_capacity(volume_count);
    for _ in 0..volume_count {
        let world_to_local = r.array::<16>()?;
        let size = r.array::<3>()?;
        let counts = [r.u32()?, r.u32()?, r.u32()?];
        let sh_base = r.u32()?;
        let flux_voxel = r.u32()? != 0;
        volumes.push(ProbeVolumeData {
            world_to_local,
            size,
            counts,
            sh_base,
            flux_voxel,
        });
    }
    let probe_count = r.u32()? as usize;
    let mut probes = Vec::with_capacity(probe_count);
    for _ in 0..probe_count {
        probes.push(r.array::<12>()?);
    }
    Ok(LightDataFile { volumes, probes })
}
