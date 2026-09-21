//! Stateful AVG32 bytecode interpreter.
//!
//! The interpreter owns only deterministic VM state.  It emits requests for
//! presentation, audio, and input so a frontend can implement those facilities
//! without teaching the bytecode layer about a particular windowing toolkit.

use std::collections::BTreeMap;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::{Datelike, Timelike};
use encoding_rs::SHIFT_JIS;

use crate::ard::AreaMap;
use crate::scene::{Avg32SceneHeader, SceneValue, ValueKind, parse_scene_value};

/// AVG32 encodes PDT buffer numbers as a plain signed `int`; scripts pass
/// `-1` for "the other side of this copy" (an in-place edit of the source
/// buffer, or the source itself when `-1` shows up as the source) rather
/// than a literal 32nd+ buffer index. Resolve that sentinel here so callers
/// never see the raw negative value.
fn pdt_index(value: i32, other_side: u32) -> u32 {
    if value < 0 { other_side } else { value as u32 }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Avg32Program {
    bytes: Vec<u8>,
    pub header: Avg32SceneHeader,
}

impl Avg32Program {
    pub fn parse(bytes: Vec<u8>) -> Result<Self> {
        let header = Avg32SceneHeader::parse(&bytes)?;
        Ok(Self { bytes, header })
    }

    pub fn code(&self) -> &[u8] {
        &self.bytes[self.header.code_offset..]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    None,
    Advance,
    Skip,
    Choice(usize),
    Pointer { x: i32, y: i32, button: i32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioKind {
    Bgm { looped: bool },
    Wave { looped: bool, channel: Option<i32> },
    Voice { wait: bool },
    Effect,
    Movie { looped: bool, wait: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmAction {
    Text(String),
    LineBreak,
    ClearText {
        hide_window: bool,
    },
    SetFontSize {
        doubled: bool,
    },
    SetFontColor(i32),
    Fade {
        pattern: Option<i32>,
        color: Option<[i32; 3]>,
        microseconds: Option<u32>,
    },
    WaitForInput {
        clears_text: bool,
    },
    WaitForPointer,
    LoadArea {
        cursor: String,
        definition: String,
    },
    Wait {
        microseconds: u32,
        cancellable_flag: Option<u32>,
    },
    LoadGraphic {
        name: String,
        target: u32,
        effect: Option<i32>,
    },
    CompositeGraphic {
        base: GraphicSource,
        effect: Option<i32>,
        layers: Vec<CompositeLayer>,
    },
    ClearGraphicBuffers,
    BufferCopy {
        source: u32,
        destination: u32,
        source_rect: [i32; 4],
        destination_xy: [i32; 2],
        masked: bool,
        color_key: Option<[i32; 3]>,
    },
    BufferFill {
        buffer: u32,
        rect: [i32; 4],
        color: [i32; 3],
    },
    BufferOutline {
        buffer: u32,
        rect: [i32; 4],
        color: [i32; 3],
    },
    BufferInvert {
        buffer: u32,
        rect: [i32; 4],
    },
    BufferColorMask {
        buffer: u32,
        rect: [i32; 4],
        color: [i32; 3],
    },
    BufferFade {
        buffer: u32,
        rect: [i32; 4],
        color: [i32; 3],
        amount: i32,
    },
    BufferMonochrome {
        buffer: u32,
        rect: [i32; 4],
    },
    /// AVG32 `PDTMGR::Swap`: exchanges RGB pixel content between two equal
    /// sized regions of `source` and `destination` in place. Unlike
    /// `BufferCopy`, this never touches either buffer's mask/alpha plane.
    BufferSwap {
        source: u32,
        destination: u32,
        source_rect: [i32; 4],
        destination_xy: [i32; 2],
    },
    BufferStretchCopy {
        source: u32,
        destination: u32,
        source_rect: [i32; 4],
        destination_rect: [i32; 4],
    },
    BufferScroll {
        source: u32,
        destination: u32,
        rect: [i32; 4],
        amount: i32,
        down: bool,
    },
    DrawBufferText {
        buffer: u32,
        position: [i32; 2],
        color: [i32; 3],
        text: String,
    },
    EndingSequence {
        mode: u8,
        alignment: u8,
        position: i32,
        wait: i32,
        pixels_per_step: i32,
        cancellable_flag: Option<u32>,
        frames: Vec<(String, i32)>,
    },
    StartAnimation {
        name: String,
        scene: u32,
        scenes: Vec<u32>,
        multi: bool,
        voice_synced: bool,
    },
    StopAnimation {
        name: Option<String>,
        scene: Option<u32>,
        clear_all: bool,
    },
    SaveRequest {
        slot: u32,
    },
    LoadRequest {
        slot: u32,
    },
    SetWindowTitle(String),
    PlayAudio {
        kind: AudioKind,
        name: String,
    },
    StopAudio {
        kind: AudioKind,
        channel: Option<i32>,
    },
    Shake {
        pattern: i32,
    },
    ChangeScene {
        scene: u32,
        call: bool,
    },
    ReturnScene,
    Choice {
        index_variable: u32,
        items: Vec<String>,
    },
    End,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositeLayer {
    pub name: String,
    pub source_rect: Option<[i32; 4]>,
    pub destination: Option<[i32; 2]>,
    pub effect: Option<i32>,
    pub alpha: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphicSource {
    File(String),
    Buffer(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmStop {
    Yield(VmAction),
    Ended,
}

/// Values and bit flags have separate namespaces in AVG32 save data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VmFlags {
    values: BTreeMap<u32, i32>,
    bits: BTreeMap<u32, bool>,
    strings: BTreeMap<u32, String>,
}

impl VmFlags {
    pub fn value(&self, index: u32) -> i32 {
        self.values.get(&index).copied().unwrap_or_default()
    }

    pub fn set_value(&mut self, index: u32, value: i32) {
        self.values.insert(index, value);
    }

    pub fn bit(&self, index: u32) -> bool {
        self.bits.get(&index).copied().unwrap_or(false)
    }

    pub fn set_bit(&mut self, index: u32, value: bool) {
        self.bits.insert(index, value);
    }

    pub fn string(&self, index: u32) -> &str {
        self.strings.get(&index).map_or("", String::as_str)
    }

    pub fn set_string(&mut self, index: u32, value: impl Into<String>) {
        self.strings.insert(index, value.into());
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::new();
        encode_map_i32(&mut output, &self.values);
        encode_map_bool(&mut output, &self.bits);
        output.extend_from_slice(&(self.strings.len() as u32).to_le_bytes());
        for (index, value) in &self.strings {
            output.extend_from_slice(&index.to_le_bytes());
            output.extend_from_slice(&(value.len() as u32).to_le_bytes());
            output.extend_from_slice(value.as_bytes());
        }
        output
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = FlagReader { bytes, at: 0 };
        let values = reader.map_i32()?;
        let bits = reader.map_bool()?;
        let strings = reader.count()?;
        let mut strings_out = BTreeMap::new();
        for _ in 0..strings {
            let index = reader.u32()?;
            let length = reader.u32()? as usize;
            let value = std::str::from_utf8(reader.take(length)?)
                .context("avg32: invalid UTF-8 in save string")?
                .to_owned();
            strings_out.insert(index, value);
        }
        if reader.at != bytes.len() {
            bail!("avg32: trailing bytes in VM flags save");
        }
        Ok(Self {
            values,
            bits,
            strings: strings_out,
        })
    }
}

fn encode_map_i32(output: &mut Vec<u8>, values: &BTreeMap<u32, i32>) {
    output.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for (index, value) in values {
        output.extend_from_slice(&index.to_le_bytes());
        output.extend_from_slice(&value.to_le_bytes());
    }
}

fn encode_map_bool(output: &mut Vec<u8>, values: &BTreeMap<u32, bool>) {
    output.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for (index, value) in values {
        output.extend_from_slice(&index.to_le_bytes());
        output.push(u8::from(*value));
    }
}

struct FlagReader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> FlagReader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(length)
            .ok_or_else(|| anyhow::anyhow!("avg32: save offset overflows"))?;
        let bytes = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| anyhow::anyhow!("avg32: truncated VM flags save"))?;
        self.at = end;
        Ok(bytes)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(
            self.take(4)?.try_into().expect("four bytes"),
        ))
    }
    fn count(&mut self) -> Result<u32> {
        let count = self.u32()?;
        if count > 1_000_000 {
            bail!("avg32: unreasonable save map count");
        }
        Ok(count)
    }
    fn map_i32(&mut self) -> Result<BTreeMap<u32, i32>> {
        let mut out = BTreeMap::new();
        for _ in 0..self.count()? {
            let index = self.u32()?;
            let value = self.i32()?;
            out.insert(index, value);
        }
        Ok(out)
    }
    fn map_bool(&mut self) -> Result<BTreeMap<u32, bool>> {
        let mut out = BTreeMap::new();
        for _ in 0..self.count()? {
            let index = self.u32()?;
            let value = match self.take(1)?[0] {
                0 => false,
                1 => true,
                _ => bail!("avg32: invalid boolean in VM flags save"),
            };
            out.insert(index, value);
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct Avg32Vm {
    program: Avg32Program,
    pc: usize,
    last_opcode_pc: usize,
    flags: VmFlags,
    call_stack: Vec<usize>,
    waiting: Option<VmAction>,
    wait_started: Option<Instant>,
    /// AVG32 17N+ prefixes every text packet with an ignored source offset.
    /// AIR reports runtime version 3217, so its FE/FF packets use this layout.
    extended_text: bool,
    popup_disabled: bool,
    menu_enabled: BTreeMap<i32, bool>,
    volumes: [i32; 4],
    muted: [bool; 4],
    novel_mode: bool,
    pointer: Option<(i32, i32, i32)>,
    pointer_wait_destinations: Option<[u32; 3]>,
    cursor_visible: bool,
    area_cursor: Option<String>,
    area_definition: Option<String>,
    area_map: Option<AreaMap>,
    disabled_areas: BTreeMap<i32, bool>,
    message_background: [i32; 4],
    message_position: [i32; 2],
    message_size: [i32; 2],
    message_font_size: [i32; 2],
    message_style: i32,
    window_move: i32,
    window_clear_box: i32,
    message_color: i32,
    message_cancel: i32,
    message_shadow: i32,
    message_shadow_color: i32,
    selection_cancel: i32,
    skip_enabled: bool,
    ended: bool,
}

impl Avg32Vm {
    pub fn new(program: Avg32Program) -> Self {
        Self {
            pc: 0,
            last_opcode_pc: 0,
            program,
            flags: VmFlags::default(),
            call_stack: Vec::new(),
            waiting: None,
            wait_started: None,
            extended_text: false,
            popup_disabled: false,
            menu_enabled: BTreeMap::new(),
            volumes: [100; 4],
            muted: [false; 4],
            novel_mode: false,
            pointer: None,
            pointer_wait_destinations: None,
            cursor_visible: true,
            area_cursor: None,
            area_definition: None,
            area_map: None,
            disabled_areas: BTreeMap::new(),
            message_background: [0, 0, 0, 0],
            message_position: [0, 0],
            message_size: [0, 0],
            message_font_size: [0, 0],
            message_style: 0,
            window_move: 0,
            window_clear_box: 0,
            message_color: 0,
            message_cancel: 0,
            message_shadow: 0,
            message_shadow_color: 0,
            selection_cancel: 0,
            skip_enabled: true,
            ended: false,
        }
    }

    /// Select the AVG32 17N+ text packet format used by AIR and later titles.
    pub fn with_extended_text(mut self) -> Self {
        self.extended_text = true;
        self
    }

    pub fn novel_mode(&self) -> bool {
        self.novel_mode
    }

    pub fn flags(&self) -> &VmFlags {
        &self.flags
    }

    pub fn flags_mut(&mut self) -> &mut VmFlags {
        &mut self.flags
    }

    pub fn replace_flags(&mut self, flags: VmFlags) {
        self.flags = flags;
    }

    pub fn set_area_map(&mut self, map: AreaMap) {
        self.area_map = Some(map);
    }

    pub fn pc(&self) -> usize {
        self.pc
    }

    pub fn last_opcode_pc(&self) -> usize {
        self.last_opcode_pc
    }

    pub fn set_pc(&mut self, pc: usize) -> Result<()> {
        if pc >= self.code().len() {
            bail!("avg32: saved PC {pc:#x} is outside scene bytecode");
        }
        self.pc = pc;
        self.waiting = None;
        self.wait_started = None;
        Ok(())
    }

    /// Runs until the VM needs external work or reaches its instruction limit.
    /// `max_instructions` is a hard guard against a corrupt script's endless
    /// loop; ordinary jumps and calls are otherwise unrestricted.
    pub fn run(&mut self, input: Input, max_instructions: usize) -> Result<VmStop> {
        if self.ended {
            return Ok(VmStop::Ended);
        }
        if let Input::Pointer { x, y, button } = input {
            self.pointer = Some((x, y, button));
            if let Some([x_destination, y_destination, button_destination]) =
                self.pointer_wait_destinations.take()
            {
                self.flags.set_value(x_destination, x);
                self.flags.set_value(y_destination, y);
                self.flags.set_value(button_destination, button);
            }
        }
        if let Some(waiting) = self.waiting.take() {
            if let VmAction::Wait {
                microseconds,
                cancellable_flag,
            } = &waiting
            {
                let elapsed = self
                    .wait_started
                    .map(|start| start.elapsed().as_micros())
                    .unwrap_or(0);
                if matches!(input, Input::Advance | Input::Skip) {
                    if let Some(flag) = cancellable_flag {
                        self.flags.set_value(*flag, 1);
                    }
                } else if elapsed < u128::from(*microseconds) {
                    self.waiting = Some(waiting.clone());
                    return Ok(VmStop::Yield(waiting));
                } else if let Some(flag) = cancellable_flag {
                    self.flags.set_value(*flag, 0);
                }
                self.wait_started = None;
            } else {
                let satisfied = match &waiting {
                    VmAction::WaitForPointer => matches!(input, Input::Pointer { .. }),
                    VmAction::Choice { .. } => matches!(input, Input::Choice(_)),
                    _ => matches!(input, Input::Advance | Input::Skip),
                };
                if !satisfied {
                    self.waiting = Some(waiting.clone());
                    return Ok(VmStop::Yield(waiting));
                }
                if let (VmAction::Choice { index_variable, .. }, Input::Choice(choice)) =
                    (&waiting, input)
                {
                    self.flags.set_value(*index_variable, choice as i32 + 1);
                }
            }
        }

        for _ in 0..max_instructions {
            let opcode_at = self.pc;
            let opcode = self.byte()?;
            self.last_opcode_pc = opcode_at;
            let action = match opcode {
                0x01 => Some(VmAction::WaitForInput { clears_text: true }),
                0x02 => {
                    self.byte()?; // AVG32 retains indentation mode; layout owns it.
                    Some(VmAction::LineBreak)
                }
                0x03 => Some(VmAction::WaitForInput { clears_text: false }),
                0x04 => self.text_window()?,
                0x05 => Some(VmAction::SetFontSize {
                    doubled: self.value()? == 2,
                }),
                0x06 => None, // Defined no-op in all known AVG32 revisions.
                0x08 => {
                    match self.byte()? {
                        1 => {}
                        0x10 | 0x11 => {
                            self.value()?;
                        }
                        subcommand => {
                            bail!("avg32: unsupported control subcommand {subcommand:#04x}")
                        }
                    }
                    None
                }
                0x0b => self.graphics()?,
                0x0c => self.animation()?,
                0x0e => self.audio()?,
                0x10 => self.inline_value_text()?,
                0x13 => self.fade()?,
                0x15 => {
                    let condition = self.condition()?;
                    let target = self.offset()?;
                    if !condition {
                        self.jump(target)?;
                    }
                    None
                }
                0x16 => self.change_scene()?,
                0x17 => self.shake()?,
                0x18 => {
                    self.require_subcommand(1, "font colour")?;
                    Some(VmAction::SetFontColor(self.value()?))
                }
                0x19 => self.wait()?,
                0x1a => None, // Defined no-op.
                0x1b => {
                    let target = self.offset()?;
                    self.call_stack.push(self.pc);
                    self.jump(target)?;
                    None
                }
                0x1c => {
                    let target = self.offset()?;
                    self.jump(target)?;
                    None
                }
                0x1d | 0x1e => self.table_jump(opcode == 0x1d)?,
                0x20 => self.return_from_call()?,
                0x22..=0x29 => None, // Reserved no-payload command family.
                0x2c..=0x31 => {
                    self.legacy_control(opcode)?;
                    None
                }
                0x37 | 0x39 | 0x3b..=0x43 | 0x49..=0x51 | 0x56 | 0x57 => {
                    self.flags_operation(opcode)?;
                    None
                }
                0x58 => self.choice()?,
                0x59 => {
                    self.strings()?;
                    None
                }
                0x5b => {
                    self.set_sequence()?;
                    None
                }
                0x5c => {
                    self.set_multi()?;
                    None
                }
                0x5d => {
                    self.copy_multi()?;
                    None
                }
                0x5e => {
                    self.system_value()?;
                    None
                }
                0x5f => {
                    self.multi_add()?;
                    None
                }
                0x60 => self.system_command()?,
                0x61 => {
                    self.name_control()?;
                    None
                }
                0x63 => self.buffer_get_put()?,
                0x64 => self.buffer_region()?,
                0x65 => None, // Reserved by AVG32; known revisions carry no payload.
                0x67 => self.buffer_copy()?,
                0x68 => self.graphics_special()?,
                0x69 => self.buffer_scroll()?,
                0x6a => self.ending_control()?,
                0x66 => self.draw_buffer_text()?,
                0x6d => self.mouse_control()?,
                0x6c => self.area_control()?,
                0x6e => {
                    self.cg_mode_control()?;
                    None
                }
                0x6f => {
                    for _ in 0..4 {
                        self.value()?;
                    }
                    None
                }
                0x70 => {
                    self.message_window_config()?;
                    None
                }
                0x72 => {
                    self.message_window_position()?;
                    None
                }
                0x73 => {
                    self.message_window_values()?;
                    None
                }
                0x74 => {
                    self.popup_menu()?;
                    None
                }
                0x75 => {
                    self.volume()?;
                    None
                }
                0x76 => {
                    self.novel_mode_control()?;
                    None
                }
                0x7f => {
                    self.value()?;
                    None
                }
                0xfe | 0xff => Some(VmAction::Text(self.text_packet()?)),
                0x00 => {
                    self.ended = true;
                    Some(VmAction::End)
                }
                _ => bail!(
                    "avg32: unsupported opcode {opcode:#04x} at scene bytecode offset {opcode_at:#x}"
                ),
            };
            if let Some(action) = action {
                if matches!(
                    action,
                    VmAction::WaitForInput { .. }
                        | VmAction::WaitForPointer
                        | VmAction::Wait { .. }
                        | VmAction::Choice { .. }
                ) {
                    self.wait_started = matches!(action, VmAction::Wait { .. }).then(Instant::now);
                    self.waiting = Some(action.clone());
                }
                if matches!(action, VmAction::End) {
                    self.ended = true;
                }
                return Ok(VmStop::Yield(action));
            }
        }
        bail!(
            "avg32: instruction limit ({max_instructions}) reached at pc {:#x}",
            self.pc
        )
    }

    fn text_window(&mut self) -> Result<Option<VmAction>> {
        let subcommand = self.byte()?;
        Ok(match subcommand {
            1 | 2 => Some(VmAction::ClearText { hide_window: true }),
            3 | 5 => Some(VmAction::ClearText { hide_window: false }),
            4 => Some(VmAction::WaitForInput { clears_text: true }),
            _ => bail!("avg32: unsupported text-window subcommand {subcommand:#04x}"),
        })
    }

    fn graphics(&mut self) -> Result<Option<VmAction>> {
        let subcommand = self.byte()?;
        match subcommand {
            0x01 | 0x03 | 0x05 => {
                let name = self.scene_text()?;
                let effect = self.value()?;
                Ok(Some(VmAction::LoadGraphic {
                    name,
                    target: 0,
                    effect: Some(effect),
                }))
            }
            0x09 | 0x10 | 0x54 => {
                let name = self.scene_text()?;
                let target = self.value()? as u32;
                Ok(Some(VmAction::LoadGraphic {
                    name,
                    target,
                    effect: None,
                }))
            }
            0x02 | 0x04 | 0x06 => {
                let name = self.scene_text()?;
                // Geometry and transition parameters (the backend applies the effect).
                for _ in 0..15 {
                    self.value()?;
                }
                Ok(Some(VmAction::LoadGraphic {
                    name,
                    target: 0,
                    effect: None,
                }))
            }
            0x08 | 0x13 => Ok(None),
            0x11 => {
                self.scene_text()?;
                Ok(None)
            }
            0x22 => {
                // Every layer is composited onto the fixed working buffer
                // (PDT#1), never a script-chosen target: the value that
                // follows the base filename is a `GAMEEXE.INI` effect-table
                // index (`SnrPDT_Effect` presents PDT1 -> the display using
                // that preset), not a buffer number.
                let count = usize::from(self.byte()?);
                let base = GraphicSource::File(self.scene_text()?);
                let effect = self.value()?;
                let mut layers = Vec::with_capacity(count);
                for _ in 0..count {
                    layers.push(self.composite_child()?);
                }
                Ok(Some(VmAction::CompositeGraphic {
                    base,
                    effect: Some(effect),
                    layers,
                }))
            }
            0x24 => {
                let count = usize::from(self.byte()?);
                let base = GraphicSource::Buffer(self.value()? as u32);
                let effect = self.value()?;
                let mut layers = Vec::with_capacity(count);
                for _ in 0..count {
                    layers.push(self.composite_child()?);
                }
                Ok(Some(VmAction::CompositeGraphic {
                    base,
                    effect: Some(effect),
                    layers,
                }))
            }
            0x30 => Ok(Some(VmAction::ClearGraphicBuffers)),
            0x31 | 0x32 | 0x33 | 0x52 => {
                self.value()?;
                Ok(None)
            }
            0x50 => Ok(None),
            _ => bail!("avg32: unsupported graphics subcommand {subcommand:#04x}"),
        }
    }

    fn composite_child(&mut self) -> Result<CompositeLayer> {
        let method = self.byte()?;
        let name = self.scene_text()?;
        let (source_rect, destination, effect, alpha) = match method {
            1 => (None, None, None, None),
            2 => (None, None, Some(self.value()?), None),
            3 => (
                Some(self.rect()?),
                Some([self.value()?, self.value()?]),
                None,
                None,
            ),
            4 => (
                Some(self.rect()?),
                Some([self.value()?, self.value()?]),
                None,
                Some(self.value()?),
            ),
            _ => bail!("avg32: unsupported graphics composite method {method:#04x}"),
        };
        Ok(CompositeLayer {
            name,
            source_rect,
            destination,
            effect,
            alpha,
        })
    }

    fn animation(&mut self) -> Result<Option<VmAction>> {
        let subcommand = self.byte()?;
        match subcommand {
            0x10 => {
                let name = self.scene_text()?;
                let scene = self.value()? as u32;
                Ok(Some(VmAction::StartAnimation {
                    name,
                    scene,
                    scenes: vec![scene],
                    multi: false,
                    voice_synced: false,
                }))
            }
            0x18 => {
                let name = self.scene_text()?;
                self.value()?;
                self.value()?;
                Ok(Some(VmAction::StartAnimation {
                    name,
                    scene: 0,
                    scenes: vec![0],
                    multi: false,
                    voice_synced: false,
                }))
            }
            0x12 | 0x30 => {
                let name = self.scene_text()?;
                let mut scenes = Vec::new();
                while self.peek()? != 0 {
                    scenes.push(self.value()? as u32);
                }
                self.byte()?;
                let scene = scenes.first().copied().unwrap_or(0);
                Ok(Some(VmAction::StartAnimation {
                    name,
                    scene,
                    scenes,
                    multi: true,
                    voice_synced: subcommand == 0x30,
                }))
            }
            0x13 => Ok(None),
            0x16 => {
                let name = self.scene_text()?;
                let scene = self.value()? as u32;
                self.value()?;
                self.value()?;
                Ok(Some(VmAction::StartAnimation {
                    name,
                    scene,
                    scenes: vec![scene],
                    multi: false,
                    voice_synced: false,
                }))
            }
            0x20 | 0x24 => {
                let name = self.scene_text()?;
                let scene = if self.peek()? == 0 {
                    None
                } else {
                    Some(self.value()? as u32)
                };
                while self.peek()? != 0 {
                    self.value()?;
                }
                self.byte()?;
                Ok(Some(VmAction::StopAnimation {
                    name: Some(name),
                    scene,
                    clear_all: false,
                }))
            }
            0x21 | 0x25 => Ok(Some(VmAction::StopAnimation {
                name: None,
                scene: None,
                clear_all: true,
            })),
            _ => bail!("avg32: unsupported animation subcommand {subcommand:#04x}"),
        }
    }

    fn audio(&mut self) -> Result<Option<VmAction>> {
        let subcommand = self.byte()?;
        let action = match subcommand {
            0x01..=0x03 => VmAction::PlayAudio {
                kind: AudioKind::Bgm {
                    looped: subcommand != 0x03,
                },
                name: self.scene_text()?,
            },
            0x05..=0x07 => {
                let name = self.scene_text()?;
                self.value()?;
                VmAction::PlayAudio {
                    kind: AudioKind::Bgm {
                        looped: subcommand != 0x07,
                    },
                    name,
                }
            }
            0x10 => {
                self.value()?;
                VmAction::StopAudio {
                    kind: AudioKind::Bgm { looped: false },
                    channel: None,
                }
            }
            0x11 | 0x12 | 0x16 => VmAction::StopAudio {
                kind: AudioKind::Bgm { looped: false },
                channel: None,
            },
            0x20 | 0x21 => VmAction::PlayAudio {
                kind: AudioKind::Voice {
                    wait: subcommand == 0x20,
                },
                name: self.value()?.to_string(),
            },
            0x22 => {
                let voice = self.value()?;
                self.value()?;
                VmAction::PlayAudio {
                    kind: AudioKind::Voice { wait: false },
                    name: voice.to_string(),
                }
            }
            0x30..=0x35 => {
                let name = self.scene_text()?;
                let has_channel = matches!(subcommand, 0x31 | 0x33 | 0x35);
                let channel = has_channel.then(|| self.value()).transpose()?;
                VmAction::PlayAudio {
                    kind: AudioKind::Wave {
                        looped: matches!(subcommand, 0x32 | 0x33),
                        channel,
                    },
                    name,
                }
            }
            0x36 | 0x38 => VmAction::StopAudio {
                kind: AudioKind::Wave {
                    looped: false,
                    channel: None,
                },
                channel: None,
            },
            0x37 | 0x39 => {
                let channel = self.value()?;
                VmAction::StopAudio {
                    kind: AudioKind::Wave {
                        looped: false,
                        channel: Some(channel),
                    },
                    channel: Some(channel),
                }
            }
            0x40 | 0x44 => VmAction::PlayAudio {
                kind: AudioKind::Effect,
                name: self.value()?.to_string(),
            },
            0x50..=0x55 => {
                let name = self.scene_text()?;
                if subcommand >= 0x54 {
                    self.scene_text()?;
                }
                for _ in 0..4 {
                    self.value()?;
                }
                VmAction::PlayAudio {
                    kind: AudioKind::Movie {
                        looped: subcommand == 0x51,
                        wait: matches!(subcommand, 0x52..=0x55),
                    },
                    name,
                }
            }
            0x60 => return Ok(None),
            _ => bail!("avg32: unsupported audio subcommand {subcommand:#04x}"),
        };
        Ok(Some(action))
    }

    fn wait(&mut self) -> Result<Option<VmAction>> {
        let subcommand = self.byte()?;
        Ok(match subcommand {
            1 | 4 => Some(VmAction::Wait {
                microseconds: self.value()? as u32,
                cancellable_flag: None,
            }),
            2 | 5 => {
                let microseconds = self.value()? as u32;
                let flag = self.value()? as u32;
                Some(VmAction::Wait {
                    microseconds,
                    cancellable_flag: Some(flag),
                })
            }
            3 | 0x10..=0x13 => None,
            6 => {
                let index = self.raw_value_index()?;
                self.flags.set_value(index, 0);
                None
            }
            _ => bail!("avg32: unsupported wait subcommand {subcommand:#04x}"),
        })
    }

    fn inline_value_text(&mut self) -> Result<Option<VmAction>> {
        let text = match self.byte()? {
            1 => {
                let index = self.raw_value_index()?;
                self.flags.value(index).to_string()
            }
            2 => {
                let index = self.raw_value_index()?;
                let width = self.value()? as usize;
                format!("{:0width$}", self.flags.value(index))
            }
            3 => {
                let index = self.raw_value_index()?;
                self.flags.string(index).to_owned()
            }
            subcommand => bail!("avg32: unsupported inline-text subcommand {subcommand:#04x}"),
        };
        Ok(Some(VmAction::Text(text)))
    }

    fn fade(&mut self) -> Result<Option<VmAction>> {
        let subcommand = self.byte()?;
        let action = match subcommand {
            1 => VmAction::Fade {
                pattern: Some(self.value()?),
                color: None,
                microseconds: None,
            },
            2 => VmAction::Fade {
                pattern: Some(self.value()?),
                color: None,
                microseconds: Some(self.value()? as u32),
            },
            3 => VmAction::Fade {
                pattern: None,
                color: Some([self.value()?, self.value()?, self.value()?]),
                microseconds: None,
            },
            4 => VmAction::Fade {
                pattern: None,
                color: Some([self.value()?, self.value()?, self.value()?]),
                microseconds: Some(self.value()? as u32),
            },
            0x10 => VmAction::Fade {
                pattern: Some(self.value()?),
                color: None,
                microseconds: Some(0),
            },
            0x11 => VmAction::Fade {
                pattern: None,
                color: Some([self.value()?, self.value()?, self.value()?]),
                microseconds: Some(0),
            },
            _ => bail!("avg32: unsupported fade subcommand {subcommand:#04x}"),
        };
        Ok(Some(action))
    }

    fn change_scene(&mut self) -> Result<Option<VmAction>> {
        let subcommand = self.byte()?;
        let scene = self.value()? as u32;
        Ok(Some(VmAction::ChangeScene {
            scene,
            call: subcommand != 1,
        }))
    }

    fn condition(&mut self) -> Result<bool> {
        if self.byte()? != 0x28 {
            bail!("avg32: condition expression does not begin with '('");
        }
        self.condition_group()
    }

    fn condition_group(&mut self) -> Result<bool> {
        let mut value = self.condition_operand()?;
        loop {
            match self.byte()? {
                0x29 => return Ok(value),
                0x26 => value &= self.condition_operand()?,
                0x27 => value |= self.condition_operand()?,
                0x58 => {
                    // Choice attributes annotate a preceding branch; they don't
                    // alter the truth value used by opcode 0x15.
                    match self.byte()? {
                        0x20 | 0x22 => {
                            self.value()?;
                        }
                        0x21 => {}
                        attribute => {
                            bail!("avg32: unsupported condition attribute {attribute:#04x}")
                        }
                    }
                }
                code => bail!("avg32: expected condition operator or ')', got {code:#04x}"),
            }
        }
    }

    fn condition_operand(&mut self) -> Result<bool> {
        let code = self.byte()?;
        if code == 0x28 {
            return self.condition_group();
        }
        let left = self.value()?;
        let right = self.value()?;
        let value = match code {
            0x36 => self.flags.bit(left as u32) != (right != 0),
            0x37 => self.flags.bit(left as u32) == (right != 0),
            0x38 => self.flags.bit(left as u32) != self.flags.bit(right as u32),
            0x39 => self.flags.bit(left as u32) == self.flags.bit(right as u32),
            0x3a => self.flags.value(left as u32) != right,
            0x3b => self.flags.value(left as u32) == right,
            0x41 | 0x42 => self.flags.value(left as u32) & right != 0,
            0x43 => self.flags.value(left as u32) ^ right != 0,
            0x44 => self.flags.value(left as u32) > right,
            0x45 => self.flags.value(left as u32) < right,
            0x46 => self.flags.value(left as u32) >= right,
            0x47 => self.flags.value(left as u32) <= right,
            0x48 => self.flags.value(left as u32) != self.flags.value(right as u32),
            0x49 => self.flags.value(left as u32) == self.flags.value(right as u32),
            0x4f | 0x50 => self.flags.value(left as u32) & self.flags.value(right as u32) != 0,
            0x51 => self.flags.value(left as u32) ^ self.flags.value(right as u32) != 0,
            0x52 => self.flags.value(left as u32) > self.flags.value(right as u32),
            0x53 => self.flags.value(left as u32) < self.flags.value(right as u32),
            0x54 => self.flags.value(left as u32) >= self.flags.value(right as u32),
            0x55 => self.flags.value(left as u32) <= self.flags.value(right as u32),
            _ => bail!("avg32: unsupported condition opcode {code:#04x}"),
        };
        Ok(value)
    }

    fn shake(&mut self) -> Result<Option<VmAction>> {
        self.require_subcommand(1, "screen shake")?;
        Ok(Some(VmAction::Shake {
            pattern: self.value()?,
        }))
    }

    fn table_jump(&mut self, call: bool) -> Result<Option<VmAction>> {
        let count = usize::from(self.byte()?);
        let selector = self.value()?;
        let mut target = None;
        for item in 1..=count {
            let offset = self.offset()?;
            if item == selector as usize {
                target = Some(offset);
            }
        }
        if let Some(target) = target {
            if call {
                self.call_stack.push(self.pc);
            }
            self.jump(target)?;
        }
        Ok(None)
    }

    fn return_from_call(&mut self) -> Result<Option<VmAction>> {
        match self.byte()? {
            1 => {
                if let Some(target) = self.call_stack.pop() {
                    self.pc = target;
                }
            }
            2 => return Ok(Some(VmAction::ReturnScene)),
            3 => {
                self.call_stack.pop();
            }
            6 => self.call_stack.clear(),
            subcommand => bail!("avg32: unsupported return subcommand {subcommand:#04x}"),
        }
        Ok(None)
    }

    fn flags_operation(&mut self, opcode: u8) -> Result<()> {
        let index = self.raw_value_index()?;
        if opcode == 0x56 {
            // The original command writes a random bit.  The deterministic core
            // chooses false; a frontend can seed this state before executing.
            self.flags.set_bit(index, false);
            return Ok(());
        }
        let value = self.value()?;
        match opcode {
            0x37 => self.flags.set_bit(index, value != 0),
            0x39 => self.flags.set_bit(index, self.flags.bit(value as u32)),
            0x3b => self.flags.set_value(index, value),
            0x3c => self
                .flags
                .set_value(index, self.flags.value(index).wrapping_add(value)),
            0x3d => self
                .flags
                .set_value(index, self.flags.value(index).wrapping_sub(value)),
            0x3e => self
                .flags
                .set_value(index, self.flags.value(index).wrapping_mul(value)),
            0x3f => {
                if value != 0 {
                    self.flags.set_value(index, self.flags.value(index) / value)
                }
            }
            0x40 => {
                if value != 0 {
                    self.flags.set_value(index, self.flags.value(index) % value)
                }
            }
            0x41 => self.flags.set_value(index, self.flags.value(index) & value),
            0x42 => self.flags.set_value(index, self.flags.value(index) | value),
            0x43 => self.flags.set_value(index, self.flags.value(index) ^ value),
            0x49 => self.flags.set_value(index, self.flags.value(value as u32)),
            0x4a => self.flags.set_value(
                index,
                self.flags
                    .value(index)
                    .wrapping_add(self.flags.value(value as u32)),
            ),
            0x4b => self.flags.set_value(
                index,
                self.flags
                    .value(index)
                    .wrapping_sub(self.flags.value(value as u32)),
            ),
            0x4c => self.flags.set_value(
                index,
                self.flags
                    .value(index)
                    .wrapping_mul(self.flags.value(value as u32)),
            ),
            0x4d => {
                let divisor = self.flags.value(value as u32);
                if divisor != 0 {
                    self.flags
                        .set_value(index, self.flags.value(index) / divisor)
                }
            }
            0x4e => {
                let divisor = self.flags.value(value as u32);
                if divisor != 0 {
                    self.flags
                        .set_value(index, self.flags.value(index) % divisor)
                }
            }
            0x4f => self.flags.set_value(
                index,
                self.flags.value(index) & self.flags.value(value as u32),
            ),
            0x50 => self.flags.set_value(
                index,
                self.flags.value(index) | self.flags.value(value as u32),
            ),
            0x51 => self.flags.set_value(
                index,
                self.flags.value(index) ^ self.flags.value(value as u32),
            ),
            0x57 => self.flags.set_bit(index, value != 0),
            _ => unreachable!("opcode filter above is exhaustive"),
        }
        Ok(())
    }

    fn choice(&mut self) -> Result<Option<VmAction>> {
        let subcommand = self.byte()?;
        match subcommand {
            1 | 2 => {
                let index_variable = self.raw_value_index()?;
                let marker = self.byte()?;
                if marker != 0x22 {
                    return Ok(None);
                }
                self.byte()?; // version-specific pad byte
                let mut items = Vec::new();
                loop {
                    let item = self.formatted_text()?;
                    items.push(item);
                    if self.byte()? == 0x23 {
                        break;
                    }
                    self.pc -= 1;
                }
                Ok(Some(VmAction::Choice {
                    index_variable,
                    items,
                }))
            }
            4 => {
                self.value()?;
                Ok(None)
            }
            _ => bail!("avg32: unsupported choice subcommand {subcommand:#04x}"),
        }
    }

    fn multi_add(&mut self) -> Result<()> {
        match self.byte()? {
            1 => {
                let destination = self.raw_value_index()?;
                let mut total = 0i32;
                loop {
                    match self.byte()? {
                        0 => break,
                        1 => {
                            let index = self.raw_value_index()?;
                            total = total.wrapping_add(self.flags.value(index));
                        }
                        2 => {
                            let start = self.raw_value_index()?;
                            let end = self.raw_value_index()?;
                            for index in start..=end {
                                total = total.wrapping_add(self.flags.value(index));
                            }
                        }
                        0x11 => {
                            let index = self.raw_value_index()?;
                            total += i32::from(self.flags.bit(index));
                        }
                        0x12 => {
                            let start = self.raw_value_index()?;
                            let end = self.raw_value_index()?;
                            for index in start..=end {
                                total += i32::from(self.flags.bit(index));
                            }
                        }
                        code => bail!("avg32: unsupported multi-add operand {code:#04x}"),
                    }
                }
                self.flags.set_value(destination, total);
            }
            0x10 => {
                let destination = self.value()? as u32;
                let numerator = self.value()?;
                let denominator = self.value()?;
                let percentage = if denominator == 0 {
                    0
                } else {
                    numerator.saturating_mul(100) / denominator
                }
                .clamp(0, 100);
                self.flags.set_value(destination, percentage);
            }
            0x20 => {
                let count = usize::from(self.byte()?);
                let mut destination = self.value()? as u32;
                let source = self.value()? as u32;
                for _ in 0..count {
                    let offset = self.value()?;
                    let value = offset
                        .try_into()
                        .ok()
                        .map(|offset: u32| self.flags.value(source.wrapping_add(offset)))
                        .unwrap_or_default();
                    self.flags.set_value(destination, value);
                    destination = destination.wrapping_add(1);
                }
            }
            subcommand => bail!("avg32: unsupported multi-add subcommand {subcommand:#04x}"),
        }
        Ok(())
    }

    fn set_multi(&mut self) -> Result<()> {
        let subcommand = self.byte()?;
        let start = self.raw_value_index()?;
        let end = self.raw_value_index()?;
        let value = self.value()?;
        match subcommand {
            1 => {
                for index in start..=end {
                    self.flags.set_value(index, value);
                }
            }
            2 => {
                for index in start..=end {
                    self.flags.set_bit(index, value != 0);
                }
            }
            _ => bail!("avg32: unsupported set-multi subcommand {subcommand:#04x}"),
        }
        Ok(())
    }

    fn set_sequence(&mut self) -> Result<()> {
        let subcommand = self.byte()?;
        let mut index = self.raw_value_index()?;
        match subcommand {
            1 => {
                while self.peek()? != 0 {
                    let value = self.value()?;
                    self.flags.set_value(index, value);
                    index += 1;
                }
            }
            2 => {
                while self.peek()? != 0 {
                    let value = self.value()?;
                    self.flags.set_bit(index, value != 0);
                    index += 1;
                }
            }
            _ => bail!("avg32: unsupported set-sequence subcommand {subcommand:#04x}"),
        }
        self.byte()?;
        Ok(())
    }

    fn copy_multi(&mut self) -> Result<()> {
        let subcommand = self.byte()?;
        let source = self.raw_value_index()?;
        let mut destination = self.raw_value_index()?;
        let count = self.value()?.max(0) as u32;
        match subcommand {
            1 => {
                for offset in 0..count {
                    self.flags
                        .set_value(destination + offset, self.flags.value(source + offset));
                }
            }
            2 => {
                for offset in 0..count {
                    self.flags
                        .set_bit(destination + offset, self.flags.bit(source + offset));
                }
            }
            _ => bail!("avg32: unsupported copy-multi subcommand {subcommand:#04x}"),
        }
        destination += count;
        let _ = destination;
        Ok(())
    }

    fn system_value(&mut self) -> Result<()> {
        let subcommand = self.byte()?;
        let destination = self.raw_value_index()?;
        let value = match subcommand {
            1 => chrono::Local::now().year(),
            2 => chrono::Local::now().month() as i32,
            3 => chrono::Local::now().day() as i32,
            4 => chrono::Local::now().hour() as i32,
            0x10 => 0, // Runtime supplies the active scene number through save metadata.
            _ => bail!("avg32: unsupported system-value subcommand {subcommand:#04x}"),
        };
        self.flags.set_value(destination, value);
        Ok(())
    }

    fn system_command(&mut self) -> Result<Option<VmAction>> {
        match self.byte()? {
            2 => Ok(Some(VmAction::LoadRequest {
                slot: self.value()? as u32,
            })),
            3 => Ok(Some(VmAction::SaveRequest {
                slot: self.value()? as u32,
            })),
            4 => Ok(Some(VmAction::SetWindowTitle(self.formatted_text()?))),
            5 => Ok(None),
            0x20 => Ok(Some(VmAction::End)),
            0x30 => {
                self.value()?;
                self.raw_value_index()?;
                Ok(None)
            }
            0x31 => {
                self.value()?;
                let destination = self.raw_value_index()?;
                self.flags.set_value(destination, 0);
                Ok(None)
            }
            0x35 => {
                self.value()?;
                self.raw_value_index()?;
                Ok(None)
            }
            0x36 | 0x37 => {
                self.value()?;
                let destination = self.raw_value_index()?;
                self.flags.set_value(destination, 0);
                Ok(None)
            }
            subcommand => bail!("avg32: unsupported system subcommand {subcommand:#04x}"),
        }
    }

    fn popup_menu(&mut self) -> Result<()> {
        match self.byte()? {
            1 => {
                let destination = self.raw_value_index()?;
                self.flags
                    .set_value(destination, i32::from(self.popup_disabled));
            }
            2 => self.popup_disabled = self.value()? != 0,
            3 => {
                let item = self.value()?;
                let destination = self.raw_value_index()?;
                let enabled = self.menu_enabled.get(&item).copied().unwrap_or(true);
                self.flags.set_value(destination, i32::from(enabled));
            }
            4 => {
                let item = self.value()?;
                let enabled = self.value()? != 0;
                self.menu_enabled.insert(item, enabled);
            }
            subcommand => bail!("avg32: unsupported popup-menu subcommand {subcommand:#04x}"),
        }
        Ok(())
    }

    fn volume(&mut self) -> Result<()> {
        let subcommand = self.byte()?;
        let value = self.value()?;
        let channel = match subcommand & 0x0f {
            1..=4 => (subcommand & 0x0f) as usize - 1,
            _ => bail!("avg32: unsupported volume subcommand {subcommand:#04x}"),
        };
        match subcommand >> 4 {
            0 => {
                let destination = value as u32;
                self.flags.set_value(destination, self.volumes[channel]);
            }
            1 => self.volumes[channel] = value.clamp(0, 100),
            2 => self.muted[channel] = value != 0,
            _ => bail!("avg32: unsupported volume subcommand {subcommand:#04x}"),
        }
        Ok(())
    }

    fn novel_mode_control(&mut self) -> Result<()> {
        match self.byte()? {
            1 => self.novel_mode = self.value()? != 0,
            2 => {
                self.value()?;
            }
            4 | 5 => {}
            subcommand => bail!("avg32: unsupported novel-mode subcommand {subcommand:#04x}"),
        }
        Ok(())
    }

    fn message_window_config(&mut self) -> Result<()> {
        match self.byte()? {
            1 => {
                let destinations = [
                    self.raw_value_index()?,
                    self.raw_value_index()?,
                    self.raw_value_index()?,
                    self.raw_value_index()?,
                ];
                for (destination, value) in destinations.into_iter().zip(self.message_background) {
                    self.flags.set_value(destination, value);
                }
            }
            2 => {
                self.message_background =
                    [self.value()?, self.value()?, self.value()?, self.value()?];
            }
            3 => {
                let destination = self.raw_value_index()?;
                self.flags.set_value(destination, self.window_move);
            }
            4 => self.window_move = self.value()?,
            5 => {
                let destination = self.raw_value_index()?;
                self.flags.set_value(destination, self.window_clear_box);
            }
            6 => self.window_clear_box = self.value()?,
            0x10 => {
                let destination = self.raw_value_index()?;
                self.flags.set_value(destination, self.message_style);
            }
            0x11 => self.message_style = self.value()?,
            subcommand => {
                bail!("avg32: unsupported message-window config subcommand {subcommand:#04x}")
            }
        }
        Ok(())
    }

    fn message_window_position(&mut self) -> Result<()> {
        match self.byte()? {
            1 => {
                let x = self.raw_value_index()?;
                let y = self.raw_value_index()?;
                self.flags.set_value(x, self.message_position[0]);
                self.flags.set_value(y, self.message_position[1]);
            }
            2..=5 => {
                let x = self.raw_value_index()?;
                let y = self.raw_value_index()?;
                self.flags.set_value(x, 0);
                self.flags.set_value(y, 0);
            }
            0x11 => self.message_position = [self.value()?, self.value()?],
            0x12..=0x15 => {
                self.value()?;
                self.value()?;
            }
            subcommand => {
                bail!("avg32: unsupported message-window position subcommand {subcommand:#04x}")
            }
        }
        Ok(())
    }

    fn message_window_values(&mut self) -> Result<()> {
        let subcommand = self.byte()?;
        match subcommand {
            1 => self.read_pair_into(self.message_size)?,
            2 => self.message_size = [self.value()?, self.value()?],
            5 => self.read_pair_into(self.message_font_size)?,
            6 => self.message_font_size = [self.value()?, self.value()?],
            0x10 => self.read_value_into(self.message_color)?,
            0x11 => self.message_color = self.value()?,
            0x12 => self.read_value_into(self.message_cancel)?,
            0x13 => self.message_cancel = self.value()?,
            0x16 => self.read_value_into(self.message_shadow)?,
            0x17 => self.message_shadow = self.value()?,
            0x18 => self.read_value_into(self.message_shadow_color)?,
            0x19 => self.message_shadow_color = self.value()?,
            0x1a => self.read_value_into(self.selection_cancel)?,
            0x1b => self.selection_cancel = self.value()?,
            0x1c => self.read_value_into(i32::from(self.skip_enabled))?,
            0x1d => self.skip_enabled = self.value()? == 0,
            // AVG32 17N+ extension settings. They share the same one-value
            // get/set wire format even where the original UI has no effect.
            0x1e | 0x20 | 0x22 | 0x24 => {
                let destination = self.raw_value_index()?;
                self.flags.set_value(destination, 0);
            }
            0x1f | 0x21 | 0x23 | 0x25 => {
                self.value()?;
            }
            _ => bail!("avg32: unsupported message-window value subcommand {subcommand:#04x}"),
        }
        Ok(())
    }

    fn read_pair_into(&mut self, values: [i32; 2]) -> Result<()> {
        let first = self.raw_value_index()?;
        let second = self.raw_value_index()?;
        self.flags.set_value(first, values[0]);
        self.flags.set_value(second, values[1]);
        Ok(())
    }

    fn read_value_into(&mut self, value: i32) -> Result<()> {
        let destination = self.raw_value_index()?;
        self.flags.set_value(destination, value);
        Ok(())
    }

    fn mouse_control(&mut self) -> Result<Option<VmAction>> {
        let subcommand = self.byte()?;
        match subcommand {
            1 | 2 => {
                let x_destination = self.raw_value_index()?;
                let y_destination = self.raw_value_index()?;
                let button_destination = self.raw_value_index()?;
                if let Some((x, y, button)) = self.pointer.take() {
                    self.flags.set_value(x_destination, x);
                    self.flags.set_value(y_destination, y);
                    self.flags.set_value(button_destination, button);
                    Ok(None)
                } else {
                    // AVG32's event loop yields after both its blocking and
                    // polling forms.  In a run-to-yield interpreter, letting
                    // the polling form spin through its backward branch would
                    // starve the host UI, so retain its destinations and wake
                    // only when the frontend supplies a pointer event.
                    self.pointer_wait_destinations =
                        Some([x_destination, y_destination, button_destination]);
                    Ok(Some(VmAction::WaitForPointer))
                }
            }
            3 => {
                self.pointer = None;
                Ok(None)
            }
            0x20 => {
                self.cursor_visible = false;
                Ok(None)
            }
            0x21 => {
                self.cursor_visible = true;
                Ok(None)
            }
            subcommand => bail!("avg32: unsupported mouse-control subcommand {subcommand:#04x}"),
        }
    }

    fn area_control(&mut self) -> Result<Option<VmAction>> {
        let subcommand = self.byte()?;
        let action = match subcommand {
            2 => {
                let cursor = self.scene_text()?;
                let definition = self.scene_text()?;
                self.area_cursor = Some(cursor.clone());
                self.area_definition = Some(definition.clone());
                Some(VmAction::LoadArea { cursor, definition })
            }
            3 => {
                self.area_cursor = None;
                self.area_definition = None;
                self.area_map = None;
                self.disabled_areas.clear();
                None
            }
            4 | 5 => {
                let area_destination = self.raw_value_index()?;
                let button_destination = self.raw_value_index()?;
                // ARD hit-testing belongs to the input frontend.  Until it has
                // supplied a pointer hit, AVG32 reports no selected area.
                let button = self.pointer.map_or(0, |(_, _, button)| button);
                let area = self.area_at(
                    self.pointer.map_or(0, |(x, _, _)| x),
                    self.pointer.map_or(0, |(_, y, _)| y),
                );
                self.flags.set_value(area_destination, area);
                self.flags.set_value(
                    button_destination,
                    if subcommand == 4 {
                        i32::from(button > 0)
                    } else {
                        button
                    },
                );
                None
            }
            0x10 => {
                let area = self.value()?;
                self.disabled_areas.insert(area, true);
                None
            }
            0x11 => {
                let area = self.value()?;
                self.disabled_areas.insert(area, false);
                None
            }
            0x15 => {
                let x = self.value()?;
                let y = self.value()?;
                let destination = self.raw_value_index()?;
                let area = self.area_at(x, y);
                self.flags.set_value(destination, area);
                None
            }
            0x20 => {
                self.value()?;
                self.value()?;
                None
            }
            subcommand => bail!("avg32: unsupported area-control subcommand {subcommand:#04x}"),
        };
        Ok(action)
    }

    fn area_at(&self, x: i32, y: i32) -> i32 {
        let area = self.area_map.as_ref().map_or(0, |map| map.area_at(x, y)) as i32;
        let disabled = self.disabled_areas.get(&area).copied().unwrap_or(false);
        if disabled { 0 } else { area }
    }

    fn buffer_get_put(&mut self) -> Result<Option<VmAction>> {
        match self.byte()? {
            1 => {
                let rect = self.rect()?;
                let buffer = self.value()? as u32;
                Ok(Some(VmAction::BufferCopy {
                    source: buffer,
                    destination: 31,
                    source_rect: rect,
                    destination_xy: [0, 0],
                    masked: false,
                    color_key: None,
                }))
            }
            2 => {
                let destination_xy = [self.value()?, self.value()?];
                let destination = self.value()? as u32;
                Ok(Some(VmAction::BufferCopy {
                    source: 31,
                    destination,
                    source_rect: [0, 0, 639, 479],
                    destination_xy,
                    masked: false,
                    color_key: None,
                }))
            }
            0x20 => {
                self.byte()?;
                self.value()?;
                Ok(None)
            }
            subcommand => bail!("avg32: unsupported graphics-get-put subcommand {subcommand:#04x}"),
        }
    }

    fn name_control(&mut self) -> Result<()> {
        match self.byte()? {
            1 => {
                // Bounds, foreground/background colours for inline name entry.
                for _ in 0..10 {
                    self.value()?;
                }
            }
            2 | 3 | 0x20 => {
                self.raw_value_index()?;
            }
            4 | 0x30 | 0x31 => {}
            0x10..=0x12 => {
                self.raw_value_index()?;
                self.raw_value_index()?;
            }
            0x21 => {
                self.raw_value_index()?;
                self.scene_text()?;
                for _ in 0..9 {
                    self.value()?;
                }
            }
            0x24 => {
                let count = usize::from(self.byte()?);
                for _ in 0..count {
                    self.raw_value_index()?;
                    self.formatted_text()?;
                }
            }
            subcommand => bail!("avg32: unsupported name-control subcommand {subcommand:#04x}"),
        }
        Ok(())
    }

    fn buffer_copy(&mut self) -> Result<Option<VmAction>> {
        // AVG32 17D+ (including AIR's 3217 runtime) stores the extra copy
        // flags.  Operations are emitted through the renderer boundary in a
        // later layer; consuming them here keeps bytecode state authoritative.
        let subcommand = self.byte()?;
        let copy = match subcommand {
            0x00 => {
                let rect = self.rect()?;
                let source = self.value()? as u32;
                self.value()?; // update flag
                Some(VmAction::BufferCopy {
                    source,
                    destination: 0,
                    source_rect: rect,
                    destination_xy: [rect[0], rect[1]],
                    masked: false,
                    color_key: None,
                })
            }
            0x01 | 0x02 | 0x08 => {
                let rect = self.rect()?;
                let source = pdt_index(self.value()?, 0);
                let destination_xy = [self.value()?, self.value()?];
                let destination = self.value()?;
                self.value()?; // AVG32 17D+ copy flag
                Some(VmAction::BufferCopy {
                    source,
                    destination: pdt_index(destination, source),
                    source_rect: rect,
                    destination_xy,
                    masked: subcommand != 1,
                    color_key: None,
                })
            }
            0x03 => {
                let rect = self.rect()?;
                let source = pdt_index(self.value()?, 0);
                let destination_xy = [self.value()?, self.value()?];
                let destination = self.value()?;
                let color_key = [self.value()?, self.value()?, self.value()?];
                Some(VmAction::BufferCopy {
                    source,
                    destination: pdt_index(destination, source),
                    source_rect: rect,
                    destination_xy,
                    masked: false,
                    color_key: Some(color_key),
                })
            }
            0x05 => {
                // AVG32 `SnrPDT_Swap`/`PDTMGR::Swap`: a true pixel exchange
                // between the two buffers, not a one-directional copy. The
                // source side is commonly an untouched work buffer that only
                // receives content via this swap, so it must not be confused
                // with `BufferCopy`.
                let rect = self.rect()?;
                let source = pdt_index(self.value()?, 0);
                let destination_xy = [self.value()?, self.value()?];
                let destination = self.value()?;
                return Ok(Some(VmAction::BufferSwap {
                    source,
                    destination: pdt_index(destination, source),
                    source_rect: rect,
                    destination_xy,
                }));
            }
            0x11 | 0x12 => {
                let source = pdt_index(self.value()?, 0);
                let destination = self.value()?;
                self.value()?; // 17D+ copy flag
                Some(VmAction::BufferCopy {
                    source,
                    destination: pdt_index(destination, source),
                    source_rect: [0, 0, 640, 480],
                    destination_xy: [0, 0],
                    masked: subcommand == 0x12,
                    color_key: None,
                })
            }
            0x20 => {
                for _ in 0..15 {
                    self.value()?;
                }
                None
            }
            0x21 => {
                for _ in 0..16 {
                    self.value()?;
                }
                None
            }
            0x22 => {
                for _ in 0..18 {
                    self.value()?;
                }
                None
            }
            subcommand => bail!("avg32: unsupported graphics-copy subcommand {subcommand:#04x}"),
        };
        if let Some(copy) = copy {
            return Ok(Some(copy));
        }
        Ok(None)
    }

    fn graphics_special(&mut self) -> Result<Option<VmAction>> {
        match self.byte()? {
            1 => {
                let buffer = self.value()? as u32;
                let color = [self.value()?, self.value()?, self.value()?];
                Ok(Some(VmAction::BufferFill {
                    buffer,
                    rect: [0, 0, 639, 479],
                    color,
                }))
            }
            0x10 => {
                let color = [self.value()?, self.value()?, self.value()?];
                let microseconds = self.value()?.max(0) as u32;
                self.value()?; // flash repetition count; frontend presents final flash colour.
                Ok(Some(VmAction::Fade {
                    pattern: None,
                    color: Some(color),
                    microseconds: Some(microseconds),
                }))
            }
            subcommand => bail!("avg32: unsupported graphics-special subcommand {subcommand:#04x}"),
        }
    }

    fn buffer_region(&mut self) -> Result<Option<VmAction>> {
        match self.byte()? {
            0x02 => {
                let rect = self.rect()?;
                let buffer = self.value()? as u32;
                let color = [self.value()?, self.value()?, self.value()?];
                Ok(Some(VmAction::BufferFill {
                    buffer,
                    rect,
                    color,
                }))
            }
            0x04 => {
                let rect = self.rect()?;
                let buffer = self.value()? as u32;
                let color = [self.value()?, self.value()?, self.value()?];
                Ok(Some(VmAction::BufferOutline {
                    buffer,
                    rect,
                    color,
                }))
            }
            0x07 => {
                let rect = self.rect()?;
                let buffer = self.value()? as u32;
                Ok(Some(VmAction::BufferInvert { buffer, rect }))
            }
            0x10 => {
                let rect = self.rect()?;
                let buffer = self.value()? as u32;
                let color = [self.value()?, self.value()?, self.value()?];
                Ok(Some(VmAction::BufferColorMask {
                    buffer,
                    rect,
                    color,
                }))
            }
            0x11 => {
                let rect = self.rect()?;
                let buffer = self.value()? as u32;
                Ok(Some(VmAction::BufferFade {
                    buffer,
                    rect,
                    color: [0, 0, 0],
                    amount: 0x80,
                }))
            }
            0x12 => {
                let rect = self.rect()?;
                let buffer = self.value()? as u32;
                Ok(Some(VmAction::BufferFade {
                    buffer,
                    rect,
                    color: [0, 0, 0],
                    amount: 0xc0,
                }))
            }
            0x15 => {
                let rect = self.rect()?;
                let buffer = self.value()? as u32;
                let color = [self.value()?, self.value()?, self.value()?];
                let amount = self.value()?;
                Ok(Some(VmAction::BufferFade {
                    buffer,
                    rect,
                    color,
                    amount,
                }))
            }
            0x20 => {
                let rect = self.rect()?;
                let buffer = self.value()? as u32;
                Ok(Some(VmAction::BufferMonochrome { buffer, rect }))
            }
            0x30 => {
                let source_rect = self.rect()?;
                let source = self.value()? as u32;
                let destination_rect = self.rect()?;
                let destination = self.value()? as u32;
                Ok(Some(VmAction::BufferStretchCopy {
                    source,
                    destination,
                    source_rect,
                    destination_rect,
                }))
            }
            0x32 => {
                for _ in 0..15 {
                    self.value()?;
                }
                Ok(None)
            }
            subcommand => bail!("avg32: unsupported graphics-region subcommand {subcommand:#04x}"),
        }
    }

    fn buffer_scroll(&mut self) -> Result<Option<VmAction>> {
        self.byte()?; // Revision-specific mode; all known AVG32 scripts use 2.
        let down = self.byte()? != 0;
        let rect = self.rect()?;
        let amount = self.value()?;
        self.value()?; // Animation speed. The renderer applies the terminal frame.
        Ok(Some(VmAction::BufferScroll {
            source: 1,
            destination: 0,
            rect,
            amount,
            down,
        }))
    }

    fn draw_buffer_text(&mut self) -> Result<Option<VmAction>> {
        self.byte()?; // All documented variants share the same payload.
        let position = [self.value()?, self.value()?];
        let buffer = self.value()? as u32;
        let color = [self.value()?, self.value()?, self.value()?];
        let text = self.formatted_text()?;
        Ok(Some(VmAction::DrawBufferText {
            buffer,
            position,
            color,
            text,
        }))
    }

    fn ending_control(&mut self) -> Result<Option<VmAction>> {
        let mode = self.byte()?;
        match mode {
            0x05 => Ok(None),
            0x10 | 0x20 | 0x30 => {
                let alignment = self.byte()?;
                let count = usize::from(self.byte()?);
                let position = self.value()?;
                let wait = self.value()?;
                let pixels_per_step = self.value()?;
                let cancellable_flag =
                    (mode == 0x30).then(|| self.raw_value_index()).transpose()?;
                let mut frames = Vec::with_capacity(count);
                for _ in 0..count {
                    frames.push((self.scene_text()?, self.value()?));
                }
                self.byte()?; // Sequence terminator.
                Ok(Some(VmAction::EndingSequence {
                    mode,
                    alignment,
                    position,
                    wait,
                    pixels_per_step,
                    cancellable_flag,
                    frames,
                }))
            }
            0x03 | 0x04 => {
                let count = usize::from(self.byte()?);
                let position = self.value()?;
                let wait = self.value()?;
                let mut frames = Vec::with_capacity(count);
                for _ in 0..count {
                    frames.push((self.scene_text()?, self.value()?));
                }
                self.byte()?; // Sequence terminator.
                Ok(Some(VmAction::EndingSequence {
                    mode,
                    alignment: 0,
                    position,
                    wait,
                    pixels_per_step: 0,
                    cancellable_flag: None,
                    frames,
                }))
            }
            _ => bail!("avg32: unsupported ending-control mode {mode:#04x}"),
        }
    }

    fn legacy_control(&mut self, opcode: u8) -> Result<()> {
        match opcode {
            0x2c => match self.byte()? {
                1 => {
                    self.value()?;
                }
                subcommand => bail!("avg32: unsupported 0x2c subcommand {subcommand:#04x}"),
            },
            0x2d => match self.byte()? {
                1 | 2 => {
                    self.value()?;
                }
                3 | 4 => {
                    self.value()?;
                    self.value()?;
                }
                subcommand => bail!("avg32: unsupported 0x2d subcommand {subcommand:#04x}"),
            },
            0x2e => match self.byte()? {
                1 => {
                    self.value()?;
                }
                2 => {
                    self.value()?;
                    self.value()?;
                }
                subcommand => bail!("avg32: unsupported 0x2e subcommand {subcommand:#04x}"),
            },
            0x2f => match self.byte()? {
                1 => {
                    self.value()?;
                    self.value()?;
                    self.value()?;
                }
                subcommand => bail!("avg32: unsupported 0x2f subcommand {subcommand:#04x}"),
            },
            0x30 => self.require_subcommand(1, "0x30 legacy control")?,
            0x31 => match self.byte()? {
                1 => {
                    self.value()?;
                }
                2 => {}
                subcommand => bail!("avg32: unsupported 0x31 subcommand {subcommand:#04x}"),
            },
            _ => unreachable!("legacy-control opcode was checked by the dispatcher"),
        }
        Ok(())
    }

    fn cg_mode_control(&mut self) -> Result<()> {
        match self.byte()? {
            1..=3 => {
                let destination = self.raw_value_index()?;
                self.flags.set_value(destination, 0);
            }
            4 => {
                // Auto-CG mode has a per-frame decoder in original engines.
                // Preserve stream alignment; a frontend can elect to expose it.
                self.value()?;
            }
            5 => {
                let current_cg = self.raw_value_index()?;
                let destination_name = self.raw_value_index()?;
                let destination_flag = self.raw_value_index()?;
                let _ = current_cg;
                self.flags.set_string(destination_name, String::new());
                self.flags.set_value(destination_flag, 0);
            }
            subcommand => bail!("avg32: unsupported CG-mode subcommand {subcommand:#04x}"),
        }
        Ok(())
    }

    fn rect(&mut self) -> Result<[i32; 4]> {
        Ok([self.value()?, self.value()?, self.value()?, self.value()?])
    }

    fn strings(&mut self) -> Result<()> {
        match self.byte()? {
            1 => {
                let destination = self.raw_value_index()?;
                let text = self.scene_text()?;
                self.flags.set_string(destination, text);
            }
            2 => {
                let destination = self.raw_value_index()?;
                let source = self.raw_value_index()?;
                self.flags.set_value(
                    destination,
                    self.flags.string(source).chars().count() as i32,
                );
            }
            3 => {
                let destination = self.raw_value_index()?;
                let left = self.raw_value_index()?;
                let right = self.raw_value_index()?;
                self.flags.set_value(
                    destination,
                    self.flags.string(left).cmp(self.flags.string(right)) as i32,
                );
            }
            4 => {
                let destination = self.raw_value_index()?;
                let source = self.raw_value_index()?;
                let value = format!(
                    "{}{}",
                    self.flags.string(destination),
                    self.flags.string(source)
                );
                self.flags.set_string(destination, value);
            }
            5 => {
                let destination = self.raw_value_index()?;
                let source = self.raw_value_index()?;
                let value = self.flags.string(source).to_owned();
                self.flags.set_string(destination, value);
            }
            6 => {
                let source = self.raw_value_index()?;
                let destination = self.raw_value_index()?;
                let radix = self.value()?;
                self.flags.set_string(
                    destination,
                    match radix {
                        16 => format!("{:X}", self.flags.value(source)),
                        _ => self.flags.value(source).to_string(),
                    },
                );
            }
            7 => {
                self.raw_value_index()?;
            }
            8 => {
                let source = self.raw_value_index()?;
                let destination = self.raw_value_index()?;
                self.flags.set_value(
                    destination,
                    self.flags.string(source).trim().parse().unwrap_or(0),
                );
            }
            subcommand => bail!("avg32: unsupported string subcommand {subcommand:#04x}"),
        }
        Ok(())
    }

    fn formatted_text(&mut self) -> Result<String> {
        let mut output = String::new();
        loop {
            match self.byte()? {
                0 => break,
                0xfe | 0xff => output.push_str(&self.c_string()?),
                0xfd => {
                    let index = self.raw_value_index()?;
                    output.push_str(self.flags.string(index));
                }
                0x10 => match self.byte()? {
                    1 => output.push_str(&self.value()?.to_string()),
                    2 => {
                        let value = self.value()?;
                        let width = self.value()? as usize;
                        output.push_str(&format!("{value:0width$}"));
                    }
                    3 => {
                        let index = self.raw_value_index()?;
                        output.push_str(self.flags.string(index));
                    }
                    0x11 => {
                        self.value()?;
                    }
                    0x13 => {}
                    subcommand => {
                        bail!("avg32: unsupported formatted-text subcommand {subcommand:#04x}")
                    }
                },
                code => bail!("avg32: unsupported formatted-text token {code:#04x}"),
            }
        }
        Ok(output)
    }

    fn scene_text(&mut self) -> Result<String> {
        if self.peek()? == b'@' {
            self.pc += 1;
            let index = self.raw_value_index()?;
            return Ok(self.flags.string(index).to_owned());
        }
        self.c_string()
    }

    fn text_packet(&mut self) -> Result<String> {
        if self.extended_text {
            // This is an authored text-position token, not a bytecode jump.
            // Retaining it would shift every subsequent opcode by four bytes.
            let _text_position = self.offset()?;
        }
        self.c_string()
    }

    fn c_string(&mut self) -> Result<String> {
        let start = self.pc;
        let end = self.code()[start..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|length| start + length)
            .ok_or_else(|| anyhow::anyhow!("avg32: unterminated Shift-JIS string at {start:#x}"))?;
        let (text, _, had_errors) = SHIFT_JIS.decode(&self.code()[start..end]);
        if had_errors {
            bail!("avg32: invalid Shift-JIS string at {start:#x}");
        }
        let text = text.into_owned();
        self.pc = end + 1;
        Ok(text)
    }

    fn code(&self) -> &[u8] {
        self.program.code()
    }

    fn value(&mut self) -> Result<i32> {
        let raw = self.scene_value()?;
        Ok(match raw.kind {
            ValueKind::Constant => raw.value as i32,
            ValueKind::Variable => self.flags.value(raw.value),
        })
    }

    fn raw_value_index(&mut self) -> Result<u32> {
        Ok(self.scene_value()?.value)
    }

    fn scene_value(&mut self) -> Result<SceneValue> {
        let (value, consumed) = parse_scene_value(&self.code()[self.pc..])
            .with_context(|| format!("avg32: invalid compact value at pc {:#x}", self.pc))?;
        self.pc += consumed;
        Ok(value)
    }

    fn offset(&mut self) -> Result<usize> {
        let start = self.pc;
        let bytes: [u8; 4] = self
            .code()
            .get(start..start + 4)
            .ok_or_else(|| anyhow::anyhow!("avg32: truncated jump offset at {start:#x}"))?
            .try_into()
            .expect("slice has four bytes");
        self.pc += 4;
        Ok(u32::from_le_bytes(bytes) as usize)
    }

    fn jump(&mut self, target: usize) -> Result<()> {
        if target >= self.code().len() {
            bail!("avg32: jump target {target:#x} is outside scene bytecode");
        }
        self.pc = target;
        Ok(())
    }

    fn require_subcommand(&mut self, expected: u8, label: &str) -> Result<()> {
        let actual = self.byte()?;
        if actual != expected {
            bail!("avg32: unsupported {label} subcommand {actual:#04x}");
        }
        Ok(())
    }

    fn peek(&self) -> Result<u8> {
        self.code()
            .get(self.pc)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("avg32: pc {:#x} is outside scene bytecode", self.pc))
    }

    fn byte(&mut self) -> Result<u8> {
        let byte = self.peek()?;
        self.pc += 1;
        Ok(byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program(code: &[u8]) -> Avg32Program {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"TPC32");
        bytes.extend_from_slice(&[0; 0x13]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 0x30]);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 5]);
        bytes.extend_from_slice(code);
        Avg32Program::parse(bytes).unwrap()
    }

    #[test]
    fn executes_variables_then_yields_text() {
        let mut vm = Avg32Vm::new(program(&[0x3b, 0x91, 0x17, 0xfe, b'O', b'K', 0]));
        assert_eq!(
            vm.run(Input::None, 10).unwrap(),
            VmStop::Yield(VmAction::Text("OK".into()))
        );
        assert_eq!(vm.flags().value(1), 7);
    }

    #[test]
    fn input_wait_does_not_reexecute_the_preceding_text() {
        let mut vm = Avg32Vm::new(program(&[0xfe, b'A', 0, 0x03, 0xfe, b'B', 0, 0]));
        assert!(matches!(
            vm.run(Input::None, 10).unwrap(),
            VmStop::Yield(VmAction::Text(_))
        ));
        assert!(matches!(
            vm.run(Input::None, 10).unwrap(),
            VmStop::Yield(VmAction::WaitForInput { .. })
        ));
        assert!(
            matches!(vm.run(Input::Advance, 10).unwrap(), VmStop::Yield(VmAction::Text(text)) if text == "B")
        );
    }

    #[test]
    fn extended_text_packet_skips_air_position_word() {
        let mut vm = Avg32Vm::new(program(&[
            0xfe, 0x34, 0x12, 0x00, 0x00, b'A', b'I', b'R', 0,
        ]))
        .with_extended_text();
        assert_eq!(
            vm.run(Input::None, 10).unwrap(),
            VmStop::Yield(VmAction::Text("AIR".into()))
        );
    }

    #[test]
    fn local_call_and_return_use_bytecode_relative_offsets() {
        // Call offset 8 -> set val 1 to 9 -> return -> text.
        let mut code = vec![0x1b];
        code.extend_from_slice(&8u32.to_le_bytes());
        code.extend_from_slice(&[0xfe, b'X', 0, 0x3b, 0x91, 0x19, 0x20, 1]);
        let mut vm = Avg32Vm::new(program(&code));
        assert!(
            matches!(vm.run(Input::None, 20).unwrap(), VmStop::Yield(VmAction::Text(text)) if text == "X")
        );
        assert_eq!(vm.flags().value(1), 9);
    }
}
