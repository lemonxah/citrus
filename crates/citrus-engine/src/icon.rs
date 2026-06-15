//! Procedural app icon (a citrus slice) plus desktop integration.
//!
//! X11/Windows take the icon directly from winit. Wayland compositors look
//! it up via the window's `app_id`, which points to a `.desktop` file, so we
//! install `citrus.desktop` and the icon into the user's XDG data dirs on
//! first run.

use std::path::PathBuf;

const SIZE: u32 = 256;

/// Render the icon: green rind, pale pith, segmented flesh.
pub fn rgba() -> (u32, u32, Vec<u8>) {
    let size = SIZE as f32;
    let center = size / 2.0;
    let radius = size * 0.47;
    let mut pixels = Vec::with_capacity((SIZE * SIZE * 4) as usize);

    let rind = [58.0, 125.0, 28.0];
    let pith = [240.0, 248.0, 224.0];
    let flesh = [186.0, 226.0, 70.0];
    let flesh_deep = [149.0, 200.0, 48.0];

    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            let r = (dx * dx + dy * dy).sqrt();
            // Anti-aliased outer edge.
            let alpha = ((radius - r) + 0.5).clamp(0.0, 1.0);
            if alpha <= 0.0 {
                pixels.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }
            let rn = r / radius;
            let angle = dy.atan2(dx);
            // Distance (radians) to the nearest of 8 segment separators.
            let seg = std::f32::consts::FRAC_PI_4;
            let to_line = ((angle.rem_euclid(seg)) - seg / 2.0).abs();
            let line_width = 0.05 + 0.05 * (1.0 - rn); // wider near the core
            let color = if rn > 0.90 {
                rind
            } else if !(0.10..=0.82).contains(&rn) || (seg / 2.0 - to_line) < line_width {
                pith
            } else {
                // Subtle radial shading inside the wedges.
                let t = (rn - 0.10) / 0.72;
                [
                    flesh[0] + (flesh_deep[0] - flesh[0]) * t,
                    flesh[1] + (flesh_deep[1] - flesh[1]) * t,
                    flesh[2] + (flesh_deep[2] - flesh[2]) * t,
                ]
            };
            pixels.extend_from_slice(&[
                color[0] as u8,
                color[1] as u8,
                color[2] as u8,
                (alpha * 255.0) as u8,
            ]);
        }
    }
    (SIZE, SIZE, pixels)
}

/// Install `citrus.desktop` and the icon into XDG data dirs if missing, so
/// Wayland compositors (which resolve icons by `app_id`) show ours.
/// Best-effort: failures only log.
pub fn install_desktop_entry() {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"));

    let icon_path = data.join("icons/hicolor/256x256/apps/citrus.png");
    if !icon_path.exists() {
        let (w, h, pixels) = rgba();
        let write = || -> anyhow::Result<()> {
            if let Some(parent) = icon_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let buffer: image::RgbaImage =
                image::ImageBuffer::from_raw(w, h, pixels).expect("icon buffer size");
            buffer.save(&icon_path)?;
            Ok(())
        };
        match write() {
            Ok(()) => tracing::info!("installed app icon to {}", icon_path.display()),
            Err(e) => tracing::debug!("installing app icon: {e:#}"),
        }
    }

    let desktop_path = data.join("applications/citrus.desktop");
    if !desktop_path.exists() {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "citrus".into());
        let entry = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=Citrus Editor\n\
             Comment=citrus engine editor\n\
             Exec={exe}\n\
             Icon=citrus\n\
             Terminal=false\n\
             Categories=Development;Graphics;3DGraphics;\n\
             StartupWMClass=citrus\n"
        );
        let write = || -> anyhow::Result<()> {
            if let Some(parent) = desktop_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&desktop_path, entry)?;
            Ok(())
        };
        match write() {
            Ok(()) => tracing::info!("installed {}", desktop_path.display()),
            Err(e) => tracing::debug!("installing desktop entry: {e:#}"),
        }
    }
}
