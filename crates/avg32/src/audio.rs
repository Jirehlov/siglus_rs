//! AVG32 audio dispatch backed by Kira on desktop platforms.

use std::collections::BTreeMap;
#[cfg(not(target_os = "horizon"))]
use std::io::Cursor;

use crate::resource::Avg32Resources;
use crate::vm::{AudioKind, VmAction};
use anyhow::{Context, Result};

#[cfg(not(target_os = "horizon"))]
use kira::manager::backend::DefaultBackend;
#[cfg(not(target_os = "horizon"))]
use kira::manager::{AudioManager, AudioManagerSettings};
#[cfg(not(target_os = "horizon"))]
use kira::sound::static_sound::{StaticSoundData, StaticSoundHandle};
#[cfg(not(target_os = "horizon"))]
use kira::tween::Tween;

pub struct Avg32Audio {
    #[cfg(not(target_os = "horizon"))]
    manager: Option<AudioManager<DefaultBackend>>,
    #[cfg(not(target_os = "horizon"))]
    waves: BTreeMap<i32, StaticSoundHandle>,
    #[cfg(not(target_os = "horizon"))]
    bgm: Option<StaticSoundHandle>,
}

impl std::fmt::Debug for Avg32Audio {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Avg32Audio").finish_non_exhaustive()
    }
}

impl Default for Avg32Audio {
    fn default() -> Self {
        Self::new()
    }
}

impl Avg32Audio {
    pub fn new() -> Self {
        #[cfg(not(target_os = "horizon"))]
        {
            Self {
                manager: AudioManager::new(AudioManagerSettings::default()).ok(),
                waves: BTreeMap::new(),
                bgm: None,
            }
        }
        #[cfg(target_os = "horizon")]
        {
            Self {}
        }
    }

    pub fn apply(&mut self, action: &VmAction, resources: &Avg32Resources) -> Result<()> {
        #[cfg(not(target_os = "horizon"))]
        match action {
            VmAction::PlayAudio {
                kind: AudioKind::Wave { channel, .. },
                name,
            } => {
                let bytes = resources
                    .read("WAV", name)
                    .with_context(|| format!("failed to open AVG32 WAV {name}"))?;
                let data = StaticSoundData::from_cursor(Cursor::new(bytes))
                    .context("failed to decode AVG32 WAV")?;
                let Some(manager) = self.manager.as_mut() else {
                    return Ok(());
                };
                let handle = manager.play(data).context("failed to play AVG32 WAV")?;
                self.waves.insert(channel.unwrap_or(-1), handle);
            }
            VmAction::PlayAudio {
                kind: AudioKind::Bgm { .. },
                name,
            } => {
                // Some AVG32 titles route BGM to CD audio. If a title exposes
                // a loose BGM route, play it; otherwise leave CD handling to
                // the platform frontend without making the VM fail.
                if let Ok(bytes) = resources.read("BGM", name) {
                    let data = StaticSoundData::from_cursor(Cursor::new(bytes))
                        .context("failed to decode AVG32 BGM")?;
                    if let Some(manager) = self.manager.as_mut() {
                        self.bgm = Some(manager.play(data).context("failed to play AVG32 BGM")?);
                    }
                }
            }
            VmAction::StopAudio {
                kind: AudioKind::Wave { .. },
                channel,
            } => {
                if let Some(channel) = channel {
                    if let Some(mut handle) = self.waves.remove(channel) {
                        handle.stop(Tween::default());
                    }
                } else {
                    for (_, mut handle) in std::mem::take(&mut self.waves) {
                        handle.stop(Tween::default());
                    }
                }
            }
            VmAction::StopAudio {
                kind: AudioKind::Bgm { .. },
                ..
            } => {
                if let Some(mut handle) = self.bgm.take() {
                    handle.stop(Tween::default());
                }
            }
            _ => {}
        }
        #[cfg(target_os = "horizon")]
        let _ = (action, resources);
        Ok(())
    }
}
