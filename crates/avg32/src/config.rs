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
    fade_colors: BTreeMap<i32, [u8; 3]>,
    default_fade_microseconds: u32,
    message_position: [i32; 2],
    message_font_size: [i32; 2],
}

impl Avg32Config {
    pub fn from_gameexe(gameexe: &GameexeConfig) -> Self {
        let mut effects = BTreeMap::new();
        let mut colors = BTreeMap::new();
        let mut fade_colors = BTreeMap::new();
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
                    // AVG32 timers are millisecond-based.  The core uses
                    // microseconds to retain sub-millisecond scheduler
                    // precision, so convert at the resource boundary.
                    step_microseconds: (values[6].max(0) as u32).saturating_mul(1_000),
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
        for entry in &gameexe.entries {
            let Some(index) = entry.key_index("FADE_TABLE") else {
                continue;
            };
            let values = parse_values(&entry.value);
            if let [red, green, blue] = values.as_slice() {
                fade_colors.insert(
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
            fade_colors,
            default_fade_microseconds: gameexe
                .get_value("FADE_TIME")
                .and_then(|value| value.trim().parse::<u32>().ok())
                .unwrap_or(40)
                .saturating_mul(1_000),
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

    pub fn fade_color(&self, index: i32) -> [u8; 3] {
        self.fade_colors.get(&index).copied().unwrap_or([0, 0, 0])
    }

    pub const fn default_fade_microseconds(&self) -> u32 {
        self.default_fade_microseconds
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
