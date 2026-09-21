//! Structural parser for AVG32 `TPC32` scene headers and compact values.

use anyhow::{Result, bail};
use encoding_rs::SHIFT_JIS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Avg32SceneHeader {
    pub labels: Vec<u32>,
    pub counter_start: u32,
    pub menus: Vec<SceneMenu>,
    pub menu_strings: Vec<String>,
    /// Byte offset of the first opcode after the header.
    pub code_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneMenu {
    pub id: u8,
    pub unknown: [u8; 2],
    pub submenus: Vec<SceneSubmenu>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneSubmenu {
    pub id: u8,
    pub unknown: [u8; 2],
    pub flags: Vec<SceneFlag>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SceneFlag {
    pub unknown: u8,
    pub values: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Constant,
    Variable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SceneValue {
    pub value: u32,
    pub kind: ValueKind,
}

impl Avg32SceneHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let mut reader = Reader::new(bytes);
        if reader.take(5)? != b"TPC32" {
            bail!("avg32: expected TPC32 scene magic");
        }
        reader.take(0x13)?;
        let label_count = reader.u32()? as usize;
        let counter_start = reader.u32()?;
        let mut labels = Vec::with_capacity(label_count);
        for _ in 0..label_count {
            labels.push(reader.u32()?);
        }
        reader.take(0x30)?;
        let menu_count = reader.u32()? as usize;
        let mut menus = Vec::with_capacity(menu_count);
        let mut string_count = 0usize;
        for _ in 0..menu_count {
            let id = reader.u8()?;
            let submenu_count = reader.u8()? as usize;
            let unknown = [reader.u8()?, reader.u8()?];
            let mut submenus = Vec::with_capacity(submenu_count);
            string_count = string_count
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("avg32: menu string count overflows"))?;
            for _ in 0..submenu_count {
                let submenu_id = reader.u8()?;
                let flag_count = reader.u8()? as usize;
                let submenu_unknown = [reader.u8()?, reader.u8()?];
                string_count = string_count
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("avg32: menu string count overflows"))?;
                let mut flags = Vec::with_capacity(flag_count);
                for _ in 0..flag_count {
                    let value_count = reader.u8()? as usize;
                    let unknown = reader.u8()?;
                    let mut values = Vec::with_capacity(value_count);
                    for _ in 0..value_count {
                        values.push(reader.u32()?);
                    }
                    flags.push(SceneFlag { unknown, values });
                }
                submenus.push(SceneSubmenu {
                    id: submenu_id,
                    unknown: submenu_unknown,
                    flags,
                });
            }
            menus.push(SceneMenu {
                id,
                unknown,
                submenus,
            });
        }
        let mut menu_strings = Vec::with_capacity(string_count);
        for _ in 0..string_count {
            menu_strings.push(reader.shift_jis_c_string()?);
        }
        reader.take(5)?;
        Ok(Self {
            labels,
            counter_start,
            menus,
            menu_strings,
            code_offset: reader.at,
        })
    }
}

/// Parses AVG32's compact integer/variable reference encoding.
pub fn parse_scene_value(bytes: &[u8]) -> Result<(SceneValue, usize)> {
    let first = *bytes
        .first()
        .ok_or_else(|| anyhow::anyhow!("avg32: missing scene value"))?;
    let len = usize::from((first >> 4) & 0x07);
    if len == 0 || len > bytes.len() {
        bail!("avg32: malformed compact scene value");
    }
    let mut value = 0u32;
    for byte in bytes[1..len].iter().rev() {
        value = (value << 8) | u32::from(*byte);
    }
    value = (value << 4) | u32::from(first & 0x0f);
    Ok((
        SceneValue {
            value,
            kind: if first & 0x80 != 0 {
                ValueKind::Variable
            } else {
                ValueKind::Constant
            },
        },
        len,
    ))
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(len)
            .ok_or_else(|| anyhow::anyhow!("avg32: scene offset overflows"))?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| anyhow::anyhow!("avg32: truncated scene header"))?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("slice has four bytes"),
        ))
    }

    fn shift_jis_c_string(&mut self) -> Result<String> {
        let tail = self
            .bytes
            .get(self.at..)
            .ok_or_else(|| anyhow::anyhow!("avg32: truncated scene string"))?;
        let len = tail
            .iter()
            .position(|&byte| byte == 0)
            .ok_or_else(|| anyhow::anyhow!("avg32: unterminated scene string"))?;
        let (decoded, _, had_errors) = SHIFT_JIS.decode(&tail[..len]);
        if had_errors {
            bail!("avg32: invalid Shift-JIS menu string");
        }
        self.at += len + 1;
        Ok(decoded.into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_compact_value() {
        assert_eq!(
            parse_scene_value(&[0x1f]).unwrap(),
            (
                SceneValue {
                    value: 15,
                    kind: ValueKind::Constant
                },
                1
            )
        );
        assert_eq!(
            parse_scene_value(&[0x91]).unwrap(),
            (
                SceneValue {
                    value: 1,
                    kind: ValueKind::Variable
                },
                1
            )
        );
        assert_eq!(
            parse_scene_value(&[0x21, 0x23]).unwrap(),
            (
                SceneValue {
                    value: 0x231,
                    kind: ValueKind::Constant
                },
                2
            )
        );
    }

    #[test]
    fn parses_the_tpc32_header_before_bytecode() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"TPC32");
        bytes.extend_from_slice(&[0; 0x13]);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&42u32.to_le_bytes());
        bytes.extend_from_slice(&0x1234u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 0x30]);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&[7, 1, 8, 9]); // menu, one submenu
        bytes.extend_from_slice(&[3, 1, 4, 5]); // submenu, one flag
        bytes.extend_from_slice(&[1, 6]); // flag, one value
        bytes.extend_from_slice(&0xbeefu32.to_le_bytes());
        bytes.extend_from_slice(b"menu\0submenu\0");
        bytes.extend_from_slice(&[0; 5]);
        let header = Avg32SceneHeader::parse(&bytes).unwrap();
        assert_eq!(header.labels, [0x1234]);
        assert_eq!(header.counter_start, 42);
        assert_eq!(header.menu_strings, ["menu", "submenu"]);
        assert_eq!(header.code_offset, bytes.len());
    }
}
