use anyhow::{Result, bail};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Headless project tooling (the same logic the editor's File menu runs):
    //   citrus --new-project <parent-dir> <name>
    //   citrus --build [project-dir]   (defaults to the current directory)
    match args.get(1).map(String::as_str) {
        Some("--new-project") => {
            citrus_engine::init_logging();
            let (Some(parent), Some(name)) = (args.get(2), args.get(3)) else {
                bail!("usage: citrus --new-project <parent-dir> <name>");
            };
            let root = citrus_engine::scaffold_project(std::path::Path::new(parent), name)?;
            println!("created project at {}", root.display());
            return Ok(());
        }
        Some("--build") => {
            citrus_engine::init_logging();
            let dir = args
                .get(2)
                .map(std::path::PathBuf::from)
                .unwrap_or(std::env::current_dir()?);
            let exe = citrus_engine::build_project_dir(&dir)?;
            println!("built {}", exe.display());
            return Ok(());
        }
        _ => {}
    }

    citrus_engine::init_logging();
    let config = citrus_engine::AppConfig {
        scene_path: args.get(1).cloned(),
        ..Default::default()
    };
    citrus_engine::run(config)
}
