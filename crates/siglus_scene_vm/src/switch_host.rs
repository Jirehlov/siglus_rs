//! Horizon host entry point for the existing Siglus VM.
//!
//! Unlike the desktop host this module owns libnx lifecycle/input.  It is kept
//! separate so VM, script and resource semantics stay shared with every other
//! target.

use anyhow::Result;
use std::ffi::{CStr, c_char};
use std::io::Write;
use std::path::PathBuf;

use crate::host::{SiglusHost, SiglusHostConfig};
use crate::render::Renderer;
use crate::runtime::input::VmKey;

unsafe extern "C" {
    fn siglus_switch_random_fill(buffer: *mut u8, length: usize);
    fn siglus_switch_log_message(message: *const c_char);
}

/// Write a NUL-terminated static startup marker through the C shell. This
/// avoids Rust stdio while the Horizon engine is still being constructed.
pub(crate) fn report_switch_marker(message: &'static [u8]) {
    debug_assert!(message.last() == Some(&0));
    unsafe { siglus_switch_log_message(message.as_ptr().cast()) };
}

/// Write a temporary runtime diagnostic assembled by the Horizon host. Keep
/// this here so shared VM code never gains a platform logging dependency.
pub(crate) fn report_switch_diagnostic(message: &str) {
    if let Ok(message_c) = std::ffi::CString::new(message) {
        unsafe { siglus_switch_log_message(message_c.as_ptr()) };
    }
}

fn report_switch_error(context: &str, err: &anyhow::Error) {
    let message = format!("{context}: {err:#}\n");
    eprintln!("{message}");
    if let Ok(message_c) = std::ffi::CString::new(message.as_str()) {
        unsafe { siglus_switch_log_message(message_c.as_ptr()) };
    }
    // Standard error is not normally visible from a homebrew launcher. Keep
    // a small append-only diagnostic on the mounted SD card without making
    // logging itself a startup requirement.
    if let Ok(mut log) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("sdmc:/switch/siglus_rs/siglus_switch.log")
    {
        let _ = log.write_all(message.as_bytes());
    }
}

/// Entropy hook required by rand 0.9/eluna on Horizon. It is defined once in
/// this application root and delegates to libnx's kernel-seeded random source.
#[unsafe(no_mangle)]
unsafe extern "Rust" fn __getrandom_v03_custom(
    dest: *mut u8,
    len: usize,
) -> Result<(), getrandom_03::Error> {
    if dest.is_null() && len != 0 {
        return Err(getrandom_03::Error::UNEXPECTED);
    }
    unsafe { siglus_switch_random_fill(dest, len) };
    Ok(())
}

pub struct SwitchHost {
    host: SiglusHost,
}

impl SwitchHost {
    pub fn new(config: SiglusHostConfig, width: u32, height: u32) -> Result<Self> {
        report_switch_marker(b"siglus_switch: rust renderer-create begin\n\0");
        let renderer = Renderer::new(width, height)?;
        report_switch_marker(b"siglus_switch: rust renderer-create complete\n\0");
        report_switch_marker(b"siglus_switch: rust host-create begin\n\0");
        let host = SiglusHost::new_with_renderer_sync(config, renderer)?;
        report_switch_marker(b"siglus_switch: rust host-create complete\n\0");
        Ok(Self { host })
    }

    /// Drive one original-engine frame.  The libnx frontend supplies the
    /// elapsed time and maps controller/touch input to the shared VM input API.
    pub fn step(&mut self, dt_ms: u32) -> Result<bool> {
        self.host.step(dt_ms)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.host.resize(width, height, 1.0);
    }

    pub fn key_down(&mut self, key: VmKey) {
        self.host.key_down(key);
    }

    pub fn key_up(&mut self, key: VmKey) {
        self.host.key_up(key);
    }

    pub fn touch(&mut self, phase: i32, x: f64, y: f64) {
        self.host.touch(phase, x, y);
    }

    pub fn gamepad_button(&mut self, button: u8, down: bool) {
        let key = match button {
            0 => Some(VmKey::Enter),   // A
            1 => Some(VmKey::Escape),  // B
            2 => Some(VmKey::Space),   // X
            3 => Some(VmKey::Tab),     // Y
            6 => Some(VmKey::Shift),   // L
            7 => Some(VmKey::Control), // R
            8 => Some(VmKey::Control), // ZL
            9 => Some(VmKey::Space),   // ZR
            10 => Some(VmKey::Escape), // Plus
            11 => Some(VmKey::Tab),    // Minus
            12 => Some(VmKey::ArrowLeft),
            13 => Some(VmKey::ArrowUp),
            14 => Some(VmKey::ArrowRight),
            15 => Some(VmKey::ArrowDown),
            16 | 20 => Some(VmKey::ArrowLeft),
            17 | 21 => Some(VmKey::ArrowUp),
            18 | 22 => Some(VmKey::ArrowRight),
            19 | 23 => Some(VmKey::ArrowDown),
            _ => None,
        };
        // Feed the keyboard compatibility path first. `InputState::on_key_down`
        // deliberately switches the active input family to keyboard/mouse, so
        // recording the raw joypad edge before this call made every mapped
        // controller press immediately cancel JOYPAD mode again.
        if let Some(key) = key {
            if down {
                self.host.key_down(key);
            } else {
                self.host.key_up(key);
            }
        }
        // Keep the VM pump live for buttons which have no keyboard equivalent;
        // games can consume their raw JOY_BUTTON edge directly.
        self.host.joypad_button(button as usize, down);
    }

    pub fn host_mut(&mut self) -> &mut SiglusHost {
        &mut self.host
    }
}

/// C ABI used by the libnx/deko3d application shell.  The shell owns Horizon
/// lifecycle and controller polling; this object owns the existing engine host
/// and therefore the original VM/resource/script flow.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn siglus_switch_engine_create(
    project_dir: *const c_char,
    width: u32,
    height: u32,
) -> *mut SwitchHost {
    if project_dir.is_null() {
        return std::ptr::null_mut();
    }
    let path = unsafe { CStr::from_ptr(project_dir) }
        .to_string_lossy()
        .into_owned();
    let config = SiglusHostConfig::new(PathBuf::from(path));
    match SwitchHost::new(config, width, height) {
        Ok(host) => Box::into_raw(Box::new(host)),
        Err(err) => {
            report_switch_error("Switch engine startup failed", &err);
            std::ptr::null_mut()
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn siglus_switch_engine_step(host: *mut SwitchHost, dt_ms: u32) -> bool {
    let Some(host) = (unsafe { host.as_mut() }) else {
        return true;
    };
    match host.step(dt_ms) {
        Ok(exit) => exit,
        Err(err) => {
            report_switch_error("Switch engine frame failed", &err);
            true
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn siglus_switch_engine_gamepad(
    host: *mut SwitchHost,
    button: u8,
    down: bool,
) {
    if let Some(host) = unsafe { host.as_mut() } {
        host.gamepad_button(button, down);
    }
}

/// Touch phases match the host's existing platform-neutral convention:
/// 0=begin, 1=move, 2=end. libnx performs sampling; the shared host performs
/// the mouse/script translation so touch scripts behave the same as desktop.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn siglus_switch_engine_touch(
    host: *mut SwitchHost,
    phase: i32,
    x: f64,
    y: f64,
) {
    if let Some(host) = unsafe { host.as_mut() } {
        host.touch(phase, x, y);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn siglus_switch_engine_destroy(host: *mut SwitchHost) {
    if !host.is_null() {
        drop(unsafe { Box::from_raw(host) });
    }
}
