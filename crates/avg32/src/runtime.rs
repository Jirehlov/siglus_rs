//! Cross-scene AVG32 runtime coordination.

use anyhow::{Context, Result, bail};
use std::path::Path;

use crate::animation_player::AnimationPlayer;
use crate::audio::Avg32Audio;
use crate::game::Avg32Game;
use crate::render::Avg32Renderer;
use crate::text::MessageWindow;
use crate::vm::{Avg32Program, Avg32Vm, Input, VmAction, VmStop};

#[derive(Debug)]
pub struct Avg32Runtime {
    game: Avg32Game,
    scene_name: String,
    vm: Avg32Vm,
    renderer: Avg32Renderer,
    animations: AnimationPlayer,
    message: MessageWindow,
    audio: Avg32Audio,
    scene_stack: Vec<(String, Avg32Vm)>,
}

impl Avg32Runtime {
    pub fn open(root: impl AsRef<std::path::Path>) -> Result<Self> {
        let game = Avg32Game::open(root)?;
        let scene_name = game
            .configured_start_scene()
            .ok_or_else(|| anyhow::anyhow!("AVG32 game has no usable SEEN_START scene"))?
            .to_owned();
        Self::from_scene(game, scene_name)
    }

    pub fn scene_name(&self) -> &str {
        &self.scene_name
    }

    pub fn vm(&self) -> &Avg32Vm {
        &self.vm
    }

    pub fn renderer(&self) -> &Avg32Renderer {
        &self.renderer
    }

    pub fn message(&self) -> &MessageWindow {
        &self.message
    }

    pub fn config(&self) -> &crate::config::Avg32Config {
        &self.game.config
    }

    pub fn tick(&mut self) -> Result<()> {
        self.animations.tick(&mut self.renderer)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let scene = self.scene_name.as_bytes();
        let flags = self.vm.flags().encode();
        if scene.len() > u16::MAX as usize {
            bail!("avg32: scene name is too long to save");
        }
        let mut output = Vec::with_capacity(32 + scene.len() + flags.len());
        output.extend_from_slice(b"AVG32RS\0");
        output.extend_from_slice(&(scene.len() as u16).to_le_bytes());
        output.extend_from_slice(&(self.vm.pc() as u64).to_le_bytes());
        output.extend_from_slice(&(flags.len() as u32).to_le_bytes());
        output.extend_from_slice(scene);
        output.extend_from_slice(&flags);
        std::fs::write(path, output)?;
        Ok(())
    }

    pub fn load(root: impl AsRef<Path>, path: impl AsRef<Path>) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let mut at = 0usize;
        if take(&bytes, &mut at, 8)? != b"AVG32RS\0" {
            bail!("avg32: not an AVG32 runtime save");
        }
        let scene_len =
            u16::from_le_bytes(take(&bytes, &mut at, 2)?.try_into().expect("two bytes")) as usize;
        let pc =
            u64::from_le_bytes(take(&bytes, &mut at, 8)?.try_into().expect("eight bytes")) as usize;
        let flags_len =
            u32::from_le_bytes(take(&bytes, &mut at, 4)?.try_into().expect("four bytes")) as usize;
        let scene = std::str::from_utf8(take(&bytes, &mut at, scene_len)?)?.to_owned();
        let flags = crate::vm::VmFlags::decode(take(&bytes, &mut at, flags_len)?)?;
        if at != bytes.len() {
            bail!("avg32: trailing bytes in runtime save");
        }
        let game = Avg32Game::open(root)?;
        let mut runtime = Self::from_scene(game, scene)?;
        runtime.vm.replace_flags(flags);
        runtime.vm.set_pc(pc)?;
        Ok(runtime)
    }

    pub fn advance(&mut self, input: Input, max_instructions: usize) -> Result<VmStop> {
        let outcome = self.vm.run(input, max_instructions)?;
        if let VmStop::Yield(action) = &outcome {
            self.renderer
                .apply(action, &self.game.resources, &self.game.config)
                .with_context(|| {
                    format!(
                        "render AVG32 action at scene bytecode {:#x}: {action:?}",
                        self.vm.last_opcode_pc()
                    )
                })?;
            self.animations
                .apply(action, &self.game.resources)
                .with_context(|| format!("animate AVG32 action {action:?}"))?;
            self.message.apply(action);
            self.audio
                .apply(action, &self.game.resources)
                .with_context(|| format!("play AVG32 action {action:?}"))?;
            if let VmAction::LoadArea { definition, .. } = action {
                let map = crate::ard::AreaMap::parse(&self.game.resources.read("ARD", definition)?)
                    .with_context(|| format!("load AVG32 area map {definition}"))?;
                self.vm.set_area_map(map);
            }
        }
        match &outcome {
            VmStop::Yield(VmAction::ChangeScene { scene, call }) => {
                let scene_name = self.scene_name_for(*scene)?;
                let mut next = self.vm_for_scene(&scene_name)?;
                next.replace_flags(self.vm.flags().clone());
                if *call {
                    let previous = std::mem::replace(&mut self.vm, next);
                    self.scene_stack.push((self.scene_name.clone(), previous));
                } else {
                    self.vm = next;
                }
                self.scene_name = scene_name;
            }
            VmStop::Yield(VmAction::ReturnScene) => {
                if let Some((scene_name, mut vm)) = self.scene_stack.pop() {
                    vm.replace_flags(self.vm.flags().clone());
                    self.scene_name = scene_name;
                    self.vm = vm;
                }
            }
            VmStop::Yield(VmAction::SaveRequest { slot }) => {
                let path = self.save_slot_path(*slot);
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                self.save(path)?;
            }
            VmStop::Yield(VmAction::LoadRequest { slot }) => {
                let path = self.save_slot_path(*slot);
                if path.is_file() {
                    *self = Self::load(&self.game.layout.root, path)?;
                }
            }
            _ => {}
        }
        Ok(outcome)
    }

    fn from_scene(game: Avg32Game, scene_name: String) -> Result<Self> {
        let vm = Self::program_for(&game, &scene_name)?;
        Ok(Self {
            game,
            scene_name,
            vm: Avg32Vm::new(vm).with_extended_text(),
            renderer: Avg32Renderer::new()?,
            animations: AnimationPlayer::default(),
            message: MessageWindow::default(),
            audio: Avg32Audio::new(),
            scene_stack: Vec::new(),
        })
    }

    fn vm_for_scene(&self, scene_name: &str) -> Result<Avg32Vm> {
        Ok(Avg32Vm::new(Self::program_for(&self.game, scene_name)?).with_extended_text())
    }

    fn program_for(game: &Avg32Game, scene_name: &str) -> Result<Avg32Program> {
        Avg32Program::parse(game.read_scene(scene_name)?)
    }

    fn scene_name_for(&self, scene: u32) -> Result<String> {
        let candidates = [
            format!("SEEN{scene:03}.TXT"),
            format!("SEEN{scene:04}.TXT"),
            format!("SEEN{scene}.TXT"),
        ];
        candidates
            .iter()
            .find(|candidate| {
                self.game
                    .scene_names()
                    .any(|name| name.eq_ignore_ascii_case(candidate))
            })
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("AVG32 scene {scene} is not present in SEEN.TXT"))
    }

    fn save_slot_path(&self, slot: u32) -> std::path::PathBuf {
        self.game
            .layout
            .root
            .join("SAVE")
            .join(format!("AVG32.{slot:03}.sav"))
    }
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, length: usize) -> Result<&'a [u8]> {
    let end = at
        .checked_add(length)
        .ok_or_else(|| anyhow::anyhow!("avg32: save offset overflows"))?;
    let slice = bytes
        .get(*at..end)
        .ok_or_else(|| anyhow::anyhow!("avg32: truncated runtime save"))?;
    *at = end;
    Ok(slice)
}
