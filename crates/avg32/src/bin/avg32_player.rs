//! Minimal desktop presentation shell for the platform-neutral AVG32 runtime.
//!
//! The engine keeps graphics and VM state in `avg32`; this binary is merely a
//! 640x480 RGBA uploader plus conventional mouse/keyboard input routing.

use std::borrow::Cow;
use std::path::PathBuf;

use ab_glyph::{Font, FontVec, Glyph, PxScale, point};
use anyhow::{Context, Result, bail};
use clap::Parser;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use avg32::runtime::Avg32Runtime;
use avg32::surface::{AVG32_HEIGHT, AVG32_WIDTH};
use avg32::vm::{Input, VmAction, VmStop};

const SHADER: &str = r#"
@group(0) @binding(0) var frame_texture: texture_2d<f32>;
@group(0) @binding(1) var frame_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VertexOutput {
    var positions = array<vec2<f32>, 6>(
        vec2(-1.0, -1.0), vec2(1.0, -1.0), vec2(-1.0, 1.0),
        vec2(-1.0, 1.0), vec2(1.0, -1.0), vec2(1.0, 1.0),
    );
    var uvs = array<vec2<f32>, 6>(
        vec2(0.0, 1.0), vec2(1.0, 1.0), vec2(0.0, 0.0),
        vec2(0.0, 0.0), vec2(1.0, 1.0), vec2(1.0, 0.0),
    );
    var output: VertexOutput;
    output.position = vec4(positions[index], 0.0, 1.0);
    output.uv = uvs[index];
    return output;
}

@fragment fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(frame_texture, frame_sampler, input.uv);
}
"#;

#[derive(Debug, Parser)]
struct Args {
    /// Installed AVG32 game directory containing Gameexe.ini and DAT/SEEN.TXT.
    game_root: PathBuf,

    /// Integer window scale for the original 640x480 framebuffer.
    #[arg(long, default_value_t = 2)]
    scale: u32,
}

/// Fits the fixed 640x480 AVG32 framebuffer into an arbitrarily resized
/// window while preserving its aspect ratio (pillar/letterboxed), mirroring
/// `siglus_scene_vm`'s `aspect_fit_viewport` so both engines' desktop shells
/// behave the same way under free window resizing instead of stretching the
/// image to fill a non-4:3 window.
fn aspect_fit_viewport(surface_w: u32, surface_h: u32, game_w: u32, game_h: u32) -> (u32, u32, u32, u32) {
    let surface_w = surface_w.max(1);
    let surface_h = surface_h.max(1);
    let game_w = game_w.max(1);
    let game_h = game_h.max(1);
    let scale = (surface_w as f64 / game_w as f64).min(surface_h as f64 / game_h as f64);
    let viewport_w = ((game_w as f64 * scale).floor() as u32).clamp(1, surface_w);
    let viewport_h = ((game_h as f64 * scale).floor() as u32).clamp(1, surface_h);
    let viewport_x = surface_w.saturating_sub(viewport_w) / 2;
    let viewport_y = surface_h.saturating_sub(viewport_h) / 2;
    (viewport_x, viewport_y, viewport_w, viewport_h)
}

struct PlayerState {
    runtime: Avg32Runtime,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    configuration: wgpu::SurfaceConfiguration,
    frame_texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
    pipeline: wgpu::RenderPipeline,
    cursor: (f64, f64),
    text_font: Option<FontVec>,
    buffer_text: Vec<BufferText>,
}

#[derive(Debug, Clone)]
struct BufferText {
    buffer: u32,
    position: [i32; 2],
    color: [i32; 3],
    text: String,
}

impl PlayerState {
    async fn new(window: &'static dyn Window, root: PathBuf) -> Result<Self> {
        let runtime = Avg32Runtime::open(&root)
            .with_context(|| format!("open AVG32 game at {}", root.display()))?;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });
        let surface = instance
            .create_surface(window)
            .context("create GPU surface")?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .context("request GPU adapter")?;
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("avg32-player-device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::default(),
                },
                None,
            )
            .await
            .context("request GPU device")?;
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|format| format.is_srgb())
            .unwrap_or(caps.formats[0]);
        let size = window.surface_size();
        let configuration = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: caps
                .present_modes
                .iter()
                .copied()
                .find(|mode| *mode == wgpu::PresentMode::Fifo)
                .unwrap_or(caps.present_modes[0]),
            alpha_mode: caps
                .alpha_modes
                .iter()
                .copied()
                .find(|mode| *mode == wgpu::CompositeAlphaMode::Opaque)
                .unwrap_or(caps.alpha_modes[0]),
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &configuration);

        let frame_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("avg32-framebuffer"),
            size: wgpu::Extent3d {
                width: AVG32_WIDTH,
                height: AVG32_HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let texture_view = frame_texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("avg32-framebuffer-sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("avg32-framebuffer-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("avg32-framebuffer-bind-group"),
            layout: &bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&texture_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("avg32-present-shader"),
            source: wgpu::ShaderSource::Wgsl(Cow::Borrowed(SHADER)),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("avg32-present-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("avg32-present-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });
        Ok(Self {
            runtime,
            surface,
            device,
            queue,
            configuration,
            frame_texture,
            bind_group,
            pipeline,
            cursor: (0.0, 0.0),
            text_font: load_system_font(),
            buffer_text: Vec::new(),
        })
    }

    fn drive(&mut self, input: Input) -> Result<()> {
        let mut input = input;
        for _ in 0..512 {
            let outcome = self.runtime.advance(input, 65_536)?;
            if let VmStop::Yield(action) = &outcome {
                self.apply_frontend_action(action);
            }
            match outcome {
                VmStop::Ended | VmStop::Yield(VmAction::End) => return Ok(()),
                VmStop::Yield(
                    VmAction::WaitForInput { .. }
                    | VmAction::WaitForPointer
                    | VmAction::Wait { .. }
                    | VmAction::Choice { .. },
                ) => return Ok(()),
                VmStop::Yield(_) => input = Input::None,
            }
        }
        bail!("AVG32 front-end exceeded 512 consecutive yield actions")
    }

    fn apply_frontend_action(&mut self, action: &VmAction) {
        match action {
            VmAction::DrawBufferText {
                buffer,
                position,
                color,
                text,
            } => self.buffer_text.push(BufferText {
                buffer: *buffer,
                position: *position,
                color: *color,
                text: text.clone(),
            }),
            VmAction::ClearGraphicBuffers => self.buffer_text.retain(|item| item.buffer == 0),
            VmAction::CompositeGraphic { .. } => self
                .buffer_text
                .retain(|item| item.buffer != 0 && item.buffer != 1),
            VmAction::LoadGraphic { target, .. }
            | VmAction::BufferFill { buffer: target, .. }
            | VmAction::BufferOutline { buffer: target, .. }
            | VmAction::BufferInvert { buffer: target, .. }
            | VmAction::BufferColorMask { buffer: target, .. }
            | VmAction::BufferFade { buffer: target, .. }
            | VmAction::BufferMonochrome { buffer: target, .. }
            | VmAction::BufferStretchCopy {
                destination: target,
                ..
            }
            | VmAction::BufferScroll {
                destination: target,
                ..
            } => self.buffer_text.retain(|item| item.buffer != *target),
            VmAction::BufferCopy { destination, .. } => {
                self.buffer_text.retain(|item| item.buffer != *destination)
            }
            VmAction::BufferSwap {
                source,
                destination,
                ..
            } => self
                .buffer_text
                .retain(|item| item.buffer != *source && item.buffer != *destination),
            _ => {}
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.configuration.width = width;
        self.configuration.height = height;
        self.surface.configure(&self.device, &self.configuration);
    }

    /// The letterboxed rect (within the physical surface) the 640x480
    /// framebuffer is actually drawn into at the current window size.
    fn viewport(&self) -> (u32, u32, u32, u32) {
        aspect_fit_viewport(
            self.configuration.width,
            self.configuration.height,
            AVG32_WIDTH,
            AVG32_HEIGHT,
        )
    }

    fn pointer_input(&self, button: i32) -> Input {
        let (vx, vy, vw, vh) = self.viewport();
        let x = ((self.cursor.0 - f64::from(vx)) * f64::from(AVG32_WIDTH) / f64::from(vw.max(1)))
            .floor() as i32;
        let y = ((self.cursor.1 - f64::from(vy)) * f64::from(AVG32_HEIGHT) / f64::from(vh.max(1)))
            .floor() as i32;
        Input::Pointer { x, y, button }
    }

    fn render(&mut self) -> Result<()> {
        self.runtime.tick()?;
        self.drive(Input::None)?;
        let mut pixels = self.runtime.renderer().display().pixels().to_vec();
        if let Some(font) = &self.text_font {
            draw_message(
                font,
                &mut pixels,
                self.runtime.message(),
                self.runtime.config(),
            );
            for item in self.buffer_text.iter().filter(|item| item.buffer == 0) {
                draw_text(
                    font,
                    &mut pixels,
                    &item.text,
                    item.position,
                    [
                        item.color[0] as u8,
                        item.color[1] as u8,
                        item.color[2] as u8,
                    ],
                    self.runtime.config().message_font_size()[1].max(1),
                );
            }
        }
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.frame_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &pixels,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4 * AVG32_WIDTH),
                rows_per_image: Some(AVG32_HEIGHT),
            },
            wgpu::Extent3d {
                width: AVG32_WIDTH,
                height: AVG32_HEIGHT,
                depth_or_array_layers: 1,
            },
        );
        let frame = match self.surface.get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                self.surface.configure(&self.device, &self.configuration);
                return Ok(());
            }
            Err(wgpu::SurfaceError::Timeout) => return Ok(()),
            Err(wgpu::SurfaceError::OutOfMemory) => bail!("GPU surface out of memory"),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("avg32-present-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("avg32-present-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            let (vx, vy, vw, vh) = self.viewport();
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_viewport(vx as f32, vy as f32, vw as f32, vh as f32, 0.0, 1.0);
            pass.set_scissor_rect(vx, vy, vw, vh);
            pass.draw(0..6, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }
}

fn load_system_font() -> Option<FontVec> {
    #[cfg(target_os = "macos")]
    const CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/ヒラギノ角ゴシック W3.ttc",
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
    ];
    #[cfg(target_os = "windows")]
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\msgothic.ttc",
        r"C:\Windows\Fonts\meiryo.ttc",
    ];
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    ];
    CANDIDATES.iter().find_map(|path| {
        let bytes = std::fs::read(path).ok()?;
        (0..8).find_map(|index| FontVec::try_from_vec_and_index(bytes.clone(), index).ok())
    })
}

fn draw_message(
    font: &FontVec,
    pixels: &mut [u8],
    message: &avg32::text::MessageWindow,
    config: &avg32::config::Avg32Config,
) {
    if !message.visible {
        return;
    }
    let position = config.message_position();
    let font_size = config.message_font_size();
    let scale = PxScale::from(font_size[1].max(1) as f32);
    let line_height = font_size[1].max(1) as f32;
    let color = config.color(message.color_index);
    let mut baseline = position[1] as f32 + font.ascent_unscaled() * scale.y;
    for line in message.lines() {
        draw_text(
            font,
            pixels,
            line,
            [position[0], baseline as i32],
            color,
            font_size[1],
        );
        baseline += line_height;
    }
}

fn draw_text(
    font: &FontVec,
    pixels: &mut [u8],
    text: &str,
    position: [i32; 2],
    color: [u8; 3],
    size: i32,
) {
    let scale = PxScale::from(size.max(1) as f32);
    let mut x = position[0] as f32;
    for character in text.chars() {
        let glyph = font
            .glyph_id(character)
            .with_scale_and_position(scale, point(x, position[1] as f32));
        x += font.h_advance_unscaled(glyph.id) * scale.x;
        let Some(outline) = font.outline_glyph(glyph) else {
            continue;
        };
        let bounds = outline.px_bounds();
        outline.draw(|glyph_x, glyph_y, coverage| {
            let x = glyph_x as i32 + bounds.min.x as i32;
            let y = glyph_y as i32 + bounds.min.y as i32;
            if x < 0 || y < 0 || x >= AVG32_WIDTH as i32 || y >= AVG32_HEIGHT as i32 {
                return;
            }
            let at = (y as usize * AVG32_WIDTH as usize + x as usize) * 4;
            let alpha = coverage * 255.0;
            for component in 0..3 {
                pixels[at + component] = ((pixels[at + component] as f32 * (255.0 - alpha)
                    + color[component] as f32 * alpha)
                    / 255.0) as u8;
            }
            pixels[at + 3] = 255;
        });
    }
}

struct App {
    args: Args,
    window: Option<&'static dyn Window>,
    state: Option<PlayerState>,
}

impl ApplicationHandler for App {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let scale = self.args.scale.max(1);
        let window = match event_loop.create_window(
            winit::window::WindowAttributes::default()
                .with_title("AVG32 player")
                // The window is freely resizable by dragging its edges, like
                // siglus_scene_vm's desktop shell; the 640x480 framebuffer is
                // letterboxed into whatever size the player drags it to
                // instead of being stretched (see `aspect_fit_viewport`).
                .with_resizable(true)
                .with_surface_size(LogicalSize::new(
                    f64::from(AVG32_WIDTH * scale),
                    f64::from(AVG32_HEIGHT * scale),
                ))
                .with_min_surface_size(LogicalSize::new(
                    f64::from(AVG32_WIDTH / 4),
                    f64::from(AVG32_HEIGHT / 4),
                )),
        ) {
            Ok(window) => window,
            Err(error) => {
                eprintln!("create window: {error}");
                event_loop.exit();
                return;
            }
        };
        let window: &'static dyn Window = Box::leak(window);
        match pollster::block_on(PlayerState::new(window, self.args.game_root.clone())) {
            Ok(mut state) => {
                if let Err(error) = state.drive(Input::None) {
                    eprintln!("start AVG32 runtime: {error:#}");
                    event_loop.exit();
                    return;
                }
                self.window = Some(window);
                self.state = Some(state);
                window.request_redraw();
            }
            Err(error) => {
                eprintln!("initialize AVG32 player: {error:#}");
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &dyn ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window else {
            return;
        };
        if window.id() != window_id {
            return;
        }
        let Some(state) = self.state.as_mut() else {
            return;
        };
        let result = match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
                Ok(())
            }
            WindowEvent::SurfaceResized(size) => {
                state.resize(size.width, size.height);
                Ok(())
            }
            WindowEvent::ScaleFactorChanged { .. } => {
                let size = window.surface_size();
                state.resize(size.width, size.height);
                Ok(())
            }
            WindowEvent::PointerMoved { position, .. } => {
                state.cursor = (position.x, position.y);
                Ok(())
            }
            WindowEvent::PointerButton {
                state: ElementState::Released,
                button,
                position,
                ..
            } => {
                state.cursor = (position.x, position.y);
                state.drive(
                    state.pointer_input(button.mouse_button().map_or(0, |button| button as i32)),
                )
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && matches!(
                        event.physical_key,
                        PhysicalKey::Code(KeyCode::Enter | KeyCode::Space | KeyCode::ArrowDown)
                    ) =>
            {
                state.drive(Input::Advance)
            }
            WindowEvent::RedrawRequested => state.render(),
            _ => Ok(()),
        };
        if let Err(error) = result {
            eprintln!("AVG32 runtime: {error:#}");
            event_loop.exit();
        }
        window.request_redraw();
    }

    fn about_to_wait(&mut self, _event_loop: &dyn ActiveEventLoop) {
        if let Some(window) = self.window {
            window.request_redraw();
        }
    }
}

fn main() -> Result<()> {
    let event_loop = EventLoop::new().context("create event loop")?;
    event_loop
        .run_app(App {
            args: Args::parse(),
            window: None,
            state: None,
        })
        .context("run AVG32 player")
}
