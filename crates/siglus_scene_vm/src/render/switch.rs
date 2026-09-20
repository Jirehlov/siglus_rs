//! Nintendo Switch renderer backend.
//!
//! This module is selected only for Rust's built-in
//! `aarch64-nintendo-switch-freestanding` target (`target_os = "horizon"`).
//! The desktop WGPU renderer remains untouched in `render/mod.rs`; the shared
//! VM submits the same `RenderFrame` to this backend through `switch_host`.

use anyhow::Result;

use std::collections::HashMap;
use std::sync::Arc;

use crate::assets::{RgbaImage, load_image_any};
use crate::emote::EmoteRenderPacket;
use crate::image_manager::ImageManager;
use crate::layer::{RenderFrame, Sprite, SpriteBlend, SpriteFit, SpriteSizeMode};
use crate::mesh3d::{MeshAsset, MeshGpuPrimitiveBatch, MeshTriVertex, load_mesh_asset};
use crate::render_math::{ProjectedPoint, project_model_point, sprite_quad_points_rect};
use crate::runtime::FrameCaptureBackend;

unsafe extern "C" {
    fn siglus_switch_present_rgba(pixels: *const u8, width: u32, height: u32, wait_vsync: bool);
}

/// Native deko3d renderer state.  The handle is owned by the Horizon host and
/// deliberately contains no desktop window or WGPU object.
pub struct Renderer {
    width: u32,
    height: u32,
    wait_display_vsync: bool,
    framebuffer: Vec<u8>,
    mesh_depth: Vec<f32>,
    mesh_assets: HashMap<String, Arc<MeshAsset>>,
    mesh_textures: HashMap<std::path::PathBuf, Arc<RgbaImage>>,
}

impl Renderer {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        let width = width.max(1);
        let height = height.max(1);
        let pixel_count = width as usize * height as usize;
        crate::switch_host::report_switch_marker(
            b"siglus_switch: renderer rgba allocation begin\n\0",
        );
        let framebuffer = vec![0; pixel_count * 4];
        crate::switch_host::report_switch_marker(
            b"siglus_switch: renderer rgba allocation complete\n\0",
        );
        crate::switch_host::report_switch_marker(
            b"siglus_switch: renderer depth allocation begin\n\0",
        );
        let mesh_depth = vec![f32::INFINITY; pixel_count];
        crate::switch_host::report_switch_marker(
            b"siglus_switch: renderer depth allocation complete\n\0",
        );
        crate::switch_host::report_switch_marker(
            b"siglus_switch: renderer mesh-assets map begin\n\0",
        );
        let mesh_assets = HashMap::new();
        crate::switch_host::report_switch_marker(
            b"siglus_switch: renderer mesh-assets map complete\n\0",
        );
        crate::switch_host::report_switch_marker(
            b"siglus_switch: renderer mesh-textures map begin\n\0",
        );
        let mesh_textures = HashMap::new();
        crate::switch_host::report_switch_marker(
            b"siglus_switch: renderer mesh-textures map complete\n\0",
        );
        let renderer = Self {
            width,
            height,
            wait_display_vsync: true,
            framebuffer,
            mesh_depth,
            mesh_assets,
            mesh_textures,
        };
        crate::switch_host::report_switch_marker(b"siglus_switch: renderer object constructed\n\0");
        Ok(renderer)
    }

    pub fn adapter_name(&self) -> String {
        "Nintendo Switch native renderer".to_owned()
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.framebuffer
            .resize(self.width as usize * self.height as usize * 4, 0);
        self.mesh_depth
            .resize(self.width as usize * self.height as usize, f32::INFINITY);
    }

    pub fn resize_with_scale(&mut self, width: u32, height: u32, _scale_factor: f32) {
        self.resize(width, height);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn resize_with_logical_viewport(
        &mut self,
        _surface_width: u32,
        _surface_height: u32,
        _scale_factor: f32,
        logical_width: u32,
        logical_height: u32,
        _viewport_x: u32,
        _viewport_y: u32,
        _viewport_width: u32,
        _viewport_height: u32,
    ) {
        self.resize(logical_width, logical_height);
    }

    pub fn set_wait_display_vsync(&mut self, wait_display_vsync: bool) {
        self.wait_display_vsync = wait_display_vsync;
    }

    pub fn clear_runtime_image_textures(&mut self) {}

    pub fn logical_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Compose the engine's existing painter-ordered sprite list into RGBA8,
    /// then hand it to the native deko3d presentation pass. This intentionally
    /// consumes `RenderFrame` and `ImageManager` directly: scene/VM/resource
    /// code remains identical to desktop.
    pub fn render_frame(&mut self, images: &ImageManager, frame: &RenderFrame) -> Result<()> {
        self.framebuffer.fill(0);
        for item in frame.debug_flatten() {
            let sprite = &item.sprite;
            if !sprite.visible {
                continue;
            }
            if sprite.mesh_kind != 0 && sprite.mesh_file_name.is_some() {
                self.blit_mesh(images, sprite);
                continue;
            }
            if let Some(packet) = sprite.emote_render.as_deref() {
                let image = compose_emote(packet);
                self.blit(images, sprite, &image);
                continue;
            }
            let Some(handle) = sprite.image_id.as_ref() else {
                continue;
            };
            let Some(image) = images.get(handle) else {
                continue;
            };
            self.blit(images, sprite, &image);
        }
        unsafe {
            siglus_switch_present_rgba(
                self.framebuffer.as_ptr(),
                self.width,
                self.height,
                self.wait_display_vsync,
            )
        };
        Ok(())
    }

    /// Reuse the engine's parsed/animated mesh batches. This is a native
    /// backend implementation of the existing mesh object contract, not a
    /// second model format or scene graph.
    fn blit_mesh(&mut self, images: &ImageManager, sprite: &Sprite) {
        let Some(name) = sprite.mesh_file_name.as_deref() else {
            return;
        };
        let asset = if let Some(asset) = self.mesh_assets.get(name) {
            Arc::clone(asset)
        } else {
            let Ok(asset) =
                load_mesh_asset(images.project_dir(), images.current_append_dir(), name)
            else {
                return;
            };
            let asset = Arc::new(asset);
            self.mesh_assets.insert(name.to_owned(), Arc::clone(&asset));
            asset
        };
        self.mesh_depth.fill(f32::INFINITY);
        let mut mesh_sprite = sprite.clone();
        // `project_model_point` is the shared scene projection helper. Mesh
        // vertices are already in mesh-local coordinates, so mesh pivots do
        // not participate in its model transform (matching desktop).
        mesh_sprite.pivot_x = 0.0;
        mesh_sprite.pivot_y = 0.0;
        mesh_sprite.pivot_z = 0.0;
        for batch in asset.sample_gpu_primitives_with_state(&sprite.mesh_animation) {
            let owned_texture = batch.texture_path.as_ref().and_then(|path| {
                if !self.mesh_textures.contains_key(path) {
                    let image = load_image_any(path, 0).ok()?;
                    self.mesh_textures.insert(path.clone(), Arc::new(image));
                }
                self.mesh_textures.get(path).cloned()
            });
            let fallback_texture = sprite.image_id.as_ref().and_then(|id| images.get(id));
            let texture = owned_texture.as_deref().or(fallback_texture.as_deref());
            for triangle in batch.vertices.chunks_exact(3) {
                let Some(a) = mesh_project_vertex(
                    &mesh_sprite,
                    &batch,
                    &triangle[0],
                    self.width,
                    self.height,
                ) else {
                    continue;
                };
                let Some(b) = mesh_project_vertex(
                    &mesh_sprite,
                    &batch,
                    &triangle[1],
                    self.width,
                    self.height,
                ) else {
                    continue;
                };
                let Some(c) = mesh_project_vertex(
                    &mesh_sprite,
                    &batch,
                    &triangle[2],
                    self.width,
                    self.height,
                ) else {
                    continue;
                };
                self.raster_mesh_triangle(sprite, &batch, texture, a, b, c);
            }
        }
    }

    fn raster_mesh_triangle(
        &mut self,
        sprite: &Sprite,
        batch: &MeshGpuPrimitiveBatch,
        texture: Option<&RgbaImage>,
        a: MeshProjectedVertex,
        b: MeshProjectedVertex,
        c: MeshProjectedVertex,
    ) {
        let area = edge(a.point, b.point, c.point.x, c.point.y);
        if area.abs() <= f32::EPSILON {
            return;
        }
        if sprite.culling && area <= 0.0 {
            return;
        }
        let left = a.point.x.min(b.point.x).min(c.point.x).floor().max(0.0) as i32;
        let top = a.point.y.min(b.point.y).min(c.point.y).floor().max(0.0) as i32;
        let right = a
            .point
            .x
            .max(b.point.x)
            .max(c.point.x)
            .ceil()
            .min(self.width as f32) as i32;
        let bottom = a
            .point
            .y
            .max(b.point.y)
            .max(c.point.y)
            .ceil()
            .min(self.height as f32) as i32;
        let material = &batch.material;
        for y in top..bottom {
            for x in left..right {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let wa = edge(b.point, c.point, px, py) / area;
                let wb = edge(c.point, a.point, px, py) / area;
                let wc = 1.0 - wa - wb;
                if wa < 0.0 || wb < 0.0 || wc < 0.0 {
                    continue;
                }
                let depth = wa * a.point.depth + wb * b.point.depth + wc * c.point.depth;
                let index = y as usize * self.width as usize + x as usize;
                if depth >= self.mesh_depth[index] {
                    continue;
                }
                self.mesh_depth[index] = depth;
                let (pwa, pwb, pwc) = perspective_weights(a.point, b.point, c.point, wa, wb, wc);
                let u = pwa * a.uv[0] + pwb * b.uv[0] + pwc * c.uv[0];
                let v = pwa * a.uv[1] + pwb * b.uv[1] + pwc * c.uv[1];
                let mut color = texture.map_or([1.0, 1.0, 1.0, 1.0], |image| {
                    sample_rgba(image, u, v, false)
                });
                for channel in 0..3 {
                    let vertex =
                        pwa * a.color[channel] + pwb * b.color[channel] + pwc * c.color[channel];
                    color[channel] *= material.diffuse[channel] * vertex;
                }
                color[3] *= material.diffuse[3]
                    * (pwa * a.color[3] + pwb * b.color[3] + pwc * c.color[3])
                    * sprite.alpha as f32
                    / 255.0;
                if material.alpha_test_enable && color[3] < material.alpha_ref.max(1.0 / 255.0) {
                    continue;
                }
                let di = index * 4;
                blend_pixel(&mut self.framebuffer[di..di + 4], color, sprite.blend);
            }
        }
    }

    fn blit(&mut self, images: &ImageManager, sprite: &Sprite, image: &RgbaImage) {
        if image.width == 0
            || image.height == 0
            || image.rgba.len() < image.width as usize * image.height as usize * 4
        {
            return;
        }
        // Keep the exact local-coordinate convention used by the desktop
        // renderer: object SRC_CLIP is center-relative, while the final
        // texture coordinates are 0-based in the source image.
        let (dst_x, dst_y, local_left, local_top, local_right, local_bottom, u0, v0, u1, v1) =
            match sprite.fit {
                SpriteFit::FullScreen => {
                    let Some((left, top, right, bottom)) =
                        clipped_rect(sprite.src_clip, image.width as f32, image.height as f32)
                    else {
                        return;
                    };
                    (
                        0.0,
                        0.0,
                        0.0,
                        0.0,
                        self.width as f32,
                        self.height as f32,
                        left / image.width as f32,
                        top / image.height as f32,
                        right / image.width as f32,
                        bottom / image.height as f32,
                    )
                }
                SpriteFit::PixelRect => {
                    let (logical_w, logical_h) = match sprite.size_mode {
                        SpriteSizeMode::Intrinsic => {
                            (image.width.max(1) as f32, image.height.max(1) as f32)
                        }
                        SpriteSizeMode::Explicit { width, height } => {
                            (width.max(1) as f32, height.max(1) as f32)
                        }
                    };
                    let center_x = sprite.pivot_x
                        + if sprite.object_anchor {
                            sprite.texture_center_x
                        } else {
                            0.0
                        };
                    let center_y = sprite.pivot_y
                        + if sprite.object_anchor {
                            sprite.texture_center_y
                        } else {
                            0.0
                        };
                    let Some((left, top, right, bottom)) = clipped_rect_centered(
                        sprite.src_clip,
                        logical_w,
                        logical_h,
                        center_x,
                        center_y,
                    ) else {
                        return;
                    };
                    (
                        sprite.x as f32,
                        sprite.y as f32,
                        left,
                        top,
                        right,
                        bottom,
                        left / logical_w,
                        top / logical_h,
                        right / logical_w,
                        bottom / logical_h,
                    )
                }
            };
        let Some(points) = sprite_quad_points_rect(
            sprite,
            dst_x,
            dst_y,
            local_left,
            local_top,
            local_right,
            local_bottom,
            self.width as f32,
            self.height as f32,
        ) else {
            return;
        };
        let dst_clip = sprite.dst_clip.unwrap_or(crate::layer::ClipRect {
            left: 0,
            top: 0,
            right: self.width as i32,
            bottom: self.height as i32,
        });
        let mask = sprite.mask_image_id.as_ref().and_then(|id| images.get(id));
        let tonecurve = sprite
            .tonecurve_image_id
            .as_ref()
            .and_then(|id| images.get(id));
        let wipe_source = sprite
            .wipe_src_image_id
            .as_ref()
            .and_then(|id| images.get(id));
        if self.raster_axis_aligned(
            sprite,
            image,
            mask.as_deref(),
            tonecurve.as_deref(),
            wipe_source.as_deref(),
            dst_clip,
            points,
            u0,
            v0,
            u1,
            v1,
        ) {
            return;
        }
        self.raster_triangle(
            sprite,
            image,
            mask.as_deref(),
            tonecurve.as_deref(),
            wipe_source.as_deref(),
            dst_clip,
            points[0],
            points[1],
            points[2],
            [u0, v0],
            [u1, v0],
            [u1, v1],
        );
        self.raster_triangle(
            sprite,
            image,
            mask.as_deref(),
            tonecurve.as_deref(),
            wipe_source.as_deref(),
            dst_clip,
            points[0],
            points[2],
            points[3],
            [u0, v0],
            [u1, v1],
            [u0, v1],
        );
    }

    /// The overwhelming majority of visual-novel sprites are unrotated quads.
    /// Drawing those as two generic perspective triangles performs the edge and
    /// reciprocal calculations twice for every covered pixel, which is
    /// prohibitively expensive on Horizon (and under CPU emulation). Keep the
    /// generic path for transformed sprites, but scan axis-aligned quads once.
    #[allow(clippy::too_many_arguments)]
    fn raster_axis_aligned(
        &mut self,
        sprite: &Sprite,
        image: &RgbaImage,
        mask: Option<&RgbaImage>,
        tonecurve: Option<&RgbaImage>,
        wipe_source: Option<&RgbaImage>,
        clip: crate::layer::ClipRect,
        points: [ProjectedPoint; 4],
        u0: f32,
        v0: f32,
        u1: f32,
        v1: f32,
    ) -> bool {
        const EPSILON: f32 = 0.01;
        let [a, b, c, d] = points;
        if (a.y - b.y).abs() > EPSILON
            || (b.x - c.x).abs() > EPSILON
            || (c.y - d.y).abs() > EPSILON
            || (d.x - a.x).abs() > EPSILON
            || (a.clip_w - b.clip_w).abs() > EPSILON
            || (a.clip_w - c.clip_w).abs() > EPSILON
            || (a.clip_w - d.clip_w).abs() > EPSILON
        {
            return false;
        }
        let dx = b.x - a.x;
        let dy = d.y - a.y;
        if dx.abs() <= f32::EPSILON || dy.abs() <= f32::EPSILON {
            return true;
        }
        let left = a.x.min(c.x).floor().max(clip.left as f32).max(0.0) as i32;
        let top = a.y.min(c.y).floor().max(clip.top as f32).max(0.0) as i32;
        let right = a
            .x
            .max(c.x)
            .ceil()
            .min(clip.right as f32)
            .min(self.width as f32) as i32;
        let bottom = a
            .y
            .max(c.y)
            .ceil()
            .min(clip.bottom as f32)
            .min(self.height as f32) as i32;
        if sprite_has_simple_rgba_effects(sprite, mask, tonecurve, wipe_source) {
            self.raster_axis_aligned_rgba8(
                sprite, image, a, dx, dy, left, top, right, bottom, u0, v0, u1, v1,
            );
            return true;
        }
        for y in top..bottom {
            let ty = (y as f32 + 0.5 - a.y) / dy;
            let v = v0 + (v1 - v0) * ty;
            for x in left..right {
                let tx = (x as f32 + 0.5 - a.x) / dx;
                let u = u0 + (u1 - u0) * tx;
                let mut color = sample_sprite(image, wipe_source, sprite, u, v);
                apply_sprite_effects(&mut color, sprite, mask, tonecurve, x as f32, y as f32);
                if color[3] <= 0.0 {
                    continue;
                }
                let di = (y as usize * self.width as usize + x as usize) * 4;
                blend_pixel(&mut self.framebuffer[di..di + 4], color, sprite.blend);
            }
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn raster_axis_aligned_rgba8(
        &mut self,
        sprite: &Sprite,
        image: &RgbaImage,
        origin: ProjectedPoint,
        dx: f32,
        dy: f32,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        u0: f32,
        v0: f32,
        u1: f32,
        v1: f32,
    ) {
        if left >= right || top >= bottom {
            return;
        }
        const FP_SHIFT: i32 = 16;
        const FP_ONE: i64 = 1 << FP_SHIFT;
        const FP_HALF: i64 = FP_ONE / 2;
        let source_w = image.width as usize;
        let source_h = image.height as usize;
        let source_x_max = image.width.saturating_sub(1) as f64;
        let source_y_max = image.height.saturating_sub(1) as f64;
        let source_x_at = |x: f64| {
            (u0 as f64
                + (u1 as f64 - u0 as f64) * (x - origin.x as f64) / dx as f64)
                * source_x_max
        };
        let source_y_at = |y: f64| {
            (v0 as f64
                + (v1 as f64 - v0 as f64) * (y - origin.y as f64) / dy as f64)
                * source_y_max
        };
        let start_x_fp = (source_x_at(left as f64 + 0.5) * FP_ONE as f64).round() as i64;
        let start_y_fp = (source_y_at(top as f64 + 0.5) * FP_ONE as f64).round() as i64;
        let step_x_fp = (((u1 as f64 - u0 as f64) * source_x_max / dx as f64)
            * FP_ONE as f64)
            .round() as i64;
        let step_y_fp = (((v1 as f64 - v0 as f64) * source_y_max / dy as f64)
            * FP_ONE as f64)
            .round() as i64;
        let global_alpha = sprite.alpha as u32 * sprite.tr as u32;
        let target_width = self.width as usize;

        let mut sy_fp = start_y_fp;
        for y in top..bottom {
            let sy = ((sy_fp + FP_HALF) >> FP_SHIFT).clamp(0, source_h as i64 - 1) as usize;
            let source_row = sy * source_w * 4;
            let target_row = y as usize * target_width * 4;
            let mut sx_fp = start_x_fp;
            for x in left..right {
                let sx = ((sx_fp + FP_HALF) >> FP_SHIFT).clamp(0, source_w as i64 - 1) as usize;
                let si = source_row + sx * 4;
                let di = target_row + x as usize * 4;
                let source = &image.rgba[si..si + 4];
                let alpha = (source[3] as u32 * global_alpha + 32_512) / 65_025;
                if alpha >= 255 {
                    self.framebuffer[di..di + 3].copy_from_slice(&source[..3]);
                    self.framebuffer[di + 3] = 255;
                } else if alpha != 0 {
                    let inverse = 255 - alpha;
                    for channel in 0..3 {
                        self.framebuffer[di + channel] = ((source[channel] as u32 * alpha
                            + self.framebuffer[di + channel] as u32 * inverse
                            + 127)
                            / 255) as u8;
                    }
                    self.framebuffer[di + 3] = 255;
                }
                sx_fp += step_x_fp;
            }
            sy_fp += step_y_fp;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn raster_triangle(
        &mut self,
        sprite: &Sprite,
        image: &RgbaImage,
        mask: Option<&RgbaImage>,
        tonecurve: Option<&RgbaImage>,
        wipe_source: Option<&RgbaImage>,
        clip: crate::layer::ClipRect,
        a: ProjectedPoint,
        b: ProjectedPoint,
        c: ProjectedPoint,
        auv: [f32; 2],
        buv: [f32; 2],
        cuv: [f32; 2],
    ) {
        let area = edge(a, b, c.x, c.y);
        if area.abs() <= f32::EPSILON {
            return;
        }
        let left = a.x.min(b.x).min(c.x).floor() as i32;
        let top = a.y.min(b.y).min(c.y).floor() as i32;
        let right = a.x.max(b.x).max(c.x).ceil() as i32;
        let bottom = a.y.max(b.y).max(c.y).ceil() as i32;
        for y in top.max(clip.top).max(0)..bottom.min(clip.bottom).min(self.height as i32) {
            for x in left.max(clip.left).max(0)..right.min(clip.right).min(self.width as i32) {
                let px = x as f32 + 0.5;
                let py = y as f32 + 0.5;
                let wa = edge(b, c, px, py) / area;
                let wb = edge(c, a, px, py) / area;
                let wc = 1.0 - wa - wb;
                if wa < 0.0 || wb < 0.0 || wc < 0.0 {
                    continue;
                }
                let (pwa, pwb, pwc) = perspective_weights(a, b, c, wa, wb, wc);
                let u = pwa * auv[0] + pwb * buv[0] + pwc * cuv[0];
                let v = pwa * auv[1] + pwb * buv[1] + pwc * cuv[1];
                let mut color = sample_sprite(image, wipe_source, sprite, u, v);
                apply_sprite_effects(&mut color, sprite, mask, tonecurve, x as f32, y as f32);
                if color[3] <= 0.0 {
                    continue;
                }
                let di = (y as usize * self.width as usize + x as usize) * 4;
                blend_pixel(&mut self.framebuffer[di..di + 4], color, sprite.blend);
            }
        }
    }
}

fn sprite_has_simple_rgba_effects(
    sprite: &Sprite,
    mask: Option<&RgbaImage>,
    tonecurve: Option<&RgbaImage>,
    wipe_source: Option<&RgbaImage>,
) -> bool {
    sprite.blend == SpriteBlend::Normal
        && sprite.wipe_fx_mode == 0
        && wipe_source.is_none()
        && mask.is_none()
        && tonecurve.is_none()
        && sprite.mask_mode == 0
        && sprite.reverse == 0
        && sprite.mono == 0
        && sprite.bright == 0
        && sprite.dark == 0
        && sprite.color_rate == 0
        && sprite.color_add_r == 0
        && sprite.color_add_g == 0
        && sprite.color_add_b == 0
}

fn clipped_rect(
    clip: Option<crate::layer::ClipRect>,
    width: f32,
    height: f32,
) -> Option<(f32, f32, f32, f32)> {
    let (mut left, mut top, mut right, mut bottom) = (0.0f32, 0.0f32, width, height);
    if let Some(clip) = clip {
        left = (clip.left as f32).clamp(0.0, width);
        top = (clip.top as f32).clamp(0.0, height);
        right = (clip.right as f32).clamp(0.0, width);
        bottom = (clip.bottom as f32).clamp(0.0, height);
    }
    (right > left && bottom > top).then_some((left, top, right, bottom))
}

fn clipped_rect_centered(
    clip: Option<crate::layer::ClipRect>,
    width: f32,
    height: f32,
    center_x: f32,
    center_y: f32,
) -> Option<(f32, f32, f32, f32)> {
    let (mut left, mut top, mut right, mut bottom) = (0.0f32, 0.0f32, width, height);
    if let Some(clip) = clip {
        left = left.max(clip.left as f32 + center_x);
        top = top.max(clip.top as f32 + center_y);
        right = right.min(clip.right as f32 + center_x);
        bottom = bottom.min(clip.bottom as f32 + center_y);
    }
    (right > left && bottom > top).then_some((left, top, right, bottom))
}

fn edge(a: ProjectedPoint, b: ProjectedPoint, x: f32, y: f32) -> f32 {
    (x - a.x) * (b.y - a.y) - (y - a.y) * (b.x - a.x)
}

/// Converts screen-space barycentric coordinates into perspective-correct
/// weights for values that originated before the perspective divide.  The
/// desktop renderer obtains this from the GPU; Switch's software compositor
/// must apply the equivalent correction explicitly.
fn perspective_weights(
    a: ProjectedPoint,
    b: ProjectedPoint,
    c: ProjectedPoint,
    wa: f32,
    wb: f32,
    wc: f32,
) -> (f32, f32, f32) {
    fn reciprocal_clip_w(point: ProjectedPoint) -> f32 {
        (point.clip_w.is_finite() && point.clip_w.abs() > f32::EPSILON)
            .then(|| point.clip_w.recip())
            .unwrap_or(1.0)
    }

    let aw = wa * reciprocal_clip_w(a);
    let bw = wb * reciprocal_clip_w(b);
    let cw = wc * reciprocal_clip_w(c);
    let sum = aw + bw + cw;
    if sum.is_finite() && sum.abs() > f32::EPSILON {
        (aw / sum, bw / sum, cw / sum)
    } else {
        (wa, wb, wc)
    }
}

#[derive(Clone, Copy)]
struct MeshProjectedVertex {
    point: ProjectedPoint,
    uv: [f32; 2],
    color: [f32; 4],
}

fn transform_mesh_col(cols: &[[f32; 4]; 4], p: [f32; 3]) -> [f32; 3] {
    [
        cols[0][0] * p[0] + cols[1][0] * p[1] + cols[2][0] * p[2] + cols[3][0],
        cols[0][1] * p[0] + cols[1][1] * p[1] + cols[2][1] * p[2] + cols[3][1],
        cols[0][2] * p[0] + cols[1][2] * p[1] + cols[2][2] * p[2] + cols[3][2],
    ]
}

fn mesh_project_vertex(
    sprite: &Sprite,
    batch: &MeshGpuPrimitiveBatch,
    vertex: &MeshTriVertex,
    width: u32,
    height: u32,
) -> Option<MeshProjectedVertex> {
    let position = if batch.skinned && !batch.bone_cols.is_empty() {
        let mut result = [0.0; 3];
        let mut weight_sum = 0.0;
        for slot in 0..4 {
            let weight = vertex.bone_weights[slot];
            let bone = vertex.bone_indices[slot] as usize;
            if weight <= 0.0 || bone >= batch.bone_cols.len() {
                continue;
            }
            let transformed = transform_mesh_col(&batch.bone_cols[bone], vertex.pos);
            for axis in 0..3 {
                result[axis] += transformed[axis] * weight;
            }
            weight_sum += weight;
        }
        if weight_sum > 0.0 {
            result
        } else {
            transform_mesh_col(&batch.frame_cols, vertex.pos)
        }
    } else {
        transform_mesh_col(&batch.frame_cols, vertex.pos)
    };
    let point = project_model_point(
        sprite,
        position,
        sprite.x as f32,
        sprite.y as f32,
        width as f32,
        height as f32,
    )?;
    Some(MeshProjectedVertex {
        point,
        uv: vertex.uv,
        color: vertex.color,
    })
}

fn sample_rgba(image: &RgbaImage, u: f32, v: f32, clamp_to_edge: bool) -> [f32; 4] {
    sample_rgba_pixels(
        image.width,
        image.height,
        image.rgba.as_slice(),
        u,
        v,
        clamp_to_edge,
    )
}

fn sample_rgba_pixels(
    width: u32,
    height: u32,
    rgba: &[u8],
    u: f32,
    v: f32,
    clamp_to_edge: bool,
) -> [f32; 4] {
    let u = if clamp_to_edge { u.clamp(0.0, 1.0) } else { u };
    let v = if clamp_to_edge { v.clamp(0.0, 1.0) } else { v };
    if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
        return [0.0; 4];
    }
    let x = (u * width.saturating_sub(1) as f32).round() as usize;
    let y = (v * height.saturating_sub(1) as f32).round() as usize;
    let i = (y * width as usize + x) * 4;
    if i + 3 >= rgba.len() {
        return [0.0; 4];
    }
    [
        rgba[i] as f32 / 255.0,
        rgba[i + 1] as f32 / 255.0,
        rgba[i + 2] as f32 / 255.0,
        rgba[i + 3] as f32 / 255.0,
    ]
}

fn sample_sprite(
    image: &RgbaImage,
    wipe_source: Option<&RgbaImage>,
    sprite: &Sprite,
    u: f32,
    v: f32,
) -> [f32; 4] {
    let p = sprite.wipe_fx_params;
    let sample = |u, v| sample_rgba(image, u, v, false);
    let mosaic = |img: &RgbaImage| {
        let cu = p[0].max(1e-5);
        let cv = (p[0] * p[1]).max(1e-5);
        sample_rgba(img, (u / cu).floor() * cu, (v / cv).floor() * cv, false)
    };
    let raster = |vertical: bool| {
        let fraction = p[0].max(1.0);
        let axis = if vertical { u } else { v };
        let local = (axis * fraction).fract();
        let phase = (local - fraction * 0.1) / fraction;
        let progress = p[3];
        let rp = (1.0 - progress).clamp(1e-4, 1.0);
        let amp = 1.0 - ((((1.0 - rp) * 100.0).max(1e-4).ln() / 10.0f32.ln()) + 1.0) / 3.0;
        let offset = (std::f32::consts::PI * progress * p[2] + phase * std::f32::consts::PI * p[1])
            .sin()
            * amp;
        if vertical {
            sample(u, v + offset)
        } else {
            sample(u + offset, v)
        }
    };
    let explosion = |img: &RgbaImage| {
        let dx = p[0] - u;
        let dy = p[1] - v;
        let len = (dx * dx + dy * dy).sqrt();
        if len <= 1e-5 || p[2] <= 1e-5 {
            return sample_rgba(img, u, v, false);
        }
        let texel = 1.0 / img.width.max(img.height).max(1) as f32;
        let scale = texel * p[2] * len * p[3].max(0.0) / len;
        let weights = [0.19, 0.17, 0.15, 0.13, 0.11, 0.09, 0.07, 0.05, 0.03, 0.01];
        let mut out = [0.0; 4];
        for (i, weight) in weights.iter().enumerate() {
            let c = sample_rgba(
                img,
                u + dx * scale * i as f32,
                v + dy * scale * i as f32,
                false,
            );
            for channel in 0..4 {
                out[channel] += c[channel] * weight;
            }
        }
        out
    };
    match sprite.wipe_fx_mode {
        1 => mosaic(image),
        2 => raster(false),
        3 => raster(true),
        4 => explosion(image),
        5 | 6 => {
            let mut color = sample(u, v);
            let brightness = color[0] * 0.2989 + color[1] * 0.5886 + color[2] * 0.1145;
            let matches = if sprite.wipe_fx_mode == 5 {
                brightness <= p[1]
            } else {
                brightness >= p[1]
            };
            if matches {
                color[3] *= p[0].max(0.0);
            }
            color
        }
        10..=13 => match wipe_source {
            None => sample(u, v),
            Some(old) => match sprite.wipe_fx_mode {
                10 => {
                    let oldc = mosaic(old);
                    let newc = mosaic(image);
                    if p[3] < 230.5 {
                        if p[2] >= 0.5 { newc } else { oldc }
                    } else {
                        mix_rgba(oldc, newc, p[2])
                    }
                }
                11 => mix_rgba(raster_from(old, u, v, p, false), raster(false), p[3]),
                12 => mix_rgba(raster_from(old, u, v, p, true), raster(true), p[3]),
                _ => mix_rgba(
                    explosion(old),
                    explosion(image),
                    sprite.tonecurve_row.clamp(0.0, 1.0),
                ),
            },
        },
        _ => sample(u, v),
    }
}

fn raster_from(image: &RgbaImage, u: f32, v: f32, p: [f32; 4], vertical: bool) -> [f32; 4] {
    let fraction = p[0].max(1.0);
    let axis = if vertical { u } else { v };
    let local = (axis * fraction).fract();
    let phase = (local - fraction * 0.1) / fraction;
    let rp = (1.0 - p[3]).clamp(1e-4, 1.0);
    let amp = 1.0 - ((((1.0 - rp) * 100.0).max(1e-4).ln() / 10.0f32.ln()) + 1.0) / 3.0;
    let offset =
        (std::f32::consts::PI * p[3] * p[2] + phase * std::f32::consts::PI * p[1]).sin() * amp;
    if vertical {
        sample_rgba(image, u, v + offset, false)
    } else {
        sample_rgba(image, u + offset, v, false)
    }
}

fn mix_rgba(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] * (1.0 - t) + b[0] * t,
        a[1] * (1.0 - t) + b[1] * t,
        a[2] * (1.0 - t) + b[2] * t,
        a[3] * (1.0 - t) + b[3] * t,
    ]
}

fn apply_sprite_effects(
    color: &mut [f32; 4],
    sprite: &Sprite,
    mask: Option<&RgbaImage>,
    tonecurve: Option<&RgbaImage>,
    x: f32,
    y: f32,
) {
    let original_alpha = color[3] * sprite.alpha as f32 / 255.0 * sprite.tr as f32 / 255.0;
    color[3] = original_alpha;
    let mono_before = color[0] * 0.2989 + color[1] * 0.5886 + color[2] * 0.1145;
    if let Some(tonecurve) = tonecurve {
        let sat = sprite.tonecurve_sat.clamp(0.0, 1.0);
        for channel in 0..3 {
            let source = color[channel] * (1.0 - sat) + mono_before * sat;
            color[channel] = sample_rgba(tonecurve, source, sprite.tonecurve_row, true)[0];
        }
    }
    let rev = sprite.reverse as f32 / 255.0;
    let mono = sprite.mono as f32 / 255.0;
    let bright = sprite.bright as f32 / 255.0;
    let dark = sprite.dark as f32 / 255.0;
    let color_rate = sprite.color_rate as f32 / 255.0;
    for channel in 0..3 {
        color[channel] = color[channel] * (1.0 - rev) + (1.0 - color[channel]) * rev;
        color[channel] = color[channel] * (1.0 - mono) + mono_before * mono;
        color[channel] = (color[channel] + bright - dark).clamp(0.0, 1.0);
    }
    let target = [
        sprite.color_r as f32 / 255.0,
        sprite.color_g as f32 / 255.0,
        sprite.color_b as f32 / 255.0,
    ];
    let add = [
        sprite.color_add_r as f32 / 255.0,
        sprite.color_add_g as f32 / 255.0,
        sprite.color_add_b as f32 / 255.0,
    ];
    for channel in 0..3 {
        color[channel] =
            (color[channel] * (1.0 - color_rate) + target[channel] * color_rate + add[channel])
                .clamp(0.0, 1.0);
    }
    if let Some(mask) = mask {
        let mu =
            (x - sprite.mask_offset_x as f32 + mask.center_x as f32) / mask.width.max(1) as f32;
        let mv =
            (y - sprite.mask_offset_y as f32 + mask.center_y as f32) / mask.height.max(1) as f32;
        let m = sample_rgba(mask, mu, mv, true);
        for channel in 0..4 {
            color[channel] *= m[channel];
        }
    }
    if sprite.mask_mode == 1 {
        color[3] = (color[0] * 0.2989 + color[1] * 0.5886 + color[2] * 0.1145).clamp(0.0, 1.0);
    }
    if sprite.alpha_test && color[3] < 1.0 / 255.0 {
        color[3] = 0.0;
    }
}

fn blend_pixel(dst: &mut [u8], src: [f32; 4], blend: SpriteBlend) {
    let d = [
        dst[0] as f32 / 255.0,
        dst[1] as f32 / 255.0,
        dst[2] as f32 / 255.0,
    ];
    let alpha = src[3].clamp(0.0, 1.0);
    let mut out = [0.0; 3];
    for channel in 0..3 {
        out[channel] = match blend {
            SpriteBlend::Normal => src[channel] * alpha + d[channel] * (1.0 - alpha),
            SpriteBlend::Add => (d[channel] + src[channel] * alpha).min(1.0),
            SpriteBlend::Sub => (d[channel] - src[channel] * alpha).max(0.0),
            SpriteBlend::Mul => d[channel] * (1.0 - alpha + src[channel] * alpha),
            SpriteBlend::Screen => 1.0 - (1.0 - d[channel]) * (1.0 - src[channel] * alpha),
            SpriteBlend::Overlay => {
                let over = if d[channel] <= 0.5 {
                    2.0 * d[channel] * src[channel]
                } else {
                    1.0 - 2.0 * (1.0 - d[channel]) * (1.0 - src[channel])
                };
                d[channel] * (1.0 - alpha) + over * alpha
            }
        }
        .clamp(0.0, 1.0);
    }
    dst[0] = (out[0] * 255.0).round() as u8;
    dst[1] = (out[1] * 255.0).round() as u8;
    dst[2] = (out[2] * 255.0).round() as u8;
    dst[3] = 255;
}

/// Eluna already performs the PSB/timeline/physics work for an Emote object.
/// This translates its resolved static draw list into the same software
/// intermediate used by the native renderer; the enclosing Siglus sprite is
/// still composed by `blit` above.
fn compose_emote(packet: &EmoteRenderPacket) -> RgbaImage {
    let width = packet.width.max(1);
    let height = packet.height.max(1);
    let mut rgba = vec![0; width as usize * height as usize * 4];
    for sprite in &packet.scene.sprites {
        if !sprite.visible || sprite.feedback_history {
            continue;
        }
        let Some(texture) = packet.textures.get(&sprite.texture_resource_index) else {
            continue;
        };
        if texture.rgba.len() < texture.width as usize * texture.height as usize * 4 {
            continue;
        }
        let corners = [
            emote_point(sprite, sprite.left(), sprite.top(), packet),
            emote_point(sprite, sprite.right(), sprite.top(), packet),
            emote_point(sprite, sprite.right(), sprite.bottom(), packet),
            emote_point(sprite, sprite.left(), sprite.bottom(), packet),
        ];
        let colors = [
            emote_color(sprite.corner_colors[0], sprite.opacity),
            emote_color(sprite.corner_colors[1], sprite.opacity),
            emote_color(sprite.corner_colors[3], sprite.opacity),
            emote_color(sprite.corner_colors[2], sprite.opacity),
        ];
        let uvs = [
            [sprite.uv_left, sprite.uv_top],
            [sprite.uv_right, sprite.uv_top],
            [sprite.uv_right, sprite.uv_bottom],
            [sprite.uv_left, sprite.uv_bottom],
        ];
        emote_triangle(
            &mut rgba, width, height, packet, sprite, texture.width, texture.height,
            texture.rgba.as_slice(), corners[0], corners[1], corners[2],
            uvs[0], uvs[1], uvs[2], colors[0], colors[1], colors[2],
        );
        emote_triangle(
            &mut rgba, width, height, packet, sprite, texture.width, texture.height,
            texture.rgba.as_slice(), corners[0], corners[2], corners[3],
            uvs[0], uvs[2], uvs[3], colors[0], colors[2], colors[3],
        );
    }
    RgbaImage {
        width,
        height,
        center_x: 0,
        center_y: 0,
        rgba,
    }
}

fn emote_point(
    sprite: &eluna::EmoteStaticSprite,
    x: f32,
    y: f32,
    packet: &EmoteRenderPacket,
) -> ProjectedPoint {
    let sx = if sprite.scale_x.is_finite() {
        sprite.scale_x
    } else {
        1.0
    };
    let sy = if sprite.scale_y.is_finite() {
        sprite.scale_y
    } else {
        1.0
    };
    let angle = if sprite.rotation_degrees.is_finite() {
        sprite.rotation_degrees.to_radians()
    } else {
        0.0
    };
    let (sin, cos) = angle.sin_cos();
    let dx = (x - sprite.center_x) * sx;
    let dy = (y - sprite.center_y) * sy;
    let lx = sprite.center_x + dx * cos - dy * sin;
    let ly = sprite.center_y + dx * sin + dy * cos;
    let m = sprite.world_transform;
    ProjectedPoint {
        x: m[0] * lx + m[1] * ly + m[4] - packet.rep_x,
        y: m[2] * lx + m[3] * ly + m[5] + packet.rep_y,
        depth: 0.0,
        clip_w: 1.0,
    }
}

fn emote_color(packed: u32, opacity: f32) -> [f32; 4] {
    [
        ((packed >> 24) & 0xff) as f32 / 255.0,
        ((packed >> 16) & 0xff) as f32 / 255.0,
        ((packed >> 8) & 0xff) as f32 / 255.0,
        ((packed & 0xff) as f32 / 255.0 * opacity).clamp(0.0, 1.0),
    ]
}

#[allow(clippy::too_many_arguments)]
fn emote_triangle(
    target: &mut [u8],
    width: u32,
    height: u32,
    packet: &EmoteRenderPacket,
    sprite: &eluna::EmoteStaticSprite,
    texture_width: u32,
    texture_height: u32,
    texture_rgba: &[u8],
    a: ProjectedPoint,
    b: ProjectedPoint,
    c: ProjectedPoint,
    auv: [f32; 2],
    buv: [f32; 2],
    cuv: [f32; 2],
    ac: [f32; 4],
    bc: [f32; 4],
    cc: [f32; 4],
) {
    let area = edge(a, b, c.x, c.y);
    if area.abs() <= f32::EPSILON {
        return;
    }
    let clip = sprite
        .draw_frame_info
        .clip_rect
        .unwrap_or([-1.0e30, -1.0e30, 1.0e30, 1.0e30]);
    let left = a.x.min(b.x).min(c.x).floor().max(0.0) as i32;
    let top = a.y.min(b.y).min(c.y).floor().max(0.0) as i32;
    let right = a.x.max(b.x).max(c.x).ceil().min(width as f32) as i32;
    let bottom = a.y.max(b.y).max(c.y).ceil().min(height as f32) as i32;
    for y in top..bottom {
        for x in left..right {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let wa = edge(b, c, px, py) / area;
            let wb = edge(c, a, px, py) / area;
            let wc = 1.0 - wa - wb;
            if wa < 0.0 || wb < 0.0 || wc < 0.0 {
                continue;
            }
            // Clip rectangles are authored in Emote model coordinates; undo
            // the render-target rep offset before testing them.
            let model_x = px + packet.rep_x;
            let model_y = py - packet.rep_y;
            if model_x < clip[0] || model_y < clip[1] || model_x > clip[2] || model_y > clip[3] {
                continue;
            }
            let u = wa * auv[0] + wb * buv[0] + wc * cuv[0];
            let v = wa * auv[1] + wb * buv[1] + wc * cuv[1];
            let mut color = sample_rgba_pixels(
                texture_width,
                texture_height,
                texture_rgba,
                u,
                v,
                false,
            );
            for channel in 0..4 {
                color[channel] *= wa * ac[channel] + wb * bc[channel] + wc * cc[channel];
            }
            if sprite.draw_frame_info.stencil_wipe_enabled {
                color[3] = (color[3] * sprite.draw_frame_info.stencil_wipe_scale
                    + sprite.draw_frame_info.stencil_wipe_bias)
                    .clamp(0.0, 1.0);
            }
            let low = sprite.blend_mode & 0xff0f;
            if sprite.blend_mode & 0xf0 == 0x10 {
                for channel in 0..3 {
                    color[channel] = (color[channel] * 2.0).min(1.0);
                }
            }
            if low == 3 || low == 4 {
                for channel in 0..3 {
                    color[channel] *= color[3];
                }
            }
            if low == 5 {
                for channel in 0..3 {
                    color[channel] = 1.0 - color[channel];
                }
            }
            if color[3] <= 0.003 {
                continue;
            }
            let i = (y as usize * width as usize + x as usize) * 4;
            emote_blend(&mut target[i..i + 4], color, low);
        }
    }
}

fn emote_blend(dst: &mut [u8], src: [f32; 4], mode: u32) {
    let d = [
        dst[0] as f32 / 255.0,
        dst[1] as f32 / 255.0,
        dst[2] as f32 / 255.0,
    ];
    let mut out = [0.0; 3];
    for channel in 0..3 {
        out[channel] = match mode {
            1 => (d[channel] + src[channel] * src[3]).min(1.0),
            2 | 5 => (d[channel] - src[channel] * src[3]).max(0.0),
            3 => d[channel] * src[channel] + d[channel] * (1.0 - src[3]),
            4 => (src[channel] * (1.0 - d[channel]) + d[channel]).min(1.0),
            _ => src[channel] * src[3] + d[channel] * (1.0 - src[3]),
        }
        .clamp(0.0, 1.0);
    }
    dst[0] = (out[0] * 255.0).round() as u8;
    dst[1] = (out[1] * 255.0).round() as u8;
    dst[2] = (out[2] * 255.0).round() as u8;
    dst[3] = 255;
}

impl FrameCaptureBackend for Renderer {
    fn capture_render_frame(
        &mut self,
        images: &ImageManager,
        frame: &RenderFrame,
        logical_width: u32,
        logical_height: u32,
    ) -> Result<RgbaImage> {
        let width = logical_width.max(1);
        let height = logical_height.max(1);
        if self.width != width || self.height != height {
            self.resize(width, height);
        }
        self.render_frame(images, frame)?;
        Ok(RgbaImage {
            width,
            height,
            center_x: 0,
            center_y: 0,
            rgba: self.framebuffer.clone(),
        })
    }
}
