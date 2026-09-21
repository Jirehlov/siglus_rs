use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use avg32::Avg32Game;

fn main() -> Result<()> {
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("usage: avg32_probe <game-root>"))?;
    let game = Avg32Game::open(&root)?;
    println!("engine: {:?}", game.layout.kind);
    println!("root: {}", game.layout.root.display());
    println!("scenes: {}", game.scene_names().count());
    if let Some(start) = game.configured_start_scene() {
        let header = game.scene_header(start)?;
        println!("start scene: {start}");
        println!("start scene labels: {}", header.labels.len());
        println!("start scene code offset: {:#x}", header.code_offset);
    } else {
        bail!("Gameexe.ini did not resolve #SEEN_START to a scene in SEEN.TXT");
    }
    let logo = game
        .load_pdt("AIRLOGO")
        .context("failed to decode pdt/AIRLOGO.PDT")?;
    println!("AIRLOGO: {}x{}", logo.width, logo.height);
    Ok(())
}
