use anyhow::Result;

fn main() -> Result<()> {
    citrus_engine::init_logging();

    let config = citrus_engine::AppConfig {
        scene_path: std::env::args().nth(1),
        ..Default::default()
    };
    citrus_engine::run(config)
}
