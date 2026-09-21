//! Time-driven playback of parsed AVG32 ANM32 cells.
//!
//! Mirrors the original engine's `SYSTEM::AnimationExec`/`MultiAnimationExec`:
//! a playing scene advances through *all* of its streams in order, not just
//! stream 0. A single-shot animation (`PA` subcommand `$10`/`$16`/`$18`) stops
//! once its last stream's cells are spent; a multi animation (`$12`/`$30`)
//! loops back to stream 0 of the same scene until an explicit `StopAnimation`.

use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::animation::{AnimationCell, Avg32Animation};
use crate::pdt::decode_pdt;
use crate::render::Avg32Renderer;
use crate::resource::Avg32Resources;
use crate::surface::Surface;
use crate::vm::VmAction;

#[derive(Debug)]
struct PlayingAnimation {
    name: String,
    scene: u32,
    animation: Rc<Avg32Animation>,
    stream: usize,
    stream_count: usize,
    frames: Vec<AnimationCell>,
    frame: usize,
    source: Surface,
    due: Instant,
    multi: bool,
}

#[derive(Debug, Default)]
pub struct AnimationPlayer {
    playing: Vec<PlayingAnimation>,
}

impl AnimationPlayer {
    pub fn apply(&mut self, action: &VmAction, resources: &Avg32Resources) -> Result<()> {
        match action {
            VmAction::StartAnimation {
                name,
                scene,
                scenes,
                multi,
                ..
            } => {
                let animation = Rc::new(
                    Avg32Animation::parse(resources.read("ANM", name)?)
                        .with_context(|| format!("failed to decode AVG32 animation {name}"))?,
                );
                let image = decode_pdt(&resources.read("PDT", &animation.source_pdt)?)
                    .with_context(|| {
                        format!(
                            "failed to decode animation source PDT {}",
                            animation.source_pdt
                        )
                    })?;
                let scenes = if *multi { scenes.clone() } else { vec![*scene] };
                let mut items = Vec::with_capacity(scenes.len());
                for scene in scenes {
                    let stream_count = animation.scene_stream_count(scene as usize)?;
                    let frames = animation.stream_frames(scene as usize, 0)?;
                    items.push(PlayingAnimation {
                        name: name.clone(),
                        scene,
                        animation: animation.clone(),
                        stream: 0,
                        stream_count,
                        frames,
                        frame: 0,
                        source: Surface::from_pdt(image.clone()),
                        due: Instant::now(),
                        multi: *multi,
                    });
                }
                if *multi {
                    self.playing.extend(items);
                } else {
                    self.playing.retain(|playing| !playing.multi);
                    self.playing.extend(items);
                }
            }
            VmAction::StopAnimation {
                name,
                scene,
                clear_all,
            } => {
                if *clear_all {
                    self.playing.clear();
                } else {
                    self.playing.retain(|playing| {
                        name.as_ref().is_some_and(|name| name != &playing.name)
                            || scene.is_some_and(|scene| scene != playing.scene)
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn tick(&mut self, renderer: &mut Avg32Renderer) -> Result<()> {
        let now = Instant::now();
        let mut error = None;
        self.playing.retain_mut(|playing| {
            if now < playing.due {
                return true;
            }
            if playing.frame >= playing.frames.len() {
                playing.stream += 1;
                if playing.stream >= playing.stream_count {
                    if !playing.multi {
                        return false;
                    }
                    playing.stream = 0;
                }
                playing.frames = match playing
                    .animation
                    .stream_frames(playing.scene as usize, playing.stream)
                {
                    Ok(frames) => frames,
                    Err(err) => {
                        error = Some(err);
                        return false;
                    }
                };
                playing.frame = 0;
                if playing.frames.is_empty() {
                    return playing.multi;
                }
            }
            let cell = playing.frames[playing.frame];
            renderer.blit_to_display(&playing.source, cell.source_rect, cell.destination);
            playing.frame += 1;
            playing.due = now + Duration::from_micros(u64::from(cell.wait_microseconds));
            true
        });
        match error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}
