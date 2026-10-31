//! Export the real scaffolded template scene data for offline Hub previews.
//! cargo run -p citrus-engine --example template_preview_data -- /tmp/citrus-preview-scenes
use anyhow::Result;
use std::path::PathBuf;

fn main() -> Result<()> {
    let root = PathBuf::from(std::env::args().nth(1).expect("output folder"));
    std::fs::create_dir_all(&root)?;
    for template in citrus_engine::templates::TEMPLATES {
        let project = citrus_engine::scaffold_project(&root, template.id, template.id)?;
        let settings = citrus_assets::load_project_file(&project)?;
        let scene = citrus_assets::load_scene_file(project.join(settings.boot_scene.unwrap()))?;
        std::fs::write(project.join("preview.json"), serde_json::to_vec_pretty(&scene)?)?;
        println!("{}", project.display());
    }
    Ok(())
}
