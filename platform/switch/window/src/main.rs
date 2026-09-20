#![no_std]
#![no_main]

extern crate alloc;

mod software_renderer;

use ::alloc::format;

use alloc::sync::Arc;
use nx::diag::abort;
use nx::fs::FileOpenOption;
use nx::gpu::canvas::Canvas as _;
use nx::gpu::{BlockLinearHeights, SCREEN_HEIGHT, SCREEN_WIDTH};
use nx::input;
use nx::ipc::sf::AppletResourceUserId;
use nx::result::*;
use nx::service::hid;
use nx::service::vi::LayerFlags;
use nx::sync::RwLock;
use nx::util;
use nx::{fs, gpu};
use software_renderer::{
    BlendMode, LOGICAL_HEIGHT, LOGICAL_WIDTH, draw_background, draw_textured_quad,
};

use core::fmt::Write;
use core::panic;

// The double-buffered RGBA8888 logical framebuffer and its CPU staging buffer
// need about 12 MiB.  Leave room for game data and the future decoder stack.
const CUSTOM_HEAP_LEN: usize = 0x20_00000;
static mut CUSTOM_HEAP: [u8; CUSTOM_HEAP_LEN] = [0; CUSTOM_HEAP_LEN];

#[unsafe(no_mangle)]
#[allow(static_mut_refs)]
pub fn initialize_heap(_hbl_heap: util::PointerAndSize) -> util::PointerAndSize {
    unsafe { util::PointerAndSize::new(&raw mut CUSTOM_HEAP as *mut _, CUSTOM_HEAP.len()) }
}

static LOG_PATH: &str = "sdmc:/switch/siglus_rs/siglus-switch.log";

type RGBType = nx::gpu::canvas::RGBA8;
const WIDTH: u32 = LOGICAL_WIDTH;
const HEIGHT: u32 = LOGICAL_HEIGHT;
const BLOCK_HEIGHT_CONFIG: BlockLinearHeights = gpu::BlockLinearHeights::TwoGobs;
const BUFFER_COUNT: u32 = 2;

#[unsafe(no_mangle)]
pub fn main() {
    fs::initialize_fspsrv_session().expect("Error starting filesystem services");
    fs::mount_sd_card("sdmc").expect("Failed to mount sd card");
    // libnx's create flag creates a file, not its parent directories.  Ignore
    // an already-exists error so a fresh SD card and subsequent launches both
    // reach the renderer.
    let _ = fs::create_directory("sdmc:/switch");
    let _ = fs::create_directory("sdmc:/switch/siglus_rs");

    let mut file = fs::open_file(
        LOG_PATH,
        FileOpenOption::Create() | FileOpenOption::Write() | FileOpenOption::Append(),
    )
    .expect("Failed to open log file.");

    let gpu_ctx = Arc::new(RwLock::new(
        match gpu::Context::new(
            gpu::NvDrvServiceKind::Applet,
            gpu::ViServiceKind::Manager,
            0x40000,
        ) {
            Ok(ok) => ok,
            Err(e) => {
                let _ = file.write_fmt(format_args!("Error getting gpu context: {}\n", e));
                return;
            }
        },
    ));

    let supported_tags =
        hid::NpadStyleTag::Handheld() | hid::NpadStyleTag::FullKey() | hid::NpadStyleTag::JoyDual();
    let input_ctx = match input::Context::new(supported_tags, 1) {
        Ok(ok) => ok,
        Err(e) => {
            let _ = file.write_fmt(format_args!("Error getting input context: {}\n", e));
            return;
        }
    };

    let x = (SCREEN_WIDTH - WIDTH) / 2;
    let y = (SCREEN_HEIGHT - HEIGHT) / 2;

    let _c_black = RGBType::new_scaled(0, 0, 0, 0xFF);

    let mut surface = match nx::gpu::canvas::CanvasManager::new_managed(
        gpu_ctx,
        Default::default(),
        x,
        y,
        gpu::LayerZ::Max,
        WIDTH,
        HEIGHT,
        AppletResourceUserId::new(0),
        LayerFlags::None(),
        BUFFER_COUNT,
        BLOCK_HEIGHT_CONFIG,
        gpu::surface::ScaleMode::PreseveAspect { height: 1080 },
    ) {
        Ok(ok) => ok,
        Err(e) => {
            let _ = file.write_fmt(format_args!("Error getting surface: {}\n", e));
            return;
        }
    };

    let mut frame = 0u32;
    let mut blend_mode = BlendMode::Alpha;
    'render: loop {
        for controller in [hid::NpadIdType::Handheld, hid::NpadIdType::No1]
            .iter()
            .cloned()
        {
            let mut p_handheld = input_ctx.get_player(controller);

            let buttons_down = p_handheld.get_buttons_down();
            if buttons_down.contains(hid::NpadButton::A()) {
                blend_mode = match blend_mode {
                    BlendMode::Alpha => BlendMode::Additive,
                    BlendMode::Additive => BlendMode::Alpha,
                };
            }
            if buttons_down.contains(hid::NpadButton::Plus()) {
                // Exit if Plus/+ is pressed.
                break 'render;
            }
        }

        let _ = surface.render(Some(_c_black), |canvas| {
            draw_background(canvas, frame);
            draw_textured_quad(
                canvas,
                (frame as i32 % 620) - 40,
                180,
                240,
                160,
                &SPRITE_TEXTURE,
                8,
                8,
                [255, 220, 240, 210],
                blend_mode,
            );
            canvas.draw_ascii_bitmap_text(
                "SIGLUS SWITCH",
                RGBType::new_scaled(255, 255, 255, 255),
                3,
                44,
                40,
                gpu::canvas::AlphaBlend::Source,
            );
            canvas.draw_ascii_bitmap_text(
                "CPU bootstrap renderer",
                RGBType::new_scaled(175, 205, 255, 255),
                2,
                44,
                90,
                gpu::canvas::AlphaBlend::Source,
            );
            canvas.draw_ascii_bitmap_text(
                match blend_mode {
                    BlendMode::Alpha => "A: additive blend   +: quit",
                    BlendMode::Additive => "A: alpha blend      +: quit",
                },
                RGBType::new_scaled(230, 230, 230, 255),
                2,
                44,
                420,
                gpu::canvas::AlphaBlend::Source,
            );

            Ok(())
        });
        let _ = surface.wait_vsync_event(None);
        frame = frame.wrapping_add(1);
    }
}

const SPRITE_TEXTURE: [[u8; 4]; 64] = [
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [255, 255, 255, 180],
    [255, 255, 255, 180],
    [255, 255, 255, 180],
    [255, 255, 255, 180],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [255, 180, 205, 220],
    [255, 180, 205, 220],
    [255, 225, 235, 255],
    [255, 225, 235, 255],
    [255, 180, 205, 220],
    [255, 180, 205, 220],
    [0, 0, 0, 0],
    [255, 180, 205, 220],
    [255, 210, 225, 255],
    [80, 45, 90, 255],
    [255, 220, 235, 255],
    [255, 220, 235, 255],
    [80, 45, 90, 255],
    [255, 210, 225, 255],
    [255, 180, 205, 220],
    [255, 180, 205, 220],
    [255, 220, 235, 255],
    [255, 220, 235, 255],
    [255, 170, 200, 255],
    [255, 170, 200, 255],
    [255, 220, 235, 255],
    [255, 220, 235, 255],
    [255, 180, 205, 220],
    [255, 180, 205, 220],
    [255, 210, 225, 255],
    [255, 220, 235, 255],
    [255, 240, 245, 255],
    [255, 240, 245, 255],
    [255, 220, 235, 255],
    [255, 210, 225, 255],
    [255, 180, 205, 220],
    [0, 0, 0, 0],
    [255, 180, 205, 220],
    [255, 210, 225, 255],
    [255, 220, 235, 255],
    [255, 220, 235, 255],
    [255, 210, 225, 255],
    [255, 180, 205, 220],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [255, 180, 205, 220],
    [255, 180, 205, 220],
    [255, 180, 205, 220],
    [255, 180, 205, 220],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [255, 180, 205, 220],
    [255, 180, 205, 220],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
    [0, 0, 0, 0],
];

#[panic_handler]
fn panic_handler(info: &panic::PanicInfo) -> ! {
    let _panic_str = format!("{}", info);
    nx::diag::abort::abort(
        abort::AbortLevel::FatalThrow(),
        nx::rc::ResultPanicked::make(),
    );
}
