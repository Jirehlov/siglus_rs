//! AVG32 runtime tables decoded from `Gameexe.ini`.

use std::collections::BTreeMap;

use siglus_assets::gameexe::GameexeConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Effect {
    pub source_rect: [i32; 4],
    pub destination: [i32; 2],
    pub step_microseconds: u32,
    pub command: i32,
    pub mask: i32,
    pub arguments: [i32; 6],
}

#[derive(Debug, Clone, Default)]
pub struct Avg32Config {
    effects: BTreeMap<usize, Effect>,
    colors: BTreeMap<i32, [u8; 3]>,
    message_position: [i32; 2],
    message_font_size: [i32; 2],
}

impl Avg32Config {
    pub fn from_gameexe(gameexe: &GameexeConfig) -> Self {
        let mut effects = BTreeMap::new();
        let mut colors = BTreeMap::new();
        for entry in &gameexe.entries {
            let Some(index) = entry.key_index("SEL") else {
                continue;
            };
            let values: Vec<i32> = entry
                .value
                .split(',')
                .filter_map(|value| value.trim().parse().ok())
                .collect();
            if values.len() != 15 {
                continue;
            }
            effects.insert(
                index,
                Effect {
                    source_rect: [values[0], values[1], values[2], values[3]],
                    destination: [values[4], values[5]],
                    step_microseconds: values[6].max(0) as u32,
                    command: values[7],
                    mask: values[8],
                    arguments: [
                        values[9], values[10], values[11], values[12], values[13], values[14],
                    ],
                },
            );
        }
        for entry in &gameexe.entries {
            let Some(index) = entry.key_index("COLOR_TABLE") else {
                continue;
            };
            let values = parse_values(&entry.value);
            if let [red, green, blue] = values.as_slice() {
                colors.insert(
                    index as i32,
                    [
                        (*red).clamp(0, 255) as u8,
                        (*green).clamp(0, 255) as u8,
                        (*blue).clamp(0, 255) as u8,
                    ],
                );
            }
        }
        Self {
            effects,
            colors,
            message_position: pair(gameexe, "WINDOW_MSG_POS", [0, 0]),
            message_font_size: pair(gameexe, "MSG_MOJI_SIZE", [16, 24]),
        }
    }

    pub fn effect(&self, index: usize) -> Option<Effect> {
        self.effects.get(&index).copied()
    }

    pub fn color(&self, index: i32) -> [u8; 3] {
        self.colors.get(&index).copied().unwrap_or([255, 255, 255])
    }

    pub const fn message_position(&self) -> [i32; 2] {
        self.message_position
    }

    pub const fn message_font_size(&self) -> [i32; 2] {
        self.message_font_size
    }
}

fn pair(gameexe: &GameexeConfig, key: &str, default: [i32; 2]) -> [i32; 2] {
    let Some(value) = gameexe.get_value(key) else {
        return default;
    };
    let values = parse_values(value);
    match values.as_slice() {
        [first, second, ..] => [*first, *second],
        _ => default,
    }
}

fn parse_values(value: &str) -> Vec<i32> {
    value
        .split(',')
        .filter_map(|value| value.trim().parse().ok())
        .collect()
}
