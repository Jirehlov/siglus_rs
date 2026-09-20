//! CPU renderer used until the engine's wgpu renderer is split into a GLES2
//! backend.  Switch homebrew does not expose a wgpu-capable graphics API, so
//! this deliberately keeps the first frontend independent of WGSL and GPU
//! compute shaders.

use nx::gpu::canvas::{AlphaBlend, Canvas, RGBA8};

pub const LOGICAL_WIDTH: u32 = 854;
pub const LOGICAL_HEIGHT: u32 = 480;

#[derive(Clone, Copy)]
pub enum BlendMode {
    Alpha,
    Additive,
}

/// A small, nearest-neighbour textured-quad implementation.  It is the
/// software equivalent of the sprite shader's texture sample + tint stage and
/// gives the Switch frontend one renderer contract that can later be replaced
/// by GLES2 without changing its callers.
pub fn draw_textured_quad<C: Canvas<ColorFormat = RGBA8>>(
    canvas: &mut C,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    texture: &[[u8; 4]],
    texture_width: u32,
    texture_height: u32,
    tint: [u8; 4],
    blend: BlendMode,
) {
    if texture_width == 0
        || texture_height == 0
        || texture.len() != (texture_width * texture_height) as usize
    {
        return;
    }

    for dy in 0..height {
        let ty = (dy * texture_height / height) as usize;
        for dx in 0..width {
            let tx = (dx * texture_width / width) as usize;
            let sample = texture[ty * texture_width as usize + tx];
            let shaded = shade(sample, tint, blend);
            if shaded[3] == 0 {
                continue;
            }
            canvas.draw_single(
                x.saturating_add(dx as i32),
                y.saturating_add(dy as i32),
                RGBA8::new_scaled(shaded[0], shaded[1], shaded[2], shaded[3]),
                match blend {
                    BlendMode::Alpha => AlphaBlend::Source,
                    BlendMode::Additive => AlphaBlend::Destination,
                },
            );
        }
    }
}

fn shade(sample: [u8; 4], tint: [u8; 4], blend: BlendMode) -> [u8; 4] {
    let mut out = [
        ((sample[0] as u16 * tint[0] as u16) / 255) as u8,
        ((sample[1] as u16 * tint[1] as u16) / 255) as u8,
        ((sample[2] as u16 * tint[2] as u16) / 255) as u8,
        ((sample[3] as u16 * tint[3] as u16) / 255) as u8,
    ];
    if matches!(blend, BlendMode::Additive) {
        out[3] /= 2;
    }
    out
}

pub fn draw_background<C: Canvas<ColorFormat = RGBA8>>(canvas: &mut C, frame: u32) {
    for y in 0..LOGICAL_HEIGHT {
        let wave = ((frame / 4 + y / 3) & 31) as u8;
        canvas.draw_rect(
            0,
            y as i32,
            LOGICAL_WIDTH,
            1,
            RGBA8::new_scaled(10 + wave / 3, 14 + wave / 2, 35 + wave, 255),
            AlphaBlend::None,
        );
    }
}
