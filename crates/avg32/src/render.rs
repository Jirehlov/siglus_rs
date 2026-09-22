//! AVG32 PDT-buffer renderer.
//!
//! The renderer is deliberately CPU-first. A desktop or Switch frontend only
//! needs to upload `display()` as RGBA8; all AVG32 buffer semantics remain
//! identical across backends.

use anyhow::{Context, Result};

use crate::config::Avg32Config;
use crate::pdt::decode_pdt;
use crate::resource::Avg32Resources;
use crate::surface::{AVG32_HEIGHT, AVG32_WIDTH, Surface, SurfaceBank};
use crate::vm::{GraphicSource, VmAction};

#[derive(Debug, Clone)]
pub struct Avg32Renderer {
    buffers: SurfaceBank,
    backup: Option<Surface>,
}

impl Default for Avg32Renderer {
    fn default() -> Self {
        Self::new().expect("AVG32 fixed buffers are valid")
    }
}

impl Avg32Renderer {
    pub fn new() -> Result<Self> {
        Ok(Self {
            buffers: SurfaceBank::new()?,
            backup: None,
        })
    }

    pub fn display(&self) -> &Surface {
        self.buffers.get(0).expect("display buffer is permanent")
    }

    pub fn buffers(&self) -> &SurfaceBank {
        &self.buffers
    }

    pub(crate) fn buffer_clone(&self, index: u32) -> Result<Surface> {
        self.buffers
            .get(index as usize)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("AVG32 graphics buffer {index} is out of range"))
    }

    pub(crate) fn stretch_surface_to(
        &mut self,
        source: &Surface,
        destination: u32,
        source_rect: [i32; 4],
        destination_rect: [i32; 4],
    ) -> Result<()> {
        self.buffers
            .get_mut(destination as usize)
            .ok_or_else(|| {
                anyhow::anyhow!("AVG32 stretch destination buffer {destination} is out of range")
            })?
            .stretch_copy(source, source_rect, destination_rect);
        Ok(())
    }

    /// Applies deterministic graphics requests emitted by the VM. Effects are
    /// represented by their final composited frame here; time-based transition
    /// presentation is handled by a frontend scheduler.
    pub fn apply(
        &mut self,
        action: &VmAction,
        resources: &Avg32Resources,
        config: &Avg32Config,
    ) -> Result<()> {
        match action {
            VmAction::LoadGraphic {
                name,
                target,
                effect,
                direct_effect,
            } => {
                // An empty AVG32 PDT name allocates a cleared working buffer.
                // It occurs in AIR's transition scenes and is not a request for
                // a literal `.PDT` filename.
                if name.is_empty() {
                    let blank = Surface::transparent(AVG32_WIDTH, AVG32_HEIGHT)?;
                    if *target == 0 {
                        self.buffers
                            .get_mut(0)
                            .expect("display buffer is permanent")
                            .clear([0, 0, 0, 0]);
                    } else {
                        self.buffers.insert(*target as usize, blank)?;
                    }
                    return Ok(());
                }
                // AVG32 reserves `?` and `*` for the working PDT buffer
                // rather than a filename.  `SnrPDT_LoadBaseFile` leaves PDT1
                // untouched for these names, then runs the transition from
                // that buffer into the requested destination.
                if matches!(name.as_str(), "?" | "*") {
                    let source =
                        self.buffers.get(1).cloned().ok_or_else(|| {
                            anyhow::anyhow!("AVG32 working buffer 1 is out of range")
                        })?;
                    if *target == 0 {
                        let display = self
                            .buffers
                            .get_mut(0)
                            .expect("display buffer is permanent");
                        display.clear([0, 0, 0, 0]);
                        if let Some(effect) = direct_effect {
                            display.blit(
                                &source,
                                effect.source_rect[0],
                                effect.source_rect[1],
                                effect.source_rect[2] - effect.source_rect[0] + 1,
                                effect.source_rect[3] - effect.source_rect[1] + 1,
                                effect.destination[0],
                                effect.destination[1],
                            );
                        } else {
                            display.blit_full(&source, 0, 0);
                        }
                    } else {
                        self.buffers.insert(*target as usize, source)?;
                    }
                    return Ok(());
                }
                let image = decode_pdt(&resources.read("PDT", name)?)
                    .with_context(|| format!("failed to decode AVG32 PDT {name}"))?;
                let surface = Surface::from_pdt(image);
                if *target == 0 {
                    let display = self
                        .buffers
                        .get_mut(0)
                        .expect("display buffer is permanent");
                    if let Some(effect) = direct_effect
                        .as_ref()
                        .map(|effect| (effect.source_rect, effect.destination))
                        .or_else(|| {
                            effect
                                .and_then(|index| config.effect(index.max(0) as usize))
                                .map(|effect| (effect.source_rect, effect.destination))
                        })
                    {
                        display.blit(
                            &surface,
                            effect.0[0],
                            effect.0[1],
                            effect.0[2] - effect.0[0] + 1,
                            effect.0[3] - effect.0[1] + 1,
                            effect.1[0],
                            effect.1[1],
                        );
                    } else {
                        display.clear([0, 0, 0, 0]);
                        display.blit_full(&surface, 0, 0);
                    }
                } else {
                    self.buffers.insert(*target as usize, surface)?;
                }
            }
            VmAction::CompositeGraphic {
                base,
                effect: composite_effect,
                layers,
            } => {
                // AVG32's multi-PDT overlay commands always assemble into the
                // fixed working buffer (PDT#1); the value that follows the
                // base is a `GAMEEXE.INI` effect-table index used to present
                // PDT1 onto the display afterwards, not a script-chosen
                // destination buffer.
                let mut output = match base {
                    GraphicSource::File(name) if matches!(name.as_str(), "?" | "*") => {
                        self.buffers.get(1).cloned().ok_or_else(|| {
                            anyhow::anyhow!("AVG32 working buffer 1 is out of range")
                        })?
                    }
                    GraphicSource::File(name) => {
                        Surface::from_pdt(decode_pdt(&resources.read("PDT", name)?).with_context(
                            || format!("failed to decode AVG32 composite base {name}"),
                        )?)
                    }
                    GraphicSource::Buffer(index) => {
                        self.buffers.get(*index as usize).cloned().ok_or_else(|| {
                            anyhow::anyhow!("AVG32 composite buffer {index} is out of range")
                        })?
                    }
                };
                let mut last_name = match base {
                    GraphicSource::File(name) => name.as_str(),
                    GraphicSource::Buffer(_) => "",
                };
                for layer in layers {
                    let layer_name = if layer.name.is_empty() {
                        if last_name.is_empty() {
                            continue;
                        }
                        last_name
                    } else {
                        last_name = layer.name.as_str();
                        last_name
                    };
                    let mut source = if matches!(layer_name, "?" | "*") {
                        self.buffers.get(1).cloned().ok_or_else(|| {
                            anyhow::anyhow!("AVG32 working buffer 1 is out of range")
                        })?
                    } else {
                        Surface::from_pdt(decode_pdt(&resources.read("PDT", layer_name)?)?)
                    };
                    if let Some(alpha) = layer.alpha {
                        source.multiply_alpha(alpha.clamp(0, 255) as u8);
                    }
                    let (source_rect, destination) = if let Some(effect) = layer.effect {
                        config
                            .effect(effect.max(0) as usize)
                            .map(|effect| {
                                (
                                    effect.source_rect,
                                    [effect.destination[0], effect.destination[1]],
                                )
                            })
                            .unwrap_or((
                                [0, 0, source.width() as i32 - 1, source.height() as i32 - 1],
                                [0, 0],
                            ))
                    } else {
                        (
                            layer.source_rect.unwrap_or([
                                0,
                                0,
                                source.width() as i32 - 1,
                                source.height() as i32 - 1,
                            ]),
                            layer.destination.unwrap_or([0, 0]),
                        )
                    };
                    output.blit(
                        &source,
                        source_rect[0],
                        source_rect[1],
                        source_rect[2] - source_rect[0] + 1,
                        source_rect[3] - source_rect[1] + 1,
                        destination[0],
                        destination[1],
                    );
                }
                self.buffers.insert(1, output.clone())?;
                let display = self
                    .buffers
                    .get_mut(0)
                    .expect("display buffer is permanent");
                if let Some(effect) =
                    composite_effect.and_then(|index| config.effect(index.max(0) as usize))
                {
                    display.blit(
                        &output,
                        effect.source_rect[0],
                        effect.source_rect[1],
                        effect.source_rect[2] - effect.source_rect[0] + 1,
                        effect.source_rect[3] - effect.source_rect[1] + 1,
                        effect.destination[0],
                        effect.destination[1],
                    );
                } else {
                    display.clear([0, 0, 0, 0]);
                    display.blit_full(&output, 0, 0);
                }
            }
            VmAction::ClearGraphicBuffers => self.buffers.clear_non_display(),
            VmAction::BufferSwap {
                source,
                destination,
                source_rect,
                destination_xy,
            } => self.buffers.swap_rgb(
                *source as usize,
                *destination as usize,
                *source_rect,
                *destination_xy,
            )?,
            VmAction::BufferCopy {
                source,
                destination,
                source_rect,
                destination_xy,
                masked,
                color_key,
            } => {
                let source = self
                    .buffers
                    .get(*source as usize)
                    .ok_or_else(|| anyhow::anyhow!("AVG32 source buffer {source} is out of range"))?
                    .clone();
                let destination = self.buffers.get_mut(*destination as usize).ok_or_else(|| {
                    anyhow::anyhow!("AVG32 destination buffer {destination} is out of range")
                })?;
                let width = source_rect[2] - source_rect[0] + 1;
                let height = source_rect[3] - source_rect[1] + 1;
                if let Some(color_key) = color_key {
                    destination.copy_rect_excluding_color(
                        &source,
                        source_rect[0],
                        source_rect[1],
                        width,
                        height,
                        destination_xy[0],
                        destination_xy[1],
                        [color_key[0] as u8, color_key[1] as u8, color_key[2] as u8],
                    );
                } else if *masked {
                    destination.blit(
                        &source,
                        source_rect[0],
                        source_rect[1],
                        width,
                        height,
                        destination_xy[0],
                        destination_xy[1],
                    );
                } else {
                    destination.copy_rect(
                        &source,
                        source_rect[0],
                        source_rect[1],
                        width,
                        height,
                        destination_xy[0],
                        destination_xy[1],
                    );
                }
            }
            VmAction::BufferDigits {
                value,
                source,
                destination,
                glyph_origin,
                glyph_size,
                glyph_stride,
                destination_origin,
                destination_stride,
                count,
                zero_padded,
                masked,
                color_key,
            } => {
                let source = self
                    .buffers
                    .get(*source as usize)
                    .ok_or_else(|| {
                        anyhow::anyhow!("AVG32 digit source buffer {source} is out of range")
                    })?
                    .clone();
                let destination = self.buffers.get_mut(*destination as usize).ok_or_else(|| {
                    anyhow::anyhow!("AVG32 digit destination buffer {destination} is out of range")
                })?;
                let mut remaining = *value;
                // The native loop decrements before drawing, so a non-positive
                // field count is a no-op and the rightmost cell gets units.
                for cell in (0..(*count).max(0)).rev() {
                    let digit = remaining.rem_euclid(10);
                    let source_x = glyph_origin[0] + glyph_stride[0] * digit;
                    let source_y = glyph_origin[1] + glyph_stride[1] * digit;
                    let destination_x = destination_origin[0] + destination_stride[0] * cell;
                    let destination_y = destination_origin[1] + destination_stride[1] * cell;
                    if let Some(color_key) = color_key {
                        destination.copy_rect_excluding_color(
                            &source,
                            source_x,
                            source_y,
                            glyph_size[0],
                            glyph_size[1],
                            destination_x,
                            destination_y,
                            [color_key[0] as u8, color_key[1] as u8, color_key[2] as u8],
                        );
                    } else if *masked {
                        destination.blit(
                            &source,
                            source_x,
                            source_y,
                            glyph_size[0],
                            glyph_size[1],
                            destination_x,
                            destination_y,
                        );
                    } else {
                        destination.copy_rect(
                            &source,
                            source_x,
                            source_y,
                            glyph_size[0],
                            glyph_size[1],
                            destination_x,
                            destination_y,
                        );
                    }
                    remaining /= 10;
                    if remaining == 0 && !zero_padded {
                        break;
                    }
                }
            }
            VmAction::BufferFill {
                buffer,
                rect,
                color,
            } => self
                .buffers
                .get_mut(*buffer as usize)
                .ok_or_else(|| anyhow::anyhow!("AVG32 fill buffer {buffer} is out of range"))?
                .fill_rect(*rect, [color[0] as u8, color[1] as u8, color[2] as u8, 255]),
            VmAction::BufferOutline {
                buffer,
                rect,
                color,
            } => self
                .buffers
                .get_mut(*buffer as usize)
                .ok_or_else(|| anyhow::anyhow!("AVG32 outline buffer {buffer} is out of range"))?
                .outline_rect(*rect, [color[0] as u8, color[1] as u8, color[2] as u8]),
            VmAction::BufferInvert { buffer, rect } => self
                .buffers
                .get_mut(*buffer as usize)
                .ok_or_else(|| anyhow::anyhow!("AVG32 invert buffer {buffer} is out of range"))?
                .invert_rect(*rect),
            VmAction::BufferColorMask {
                buffer,
                rect,
                color,
            } => self
                .buffers
                .get_mut(*buffer as usize)
                .ok_or_else(|| {
                    anyhow::anyhow!("AVG32 colour-mask buffer {buffer} is out of range")
                })?
                .color_mask_rect(*rect, [color[0] as u8, color[1] as u8, color[2] as u8]),
            VmAction::BufferFade {
                buffer,
                rect,
                color,
                amount,
            } => self
                .buffers
                .get_mut(*buffer as usize)
                .ok_or_else(|| anyhow::anyhow!("AVG32 fade buffer {buffer} is out of range"))?
                .fade_rect(
                    *rect,
                    [color[0] as u8, color[1] as u8, color[2] as u8],
                    (*amount).clamp(0, 255) as u8,
                ),
            VmAction::BufferMonochrome { buffer, rect } => self
                .buffers
                .get_mut(*buffer as usize)
                .ok_or_else(|| anyhow::anyhow!("AVG32 monochrome buffer {buffer} is out of range"))?
                .monochrome_rect(*rect),
            VmAction::BufferStretchCopy {
                source,
                destination,
                source_rect,
                destination_rect,
            } => {
                let source = self.buffers.get(*source as usize).cloned().ok_or_else(|| {
                    anyhow::anyhow!("AVG32 stretch source buffer {source} is out of range")
                })?;
                self.buffers
                    .get_mut(*destination as usize)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "AVG32 stretch destination buffer {destination} is out of range"
                        )
                    })?
                    .stretch_copy(&source, *source_rect, *destination_rect);
            }
            // The runtime schedules individual `0x64:32` frames from an
            // immutable source snapshot.  Keeping this arm side-effect free
            // prevents a VM-only frontend from accidentally sampling a frame
            // that was already overwritten by the destination surface.
            VmAction::BufferStretchTween { .. } => {}
            VmAction::BufferScroll {
                source,
                destination,
                rect,
                amount: _,
                down: _,
            } => {
                let source = self.buffers.get(*source as usize).cloned().ok_or_else(|| {
                    anyhow::anyhow!("AVG32 scroll source buffer {source} is out of range")
                })?;
                self.buffers
                    .get_mut(*destination as usize)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "AVG32 scroll destination buffer {destination} is out of range"
                        )
                    })?
                    .copy_rect(
                        &source,
                        rect[0],
                        rect[1],
                        rect[2] - rect[0] + 1,
                        rect[3] - rect[1] + 1,
                        rect[0],
                        rect[1],
                    );
            }
            // Handled by `runtime.rs`'s `FadePlayer`, which drives the real
            // 16-phase ordered-dither transition over time via
            // `fade_display_phase` instead of snapping instantly.
            VmAction::Fade { .. } => {}
            _ => {}
        }
        Ok(())
    }

    pub fn save_display(&mut self) {
        self.backup = Some(self.display().clone());
    }

    pub fn restore_display(&mut self) {
        if let Some(backup) = &self.backup {
            self.buffers
                .insert(0, backup.clone())
                .expect("display buffer is valid");
        }
    }

    /// Paints one of AVG32's 4×4 ordered-dither fade passes.  `0x13` uses
    /// the same phase table as `PDTMGR::ScreenFade`, so every pixel is touched
    /// exactly once across the sixteen normal phases.
    pub(crate) fn fade_display_phase(&mut self, color: [u8; 3], phase: usize, solid: bool) {
        const X: [i32; 16] = [0, 2, 0, 2, 1, 3, 1, 3, 0, 2, 0, 2, 1, 3, 1, 3];
        const Y: [i32; 16] = [0, 2, 2, 0, 1, 3, 3, 1, 1, 3, 3, 1, 0, 2, 2, 0];
        let display = self
            .buffers
            .get_mut(0)
            .expect("display buffer is permanent");
        if solid {
            display.clear([color[0], color[1], color[2], 255]);
            return;
        }
        let phase = phase.min(15);
        for y in (Y[phase]..AVG32_HEIGHT as i32).step_by(4) {
            for x in (X[phase]..AVG32_WIDTH as i32).step_by(4) {
                let at = (y as usize * AVG32_WIDTH as usize + x as usize) * 4;
                display.pixels_mut()[at..at + 4]
                    .copy_from_slice(&[color[0], color[1], color[2], 255]);
            }
        }
    }

    pub fn clear_display(&mut self, rgba: [u8; 4]) {
        self.buffers
            .get_mut(0)
            .expect("display buffer is permanent")
            .clear(rgba);
    }

    pub fn blit_to_display(
        &mut self,
        source: &Surface,
        source_rect: [i32; 4],
        destination: [i32; 2],
    ) {
        self.buffers
            .get_mut(0)
            .expect("display buffer is permanent")
            .blit(
                source,
                source_rect[0],
                source_rect[1],
                source_rect[2] - source_rect[0] + 1,
                source_rect[3] - source_rect[1] + 1,
                destination[0],
                destination[1],
            );
    }

    pub const fn display_size() -> (u32, u32) {
        (AVG32_WIDTH, AVG32_HEIGHT)
    }
}
