//! Cross-scene AVG32 runtime coordination.

use anyhow::{Context, Result, bail};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::animation_player::AnimationPlayer;
use crate::audio::Avg32Audio;
use crate::ending::EndingPlayer;
use crate::game::Avg32Game;
use crate::movie::{AviMovie, MoviePlayer};
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
    audio_waiting: Option<crate::vm::AudioKind>,
    fade: Option<FadePlayer>,
    flash: Option<FlashPlayer>,
    stretch_tween: Option<StretchTweenPlayer>,
    ending: Option<EndingPlayer>,
    movie: Option<MoviePlayer>,
    /// Cross-scene gosub return points (`VmAction::ChangeScene { call: true,
    /// .. }` / `0x20:2`). Kept separate from `Avg32Vm::call_stack`'s
    /// same-scene call history, unlike AVG32's real engine, which shares one
    /// unified stack across both — see the long comment on `call_stack` for
    /// the known-gap details.
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

    /// Reseeds the scenario's `Rand()` opcode state. The VM otherwise starts
    /// from a fixed constant so its behaviour stays deterministic across
    /// runs; a frontend that wants per-session variety (falling-leaf
    /// positions and the like) should call this once with a real entropy
    /// source, e.g. wall-clock time, right after opening the runtime.
    pub fn seed_rng(&mut self, seed: u64) {
        self.vm.seed_rng(seed);
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
        self.animations.tick(&mut self.renderer)?;
        if let Some(fade) = &mut self.fade {
            fade.tick(&mut self.renderer);
            if !fade.active {
                self.fade = None;
            }
        }
        if let Some(flash) = &mut self.flash {
            flash.tick(&mut self.renderer);
            if !flash.active {
                self.flash = None;
            }
        }
        if let Some(tween) = &mut self.stretch_tween {
            tween.tick(&mut self.renderer)?;
            if !tween.active {
                self.stretch_tween = None;
            }
        }
        if let Some(ending) = &mut self.ending {
            ending.tick(&mut self.renderer);
            if !ending.active() {
                if let Some(flag) = ending.cancellable_flag() {
                    self.vm.flags_mut().set_value(flag, 0);
                }
                self.ending = None;
            }
        }
        if let Some(movie) = &mut self.movie {
            movie.tick(&mut self.renderer)?;
            if !movie.active() {
                self.audio.stop_movie_audio();
                self.movie = None;
            }
        }
        Ok(())
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
        if self.fade.as_ref().is_some_and(|fade| fade.active) {
            return Ok(VmStop::Yield(VmAction::Wait {
                microseconds: 1_000,
                cancellable_flag: None,
            }));
        }
        if self.flash.as_ref().is_some_and(|flash| flash.active) {
            return Ok(VmStop::Yield(VmAction::Wait {
                microseconds: 1_000,
                cancellable_flag: None,
            }));
        }
        if self
            .stretch_tween
            .as_ref()
            .is_some_and(|tween| tween.active)
        {
            return Ok(VmStop::Yield(VmAction::Wait {
                microseconds: 1_000,
                cancellable_flag: None,
            }));
        }
        if let Some(kind) = &self.audio_waiting {
            if self.audio.playing(kind) {
                return Ok(VmStop::Yield(VmAction::Wait {
                    microseconds: 1_000,
                    cancellable_flag: None,
                }));
            }
            self.audio_waiting = None;
        }
        if self.movie.as_ref().is_some_and(MoviePlayer::blocks_vm) {
            return Ok(VmStop::Yield(VmAction::Wait {
                microseconds: 1_000,
                cancellable_flag: None,
            }));
        }
        if self.animations.single_active() {
            return Ok(VmStop::Yield(VmAction::Wait {
                microseconds: 1_000,
                cancellable_flag: None,
            }));
        }
        if let Some(ending) = &mut self.ending {
            if ending.should_cancel_for(input) {
                if let Some(flag) = ending.cancellable_flag() {
                    self.vm.flags_mut().set_value(flag, 1);
                }
                ending.cancel();
                self.ending = None;
            } else if ending.active() {
                return Ok(VmStop::Yield(VmAction::Wait {
                    microseconds: 1_000,
                    cancellable_flag: None,
                }));
            }
        }
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
            if let VmAction::PlayAudio { kind, .. } = action {
                let wait = match kind {
                    crate::vm::AudioKind::Bgm { wait, .. }
                    | crate::vm::AudioKind::Wave { wait, .. }
                    | crate::vm::AudioKind::Voice { wait } => *wait,
                    crate::vm::AudioKind::Effect | crate::vm::AudioKind::Movie { .. } => false,
                };
                if wait && self.audio.playing(kind) {
                    self.audio_waiting = Some(kind.clone());
                }
            }
            if matches!(action, VmAction::EndingSequence { .. }) {
                self.ending =
                    EndingPlayer::start(action, &self.game.resources, &mut self.renderer)?;
            }
            if let VmAction::PlayAudio {
                kind: crate::vm::AudioKind::Movie { looped, wait },
                name,
            } = action
            {
                let bytes = self
                    .game
                    .resources
                    .read("AVI", name)
                    .with_context(|| format!("open AVG32 FMV {name}"))?;
                let avi =
                    AviMovie::parse(&bytes).with_context(|| format!("parse AVG32 FMV {name}"))?;
                if let Some(audio) = &avi.audio {
                    self.audio.play_movie_pcm(
                        audio.sample_rate,
                        audio.channels,
                        audio.bits_per_sample,
                        &audio.pcm,
                    )?;
                }
                self.movie = MoviePlayer::start(&avi, *looped, *wait, &mut self.renderer)?;
            }
            if let VmAction::Fade {
                pattern,
                color,
                microseconds,
            } = action
            {
                let resolved = color
                    .map(|color| [color[0] as u8, color[1] as u8, color[2] as u8])
                    .or_else(|| pattern.map(|pattern| self.game.config.fade_color(pattern)));
                if let Some(resolved) = resolved {
                    let step = microseconds.unwrap_or(self.game.config.default_fade_microseconds());
                    self.fade = Some(FadePlayer::start(&mut self.renderer, resolved, step));
                }
            }
            if let VmAction::Flash {
                color,
                microseconds,
                repetitions,
            } = action
            {
                self.flash =
                    FlashPlayer::start(&mut self.renderer, *color, *microseconds, *repetitions);
            }
            if let VmAction::BufferStretchTween { .. } = action {
                self.stretch_tween = StretchTweenPlayer::start(action, &mut self.renderer)?;
            }
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
                next.seed_rng(self.vm.rng_state());
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
                    vm.seed_rng(self.vm.rng_state());
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
            audio_waiting: None,
            fade: None,
            flash: None,
            stretch_tween: None,
            ending: None,
            movie: None,
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

/// Stateful implementation of `PDTMGR::Effect(cmd=9999)`.  The native code
/// snapshots the source PDT once, then changes the source crop rectangle at
/// each timer tick while stretching into a fixed destination rectangle.
#[derive(Debug)]
struct StretchTweenPlayer {
    source: crate::surface::Surface,
    destination: u32,
    source_initial: [i32; 4],
    source_final: [i32; 4],
    destination_rect: [i32; 4],
    steps: i32,
    current: i32,
    interval: Duration,
    due: Instant,
    active: bool,
}

impl StretchTweenPlayer {
    fn start(action: &VmAction, renderer: &mut Avg32Renderer) -> Result<Option<Self>> {
        let VmAction::BufferStretchTween {
            source,
            destination,
            source_initial,
            source_final,
            destination_rect,
            steps,
            microseconds,
        } = action
        else {
            return Ok(None);
        };
        if *steps <= 0 {
            let source = renderer.buffer_clone(*source)?;
            renderer.stretch_surface_to(&source, *destination, *source_final, *destination_rect)?;
            return Ok(None);
        }
        let mut player = Self {
            source: renderer.buffer_clone(*source)?,
            destination: *destination,
            source_initial: *source_initial,
            source_final: *source_final,
            destination_rect: *destination_rect,
            steps: *steps,
            current: 0,
            interval: Duration::from_micros((*microseconds).max(1) as u64),
            due: Instant::now(),
            active: true,
        };
        player.render_step(renderer)?;
        player.due = Instant::now() + player.interval;
        Ok(Some(player))
    }

    fn tick(&mut self, renderer: &mut Avg32Renderer) -> Result<()> {
        if !self.active || Instant::now() < self.due {
            return Ok(());
        }
        self.render_step(renderer)?;
        self.due = Instant::now() + self.interval;
        Ok(())
    }

    fn render_step(&mut self, renderer: &mut Avg32Renderer) -> Result<()> {
        self.current = (self.current + 1).min(self.steps);
        let crop = std::array::from_fn(|index| {
            self.source_initial[index]
                + ((self.source_final[index] - self.source_initial[index]) * self.current)
                    / self.steps
        });
        renderer.stretch_surface_to(&self.source, self.destination, crop, self.destination_rect)?;
        if self.current >= self.steps {
            self.active = false;
        }
        Ok(())
    }
}

/// Drives AVG32's 16-phase ordered-dither screen fade (`PDTMGR::ScreenFade`,
/// opcode `0x13`) over real time. The original decoder blocks the whole
/// scenario while this runs (`fadecmd` gates `SCENARIO::d0b` exactly like
/// `anmflag` gates animation decode), so `Avg32Runtime::advance` parks the
/// VM on a `Wait` while `active()`.
#[derive(Debug)]
struct FadePlayer {
    color: [u8; 3],
    /// Next dithered phase to draw, `0..=15`; `16` means "draw the final
    /// solid fill and stop" (mirrors the reference's `fadecount==16` check).
    phase: usize,
    due: Instant,
    interval: Duration,
    active: bool,
}

impl FadePlayer {
    fn start(renderer: &mut Avg32Renderer, color: [u8; 3], microseconds_per_step: u32) -> Self {
        let interval = Duration::from_micros(u64::from(microseconds_per_step.max(1)));
        let mut player = Self {
            color,
            phase: 0,
            due: Instant::now(),
            interval,
            active: true,
        };
        player.step(renderer);
        player.due = Instant::now() + interval;
        player
    }

    fn tick(&mut self, renderer: &mut Avg32Renderer) {
        if !self.active || Instant::now() < self.due {
            return;
        }
        self.step(renderer);
        self.due = Instant::now() + self.interval;
    }

    fn step(&mut self, renderer: &mut Avg32Renderer) {
        if self.phase < 16 {
            renderer.fade_display_phase(self.color, self.phase, false);
            self.phase += 1;
        } else {
            renderer.fade_display_phase(self.color, 15, true);
            self.active = false;
        }
    }
}

#[derive(Debug)]
struct FlashPlayer {
    color: [u8; 3],
    remaining_phases: u32,
    showing_color: bool,
    due: Instant,
    interval: Duration,
    active: bool,
}

impl FlashPlayer {
    fn start(
        renderer: &mut Avg32Renderer,
        color: [i32; 3],
        microseconds: u32,
        repetitions: u32,
    ) -> Option<Self> {
        if repetitions == 0 {
            return None;
        }
        renderer.save_display();
        let color = [color[0] as u8, color[1] as u8, color[2] as u8];
        renderer.clear_display([color[0], color[1], color[2], 255]);
        Some(Self {
            color,
            remaining_phases: repetitions.saturating_mul(2).saturating_sub(1),
            showing_color: true,
            due: Instant::now() + Duration::from_micros(u64::from(microseconds.max(1))),
            interval: Duration::from_micros(u64::from(microseconds.max(1))),
            active: true,
        })
    }

    fn tick(&mut self, renderer: &mut Avg32Renderer) {
        if !self.active || Instant::now() < self.due {
            return;
        }
        if self.remaining_phases == 0 {
            renderer.restore_display();
            self.active = false;
            return;
        }
        if self.showing_color {
            renderer.restore_display();
        } else {
            renderer.clear_display([self.color[0], self.color[1], self.color[2], 255]);
        }
        self.showing_color = !self.showing_color;
        self.remaining_phases -= 1;
        self.due = Instant::now() + self.interval;
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
