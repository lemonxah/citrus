//! Android entry point for a citrus game. The APK carries the game's assets
//! (built by tools/android/build_apk.sh); they're extracted to internal storage
//! on first run and the boot scene comes from their project.citrus.

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: citrus_engine::AndroidApp) {
    citrus_engine::init_logging();
    let root = match citrus_engine::android_assets_root(&app) {
        Ok(root) => root,
        Err(e) => {
            tracing::error!("extracting game assets: {e:#}");
            return;
        }
    };
    let config = match citrus_engine::GameConfig::from_project_dir(root) {
        Ok(config) => config,
        Err(e) => {
            tracing::error!("reading project.citrus: {e:#}");
            return;
        }
    };
    if let Err(e) = citrus_engine::run_game_android(app, config, |_| {}) {
        tracing::error!("game exited: {e:#}");
    }
}
