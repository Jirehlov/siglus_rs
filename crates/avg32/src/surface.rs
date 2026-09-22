//! CPU-side AVG32 graphics surfaces.
//!
//! AVG32 scenarios address a small numbered bank of 640×480 PDT buffers.  The
//! renderer deliberately lives below any windowing/GPU backend: it is useful to
//! the native, Switch, and headless test frontends alike.

use anyhow::{Result, bail};

use crate::pdt::PdtImage;

pub const AVG32_WIDTH: u32 = 640;
pub const AVG32_HEIGHT: u32 = 480;
pub const AVG32_SURFACE_COUNT: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Surface {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl Surface {
    pub fn transparent(width: u32, height: u32) -> Result<Self> {
        let len = pixel_len(width, height)?;
        Ok(Self {
            width,
            height,
            rgba: vec![0; len],
        })
    }

    pub fn from_pdt(image: PdtImage) -> Self {
        Self {
            width: image.width,
            height: image.height,
            rgba: image.rgba,
        }
    }

    /// Builds a surface from externally decoded RGBA8 pixels (e.g. an FMV
    /// frame), validating the buffer is exactly `width * height * 4` bytes.
    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self> {
        if rgba.len() != pixel_len(width, height)? {
            bail!("avg32: RGBA buffer does not match {width}x{height}");
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn pixels(&self) -> &[u8] {
        &self.rgba
    }

    pub fn pixels_mut(&mut self) -> &mut [u8] {
        &mut self.rgba
    }

    pub fn clear(&mut self, color: [u8; 4]) {
        for pixel in self.rgba.chunks_exact_mut(4) {
            pixel.copy_from_slice(&color);
        }
    }

    /// Alpha-composites `source` into this surface. Source and destination
    /// rectangles are half-open, so they naturally clip at surface edges.
    pub fn blit(
        &mut self,
        source: &Surface,
        src_x: i32,
        src_y: i32,
        width: i32,
        height: i32,
        dst_x: i32,
        dst_y: i32,
    ) {
        if width <= 0 || height <= 0 {
            return;
        }
        for offset_y in 0..height {
            for offset_x in 0..width {
                let sx = src_x + offset_x;
                let sy = src_y + offset_y;
                let dx = dst_x + offset_x;
                let dy = dst_y + offset_y;
                if sx < 0
                    || sy < 0
                    || dx < 0
                    || dy < 0
                    || sx >= source.width as i32
                    || sy >= source.height as i32
                    || dx >= self.width as i32
                    || dy >= self.height as i32
                {
                    continue;
                }
                let source_at = (sy as usize * source.width as usize + sx as usize) * 4;
                let dest_at = (dy as usize * self.width as usize + dx as usize) * 4;
                blend_pixel(
                    &mut self.rgba[dest_at..dest_at + 4],
                    &source.rgba[source_at..source_at + 4],
                );
            }
        }
    }

    /// Blends a whole surface at the given destination coordinate.
    pub fn blit_full(&mut self, source: &Surface, dst_x: i32, dst_y: i32) {
        self.blit(
            source,
            0,
            0,
            source.width as i32,
            source.height as i32,
            dst_x,
            dst_y,
        );
    }

    /// Copies pixels without applying source alpha. AVG32's ordinary buffer
    /// copy transfers its separate mask alongside RGB; the mask is represented
    /// by alpha in this renderer, so this is the corresponding operation.
    pub fn copy_rect(
        &mut self,
        source: &Surface,
        src_x: i32,
        src_y: i32,
        width: i32,
        height: i32,
        dst_x: i32,
        dst_y: i32,
    ) {
        if width <= 0 || height <= 0 {
            return;
        }
        for offset_y in 0..height {
            for offset_x in 0..width {
                let sx = src_x + offset_x;
                let sy = src_y + offset_y;
                let dx = dst_x + offset_x;
                let dy = dst_y + offset_y;
                if sx < 0
                    || sy < 0
                    || dx < 0
                    || dy < 0
                    || sx >= source.width as i32
                    || sy >= source.height as i32
                    || dx >= self.width as i32
                    || dy >= self.height as i32
                {
                    continue;
                }
                let source_at = (sy as usize * source.width as usize + sx as usize) * 4;
                let dest_at = (dy as usize * self.width as usize + dx as usize) * 4;
                self.rgba[dest_at..dest_at + 4]
                    .copy_from_slice(&source.rgba[source_at..source_at + 4]);
            }
        }
    }

    /// Raw copy that leaves destination pixels matching `color_key` untouched.
    /// AVG32 calls this `MonoCopy`; despite the historical name it is a colour
    /// key operation rather than grayscale conversion.
    pub fn copy_rect_excluding_color(
        &mut self,
        source: &Surface,
        src_x: i32,
        src_y: i32,
        width: i32,
        height: i32,
        dst_x: i32,
        dst_y: i32,
        color_key: [u8; 3],
    ) {
        if width <= 0 || height <= 0 {
            return;
        }
        for offset_y in 0..height {
            for offset_x in 0..width {
                let sx = src_x + offset_x;
                let sy = src_y + offset_y;
                let dx = dst_x + offset_x;
                let dy = dst_y + offset_y;
                if sx < 0
                    || sy < 0
                    || dx < 0
                    || dy < 0
                    || sx >= source.width as i32
                    || sy >= source.height as i32
                    || dx >= self.width as i32
                    || dy >= self.height as i32
                {
                    continue;
                }
                let source_at = (sy as usize * source.width as usize + sx as usize) * 4;
                if source.rgba[source_at..source_at + 3] == color_key {
                    continue;
                }
                let dest_at = (dy as usize * self.width as usize + dx as usize) * 4;
                self.rgba[dest_at..dest_at + 4]
                    .copy_from_slice(&source.rgba[source_at..source_at + 4]);
            }
        }
    }

    pub fn multiply_alpha(&mut self, alpha: u8) {
        for pixel in self.rgba.chunks_exact_mut(4) {
            pixel[3] = ((u16::from(pixel[3]) * u16::from(alpha)) / 255) as u8;
        }
    }

    /// Applies an opaque RGB fade while preserving the alpha channel.
    pub fn fade_to(&mut self, color: [u8; 3], amount: u8) {
        let inverse = u16::from(255 - amount);
        let amount = u16::from(amount);
        for pixel in self.rgba.chunks_exact_mut(4) {
            for component in 0..3 {
                pixel[component] = ((u16::from(pixel[component]) * inverse
                    + u16::from(color[component]) * amount)
                    / 255) as u8;
            }
        }
    }

    pub fn fill_rect(&mut self, rect: [i32; 4], color: [u8; 4]) {
        let [left, top, right, bottom] = rect;
        for y in top.max(0)..=bottom.min(self.height as i32 - 1) {
            for x in left.max(0)..=right.min(self.width as i32 - 1) {
                let at = (y as usize * self.width as usize + x as usize) * 4;
                self.rgba[at..at + 4].copy_from_slice(&color);
            }
        }
    }

    pub fn invert_rect(&mut self, rect: [i32; 4]) {
        let [left, top, right, bottom] = rect;
        for y in top.max(0)..=bottom.min(self.height as i32 - 1) {
            for x in left.max(0)..=right.min(self.width as i32 - 1) {
                let at = (y as usize * self.width as usize + x as usize) * 4;
                self.rgba[at] = 255 - self.rgba[at];
                self.rgba[at + 1] = 255 - self.rgba[at + 1];
                self.rgba[at + 2] = 255 - self.rgba[at + 2];
            }
        }
    }

    pub fn outline_rect(&mut self, rect: [i32; 4], color: [u8; 3]) {
        let [left, top, right, bottom] = normalized_rect(rect);
        for y in top.max(0)..=bottom.min(self.height as i32 - 1) {
            for x in left.max(0)..=right.min(self.width as i32 - 1) {
                if x == left || x == right || y == top || y == bottom {
                    let at = (y as usize * self.width as usize + x as usize) * 4;
                    self.rgba[at..at + 3].copy_from_slice(&color);
                    self.rgba[at + 3] = 255;
                }
            }
        }
    }

    pub fn color_mask_rect(&mut self, rect: [i32; 4], color: [u8; 3]) {
        let [left, top, right, bottom] = normalized_rect(rect);
        for y in top.max(0)..=bottom.min(self.height as i32 - 1) {
            for x in left.max(0)..=right.min(self.width as i32 - 1) {
                let at = (y as usize * self.width as usize + x as usize) * 4;
                for component in 0..3 {
                    self.rgba[at + component] = ((u16::from(self.rgba[at + component])
                        * u16::from(color[component]).saturating_add(1))
                        >> 8) as u8;
                }
            }
        }
    }

    pub fn fade_rect(&mut self, rect: [i32; 4], color: [u8; 3], amount: u8) {
        let [left, top, right, bottom] = normalized_rect(rect);
        let amount = u16::from(amount).saturating_add(1);
        for y in top.max(0)..=bottom.min(self.height as i32 - 1) {
            for x in left.max(0)..=right.min(self.width as i32 - 1) {
                let at = (y as usize * self.width as usize + x as usize) * 4;
                for component in 0..3 {
                    let current = i32::from(self.rgba[at + component]);
                    let target = i32::from(color[component]);
                    self.rgba[at + component] = (current
                        + (((target - current) * i32::from(amount)) >> 8))
                        .clamp(0, 255) as u8;
                }
            }
        }
    }

    pub fn monochrome_rect(&mut self, rect: [i32; 4]) {
        let [left, top, right, bottom] = normalized_rect(rect);
        for y in top.max(0)..=bottom.min(self.height as i32 - 1) {
            for x in left.max(0)..=right.min(self.width as i32 - 1) {
                let at = (y as usize * self.width as usize + x as usize) * 4;
                let luminance = (u32::from(self.rgba[at]) * 299
                    + u32::from(self.rgba[at + 1]) * 587
                    + u32::from(self.rgba[at + 2]) * 114)
                    / 1000;
                self.rgba[at..at + 3].fill(luminance as u8);
            }
        }
    }

    /// Nearest-neighbour AVG32 buffer stretch. Both rectangles use inclusive
    /// coordinates, as they do in TPC bytecode.
    pub fn stretch_copy(&mut self, source: &Surface, source_rect: [i32; 4], destination: [i32; 4]) {
        let [sx1, sy1, sx2, sy2] = normalized_rect(source_rect);
        let [dx1, dy1, dx2, dy2] = normalized_rect(destination);
        let source_width = sx2 - sx1 + 1;
        let source_height = sy2 - sy1 + 1;
        let destination_width = dx2 - dx1 + 1;
        let destination_height = dy2 - dy1 + 1;
        if source_width <= 0
            || source_height <= 0
            || destination_width <= 0
            || destination_height <= 0
        {
            return;
        }
        for dy in dy1.max(0)..=dy2.min(self.height as i32 - 1) {
            for dx in dx1.max(0)..=dx2.min(self.width as i32 - 1) {
                let sx = sx1 + ((dx - dx1) * source_width) / destination_width;
                let sy = sy1 + ((dy - dy1) * source_height) / destination_height;
                if sx < 0 || sy < 0 || sx >= source.width as i32 || sy >= source.height as i32 {
                    continue;
                }
                let source_at = (sy as usize * source.width as usize + sx as usize) * 4;
                let destination_at = (dy as usize * self.width as usize + dx as usize) * 4;
                self.rgba[destination_at..destination_at + 4]
                    .copy_from_slice(&source.rgba[source_at..source_at + 4]);
            }
        }
    }
}

fn normalized_rect([left, top, right, bottom]: [i32; 4]) -> [i32; 4] {
    [
        left.min(right),
        top.min(bottom),
        left.max(right),
        top.max(bottom),
    ]
}

/// AVG32's fixed graphics buffer bank. Slot zero is the display surface.
///
/// The original engine keeps a fixed C array of PDT framebuffers: every slot
/// is addressable from boot, holding blank (black/transparent) pixels until a
/// scenario writes into it. Scripts routinely read a buffer that no prior
/// `LoadGraphic`/`CompositeGraphic` populated (e.g. AIR's transition scenes
/// `BufferCopy` straight from an untouched slot), so slots here are always
/// present rather than `Option`-wrapped — there is no "empty buffer" state to
/// reject.
#[derive(Debug, Clone)]
pub struct SurfaceBank {
    slots: Vec<Surface>,
}

impl Default for SurfaceBank {
    fn default() -> Self {
        Self::new().expect("fixed AVG32 display dimensions are valid")
    }
}

impl SurfaceBank {
    pub fn new() -> Result<Self> {
        let mut slots = Vec::with_capacity(AVG32_SURFACE_COUNT);
        for _ in 0..AVG32_SURFACE_COUNT {
            slots.push(Surface::transparent(AVG32_WIDTH, AVG32_HEIGHT)?);
        }
        Ok(Self { slots })
    }

    pub fn get(&self, index: usize) -> Option<&Surface> {
        self.slots.get(index)
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut Surface> {
        self.slots.get_mut(index)
    }

    pub fn insert(&mut self, index: usize, surface: Surface) -> Result<()> {
        let slot = self
            .slots
            .get_mut(index)
            .ok_or_else(|| anyhow::anyhow!("avg32: graphics buffer {index} is out of range"))?;
        *slot = surface;
        Ok(())
    }

    /// Resets a single buffer back to its boot-time blank state.
    pub fn remove(&mut self, index: usize) -> Result<()> {
        if index == 0 {
            bail!("avg32: the display surface cannot be removed");
        }
        let blank = Surface::transparent(AVG32_WIDTH, AVG32_HEIGHT)?;
        self.insert(index, blank)
    }

    pub fn clear_non_display(&mut self) {
        for slot in &mut self.slots[1..] {
            *slot = Surface::transparent(AVG32_WIDTH, AVG32_HEIGHT)
                .expect("fixed AVG32 display dimensions are valid");
        }
    }

    /// AVG32 `PDTMGR::Swap`: exchanges RGB content between an equally sized
    /// region of `source` and `destination` (which may be the same buffer),
    /// leaving each side's own alpha/mask plane untouched.
    pub fn swap_rgb(
        &mut self,
        source: usize,
        destination: usize,
        source_rect: [i32; 4],
        destination_xy: [i32; 2],
    ) -> Result<()> {
        let [sx1, sy1, sx2, sy2] = source_rect;
        let width = sx2 - sx1 + 1;
        let height = sy2 - sy1 + 1;
        if width <= 0 || height <= 0 {
            return Ok(());
        }
        if source >= self.slots.len() {
            bail!("avg32: graphics buffer {source} is out of range");
        }
        if destination >= self.slots.len() {
            bail!("avg32: graphics buffer {destination} is out of range");
        }
        let swap_region = |src: &mut Surface, dst: &mut Surface| {
            let (src_w, src_h) = (src.width() as i32, src.height() as i32);
            let (dst_w, dst_h) = (dst.width() as i32, dst.height() as i32);
            for offset_y in 0..height {
                for offset_x in 0..width {
                    let sx = sx1 + offset_x;
                    let sy = sy1 + offset_y;
                    let dx = destination_xy[0] + offset_x;
                    let dy = destination_xy[1] + offset_y;
                    if sx < 0
                        || sy < 0
                        || sx >= src_w
                        || sy >= src_h
                        || dx < 0
                        || dy < 0
                        || dx >= dst_w
                        || dy >= dst_h
                    {
                        continue;
                    }
                    let src_at = (sy as usize * src_w as usize + sx as usize) * 4;
                    let dst_at = (dy as usize * dst_w as usize + dx as usize) * 4;
                    let mut src_rgb = [0u8; 3];
                    src_rgb.copy_from_slice(&src.pixels()[src_at..src_at + 3]);
                    let dst_pixels = dst.pixels_mut();
                    let mut dst_rgb = [0u8; 3];
                    dst_rgb.copy_from_slice(&dst_pixels[dst_at..dst_at + 3]);
                    dst_pixels[dst_at..dst_at + 3].copy_from_slice(&src_rgb);
                    src.pixels_mut()[src_at..src_at + 3].copy_from_slice(&dst_rgb);
                }
            }
        };
        if source == destination {
            let surface = &mut self.slots[source];
            let w = surface.width() as i32;
            let h = surface.height() as i32;
            let pixels = surface.pixels_mut();
            for offset_y in 0..height {
                for offset_x in 0..width {
                    let sx = sx1 + offset_x;
                    let sy = sy1 + offset_y;
                    let dx = destination_xy[0] + offset_x;
                    let dy = destination_xy[1] + offset_y;
                    if sx < 0
                        || sy < 0
                        || sx >= w
                        || sy >= h
                        || dx < 0
                        || dy < 0
                        || dx >= w
                        || dy >= h
                    {
                        continue;
                    }
                    let src_at = (sy as usize * w as usize + sx as usize) * 4;
                    let dst_at = (dy as usize * w as usize + dx as usize) * 4;
                    if src_at == dst_at {
                        continue;
                    }
                    for component in 0..3 {
                        pixels.swap(src_at + component, dst_at + component);
                    }
                }
            }
        } else if source < destination {
            let (left, right) = self.slots.split_at_mut(destination);
            swap_region(&mut left[source], &mut right[0]);
        } else {
            let (left, right) = self.slots.split_at_mut(source);
            swap_region(&mut right[0], &mut left[destination]);
        }
        Ok(())
    }

    pub fn copy(&mut self, from: usize, to: usize) -> Result<()> {
        let surface = self
            .get(from)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("avg32: graphics buffer {from} is out of range"))?;
        self.insert(to, surface)
    }
}

fn pixel_len(width: u32, height: u32) -> Result<usize> {
    usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow::anyhow!("avg32: surface dimensions overflow"))
}

fn blend_pixel(destination: &mut [u8], source: &[u8]) {
    let source_alpha = u32::from(source[3]);
    if source_alpha == 0 {
        return;
    }
    if source_alpha == 255 {
        destination.copy_from_slice(source);
        return;
    }
    let destination_alpha = u32::from(destination[3]);
    let out_alpha = source_alpha + (destination_alpha * (255 - source_alpha) + 127) / 255;
    for component in 0..3 {
        let source_premultiplied = u32::from(source[component]) * source_alpha;
        let destination_premultiplied =
            u32::from(destination[component]) * destination_alpha * (255 - source_alpha) / 255;
        destination[component] =
            ((source_premultiplied + destination_premultiplied) / out_alpha) as u8;
    }
    destination[3] = out_alpha as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_blends_and_clips() {
        let mut destination = Surface::transparent(2, 1).unwrap();
        destination.clear([0, 0, 255, 255]);
        let mut source = Surface::transparent(1, 1).unwrap();
        source.pixels_mut().copy_from_slice(&[255, 0, 0, 128]);
        destination.blit_full(&source, 1, 0);
        assert_eq!(destination.pixels(), &[0, 0, 255, 255, 128, 0, 127, 255]);
    }

    #[test]
    fn screen_surface_is_kept() {
        let mut bank = SurfaceBank::new().unwrap();
        assert!(bank.remove(0).is_err());
        bank.insert(1, Surface::transparent(3, 3).unwrap()).unwrap();
        bank.clear_non_display();
        assert!(bank.get(0).is_some());
        // Non-display buffers reset to a blank full-screen surface rather
        // than becoming unaddressable.
        assert_eq!(bank.get(1).unwrap().width(), AVG32_WIDTH);
        assert_eq!(bank.get(1).unwrap().height(), AVG32_HEIGHT);
    }

    #[test]
    fn every_buffer_is_addressable_before_any_write() {
        let bank = SurfaceBank::new().unwrap();
        for index in 0..AVG32_SURFACE_COUNT {
            let surface = bank
                .get(index)
                .expect("all fixed buffers are pre-allocated");
            assert_eq!(surface.width(), AVG32_WIDTH);
            assert_eq!(surface.height(), AVG32_HEIGHT);
        }
        assert!(bank.get(AVG32_SURFACE_COUNT).is_none());
    }

    #[test]
    fn region_operations_preserve_their_distinct_semantics() {
        let mut surface = Surface::transparent(3, 3).unwrap();
        surface.clear([100, 150, 200, 255]);
        surface.outline_rect([0, 0, 2, 2], [1, 2, 3]);
        assert_eq!(&surface.pixels()[0..4], &[1, 2, 3, 255]);
        assert_eq!(&surface.pixels()[16..20], &[100, 150, 200, 255]);

        surface.color_mask_rect([1, 1, 1, 1], [127, 255, 0]);
        assert_eq!(&surface.pixels()[16..20], &[50, 150, 0, 255]);
        surface.fade_rect([1, 1, 1, 1], [250, 50, 100], 255);
        assert_eq!(&surface.pixels()[16..20], &[250, 50, 100, 255]);
        surface.monochrome_rect([1, 1, 1, 1]);
        assert_eq!(&surface.pixels()[16..20], &[115, 115, 115, 255]);
    }

    #[test]
    fn stretch_copy_uses_inclusive_avg32_rectangles() {
        let mut source = Surface::transparent(2, 1).unwrap();
        source
            .pixels_mut()
            .copy_from_slice(&[10, 0, 0, 255, 20, 0, 0, 255]);
        let mut destination = Surface::transparent(4, 1).unwrap();
        destination.stretch_copy(&source, [0, 0, 1, 0], [0, 0, 3, 0]);
        assert_eq!(
            destination.pixels(),
            &[10, 0, 0, 255, 10, 0, 0, 255, 20, 0, 0, 255, 20, 0, 0, 255]
        );
    }
}
