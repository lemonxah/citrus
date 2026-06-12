use anyhow::Result;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let config = citrus_engine::AppConfig {
        scene_path: std::env::args().nth(1),
        ..Default::default()
    };
    citrus_engine::run(config)
}
