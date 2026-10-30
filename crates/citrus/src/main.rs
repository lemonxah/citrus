use anyhow::{Result, bail};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Headless project tooling (the same logic the editor's File menu runs):
    //   citrus --new-project <parent-dir> <name> [template]   (fps, tps, topdown, strategy, vr_shooter, vr_social)
    //   citrus --build [project-dir] [--target host|windows]   (defaults to the current directory, host)
    match args.get(1).map(String::as_str) {
        Some("--new-project") => {
            citrus_engine::init_logging();
            let (Some(parent), Some(name)) = (args.get(2), args.get(3)) else {
                let ids: Vec<&str> = citrus_engine::templates::TEMPLATES.iter().map(|t| t.id).collect();
                bail!("usage: citrus --new-project <parent-dir> <name> [template: {}]", ids.join(" | "));
            };
            let template = args.get(4).map(String::as_str).unwrap_or(citrus_engine::templates::DEFAULT_TEMPLATE);
            let root = citrus_engine::scaffold_project(std::path::Path::new(parent), name, template)?;
            println!("created project at {}", root.display());
            return Ok(());
        }
        Some("--build") => {
            citrus_engine::init_logging();
            // citrus --build [dir] [--target host|windows]
            let mut dir = None;
            let mut target = citrus_engine::BuildTarget::Host;
            let mut rest = args[2..].iter();
            while let Some(arg) = rest.next() {
                if arg == "--target" {
                    let name = rest.next().map(String::as_str).unwrap_or("");
                    target = citrus_engine::BuildTarget::parse(name)
                        .ok_or_else(|| anyhow::anyhow!("unknown build target '{name}' (host | windows)"))?;
                } else {
                    dir = Some(std::path::PathBuf::from(arg));
                }
            }
            let dir = match dir {
                Some(d) => d,
                None => std::env::current_dir()?,
            };
            let exe = citrus_engine::build_project_dir(&dir, target)?;
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
