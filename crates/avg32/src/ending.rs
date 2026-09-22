//! AVG32 multi-PDT ending and opening sequences (`0x6a`).
//!
//! The original interpreter keeps this command active while it scrolls or
//! advances a list of PDT images.  Keeping that state outside the bytecode VM
//! lets the VM remain platform-neutral while still preserving its blocking
//! execution semantics.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::pdt::decode_pdt;
use crate::render::Avg32Renderer;
use crate::resource::Avg32Resources;
use crate::surface::Surface;
use crate::vm::{Input, VmAction};

#[derive(Debug)]
struct EndingFrame {
    image: Surface,
    offset: i32,
}

#[derive(Debug)]
pub struct EndingPlayer {
    mode: u8,
    alignment: u8,
    position: i32,
    interval: Duration,
    pixels_per_step: i32,
    cancellable_flag: Option<u32>,
    frames: Vec<EndingFrame>,
    line: i32,
    total_lines: i32,
    next_frame: usize,
    due: Instant,
    active: bool,
}

impl EndingPlayer {
    pub fn start(
        action: &VmAction,
        resources: &Avg32Resources,
        renderer: &mut Avg32Renderer,
    ) -> Result<Option<Self>> {
        let VmAction::EndingSequence {
            mode,
            alignment,
            position,
            wait,
            pixels_per_step,
            cancellable_flag,
            frames,
        } = action
        else {
            return Ok(None);
        };
        if frames.is_empty() || *mode == 0x05 {
            return Ok(None);
        }
        let mut loaded = Vec::with_capacity(frames.len());
        let mut offset = 0;
        for (name, spacing) in frames {
            let image = Surface::from_pdt(
                decode_pdt(&resources.read("PDT", name)?)
                    .with_context(|| format!("decode AVG32 ending PDT {name}"))?,
            );
            loaded.push(EndingFrame { image, offset });
            offset =
                offset.saturating_add(loaded.last().expect("frame exists").image.height() as i32);
            offset = offset.saturating_add(*spacing);
        }
        let mut player = Self {
            mode: *mode,
            alignment: *alignment,
            position: *position,
            interval: Duration::from_millis((*wait).max(1) as u64),
            pixels_per_step: (*pixels_per_step).max(1),
            cancellable_flag: *cancellable_flag,
            frames: loaded,
            line: 0,
            total_lines: offset.max(0),
            next_frame: 0,
            due: Instant::now(),
            active: true,
        };
        renderer.save_display();
        player.redraw(renderer);
        Ok(Some(player))
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn cancellable_flag(&self) -> Option<u32> {
        self.cancellable_flag
    }

    pub fn cancel(&mut self) {
        self.active = false;
    }

    pub fn tick(&mut self, renderer: &mut Avg32Renderer) {
        if !self.active || Instant::now() < self.due {
            return;
        }
        self.due = Instant::now() + self.interval;
        match self.mode {
            0x10 | 0x20 | 0x30 => {
                self.line = self.line.saturating_add(self.pixels_per_step);
                self.redraw(renderer);
                if self.line >= self.total_lines {
                    self.active = false;
                }
            }
            0x03 | 0x04 => {
                self.next_frame += 1;
                if self.next_frame >= self.frames.len() {
                    self.active = false;
                } else {
                    self.redraw(renderer);
                }
            }
            _ => self.active = false,
        }
    }

    fn redraw(&mut self, renderer: &mut Avg32Renderer) {
        match self.mode {
            0x10 | 0x20 | 0x30 => {
                renderer.restore_display();
                for frame in &self.frames {
                    let y = 479 - (self.line - frame.offset);
                    let x = match self.alignment {
                        1 => self.position - frame.image.width() as i32 / 2,
                        3 => self.position - frame.image.width() as i32,
                        _ => self.position,
                    };
                    renderer.blit_to_display(
                        &frame.image,
                        [
                            0,
                            0,
                            frame.image.width() as i32 - 1,
                            frame.image.height() as i32 - 1,
                        ],
                        [x, y],
                    );
                }
            }
            0x03 | 0x04 => {
                let frame = &self.frames[self.next_frame];
                renderer.clear_display([0, 0, 0, 255]);
                renderer.blit_to_display(
                    &frame.image,
                    [
                        0,
                        0,
                        frame.image.width() as i32 - 1,
                        frame.image.height() as i32 - 1,
                    ],
                    [0, 0],
                );
            }
            _ => {}
        }
    }

    pub fn should_cancel_for(&self, input: Input) -> bool {
        self.mode == 0x30 && matches!(input, Input::Pointer { button, .. } if button != 0)
    }
}
