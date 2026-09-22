//! AVG32 audio dispatch backed by Kira on desktop platforms.

#[cfg(not(target_os = "horizon"))]
use crate::resource::Avg32Resources;
use crate::vm::{AudioKind, VmAction};
use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
#[cfg(not(target_os = "horizon"))]
use std::io::{Cursor, Read, Seek, SeekFrom};

#[cfg(not(target_os = "horizon"))]
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
    #[cfg(not(target_os = "horizon"))]
    voice: Option<StaticSoundHandle>,
    #[cfg(not(target_os = "horizon"))]
    cd_tracks: Option<BTreeMap<i32, CdTrack>>,
    #[cfg(not(target_os = "horizon"))]
    movie: Option<StaticSoundHandle>,
}

#[cfg(not(target_os = "horizon"))]
#[derive(Debug, Clone)]
struct CdTrack {
    image: std::path::PathBuf,
    first_sector: usize,
    sector_count: usize,
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
                voice: None,
                cd_tracks: None,
                movie: None,
            }
        }
        #[cfg(target_os = "horizon")]
        {
            Self {}
        }
    }

    /// Plays an FMV's demuxed PCM audio track (see `movie.rs`), wrapping it
    /// in a WAV header the way [`decode_cd_track`] already does for raw CD
    /// audio sectors.
    #[cfg(not(target_os = "horizon"))]
    pub fn play_movie_pcm(
        &mut self,
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u16,
        pcm: &[u8],
    ) -> Result<()> {
        let wav = wrap_pcm_as_wav(sample_rate, channels, bits_per_sample, pcm);
        let data = StaticSoundData::from_cursor(Cursor::new(wav))
            .context("failed to decode AVG32 FMV audio track")?;
        if let Some(mut previous) = self.movie.take() {
            previous.stop(Tween::default());
        }
        if let Some(manager) = self.manager.as_mut() {
            self.movie = Some(
                manager
                    .play(data)
                    .context("failed to play AVG32 FMV audio")?,
            );
        }
        Ok(())
    }

    #[cfg(target_os = "horizon")]
    pub fn play_movie_pcm(
        &mut self,
        _sample_rate: u32,
        _channels: u16,
        _bits_per_sample: u16,
        _pcm: &[u8],
    ) -> Result<()> {
        Ok(())
    }

    pub fn stop_movie_audio(&mut self) {
        #[cfg(not(target_os = "horizon"))]
        if let Some(mut handle) = self.movie.take() {
            handle.stop(Tween::default());
        }
    }

    pub fn apply(&mut self, action: &VmAction, resources: &Avg32Resources) -> Result<()> {
        #[cfg(not(target_os = "horizon"))]
        match action {
            VmAction::PlayAudio {
                kind: AudioKind::Wave {
                    channel, looped, ..
                },
                name,
            } => {
                let bytes = resources
                    .read("WAV", name)
                    .with_context(|| format!("failed to open AVG32 WAV {name}"))?;
                let data = StaticSoundData::from_cursor(Cursor::new(bytes))
                    .context("failed to decode AVG32 WAV")?;
                let data = looped.then(|| data.loop_region(..)).unwrap_or(data);
                let Some(manager) = self.manager.as_mut() else {
                    return Ok(());
                };
                let handle = manager.play(data).context("failed to play AVG32 WAV")?;
                self.waves.insert(channel.unwrap_or(-1), handle);
            }
            VmAction::PlayAudio {
                kind: AudioKind::Bgm { looped, .. },
                name,
            } => {
                // Some AVG32 titles route BGM to CD audio. If a title exposes
                // a loose BGM route, play it; otherwise leave CD handling to
                // the platform frontend without making the VM fail.
                let data = if let Ok(bytes) = resources.read("BGM", name) {
                    StaticSoundData::from_cursor(Cursor::new(bytes))
                        .context("failed to decode AVG32 BGM")?
                } else {
                    self.cd_tracks
                        .get_or_insert_with(|| discover_cd_tracks(resources.root()))
                        .get(&parse_cd_track(name)?)
                        .map(decode_cd_track)
                        .transpose()?
                        .ok_or_else(|| anyhow::anyhow!("AVG32 CD track {name} is unavailable"))?
                };
                let data = looped.then(|| data.loop_region(..)).unwrap_or(data);
                if let Some(mut previous) = self.bgm.take() {
                    previous.stop(Tween::default());
                }
                if let Some(manager) = self.manager.as_mut() {
                    self.bgm = Some(manager.play(data).context("failed to play AVG32 BGM")?);
                }
            }
            VmAction::PlayAudio {
                kind: AudioKind::Voice { .. },
                name,
            } => {
                let voice_id = name
                    .parse::<u32>()
                    .with_context(|| format!("AVG32 voice id {name:?} is not numeric"))?;
                let data = decode_afs_voice(resources.root(), voice_id)?;
                if let Some(mut previous) = self.voice.take() {
                    previous.stop(Tween::default());
                }
                if let Some(manager) = self.manager.as_mut() {
                    self.voice = Some(manager.play(data).context("failed to play AVG32 voice")?);
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

    pub fn playing(&self, kind: &AudioKind) -> bool {
        #[cfg(not(target_os = "horizon"))]
        {
            let playing = |handle: &StaticSoundHandle| {
                matches!(handle.state(), kira::sound::PlaybackState::Playing)
            };
            match kind {
                AudioKind::Bgm { .. } => self.bgm.as_ref().is_some_and(playing),
                AudioKind::Wave { channel, .. } => channel.map_or_else(
                    || self.waves.values().any(playing),
                    |channel| self.waves.get(&channel).is_some_and(playing),
                ),
                AudioKind::Voice { .. } => self.voice.as_ref().is_some_and(playing),
                AudioKind::Effect | AudioKind::Movie { .. } => false,
            }
        }
        #[cfg(target_os = "horizon")]
        {
            let _ = kind;
            false
        }
    }
}

#[cfg(not(target_os = "horizon"))]
fn decode_afs_voice(game_root: &std::path::Path, voice_id: u32) -> Result<StaticSoundData> {
    let group = (voice_id >> 16) & 0xff;
    let index = (voice_id & 0xffff) as usize;
    let parent = game_root
        .parent()
        .ok_or_else(|| anyhow::anyhow!("AVG32 game root has no voice-data parent"))?;
    let path = parent
        .join("vair_for_win_mixer")
        .join(format!("VOICE{group:02}.AFS"));
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read AVG32 voice archive {}", path.display()))?;
    if bytes.get(..3) != Some(b"AFS") {
        bail!("AVG32 voice archive {} is not AFS", path.display());
    }
    let count = le_u32(&bytes, 4)? as usize;
    if index >= count {
        bail!("AVG32 voice {voice_id:#x} is outside {} entries", count);
    }
    let entry = 8usize
        .checked_add(
            index
                .checked_mul(8)
                .ok_or_else(|| anyhow::anyhow!("AFS index overflow"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("AFS table offset overflow"))?;
    let offset = le_u32(&bytes, entry)? as usize;
    let length = le_u32(&bytes, entry + 4)? as usize;
    let adx = bytes
        .get(offset..offset.saturating_add(length))
        .ok_or_else(|| anyhow::anyhow!("AVG32 AFS entry lies outside archive"))?;
    let (sample_rate, pcm) = decode_adx(adx)?;
    pcm_to_wav(sample_rate, 1, &pcm)
}

#[cfg(not(target_os = "horizon"))]
fn decode_adx(adx: &[u8]) -> Result<(u32, Vec<i16>)> {
    if adx.get(..2) != Some(&[0x80, 0x00]) {
        bail!("AVG32 voice entry is not CRI ADX");
    }
    let data_offset = usize::from(be_u16(adx, 2)?)
        .checked_add(4)
        .ok_or_else(|| anyhow::anyhow!("ADX data offset overflow"))?;
    let sample_rate = be_u32(adx, 8)?;
    let sample_count = be_u32(adx, 12)? as usize;
    let mut output = Vec::with_capacity(sample_count);
    let mut cursor = data_offset;
    let mut previous = 0i32;
    let mut previous_previous = 0i32;
    while output.len() < sample_count {
        let scale = i32::from(be_u16(adx, cursor)?);
        cursor = cursor
            .checked_add(2)
            .ok_or_else(|| anyhow::anyhow!("ADX cursor overflow"))?;
        let packed = adx
            .get(cursor..cursor + 16)
            .ok_or_else(|| anyhow::anyhow!("truncated ADX block"))?;
        cursor += 16;
        for byte in packed {
            for nibble in [byte >> 4, byte & 0x0f] {
                let nibble = if nibble & 8 != 0 {
                    i32::from(nibble) - 16
                } else {
                    i32::from(nibble)
                };
                let sample = ((nibble * scale * 0x7f00 + 0x7298 * previous
                    - 0x3350 * previous_previous)
                    >> 14)
                    .clamp(i32::from(i16::MIN), i32::from(i16::MAX));
                previous_previous = previous;
                previous = sample;
                output.push(sample as i16);
                if output.len() == sample_count {
                    break;
                }
            }
            if output.len() == sample_count {
                break;
            }
        }
    }
    Ok((sample_rate, output))
}

#[cfg(not(target_os = "horizon"))]
fn pcm_to_wav(sample_rate: u32, channels: u16, samples: &[i16]) -> Result<StaticSoundData> {
    let byte_count = samples
        .len()
        .checked_mul(2)
        .ok_or_else(|| anyhow::anyhow!("PCM payload is too large"))?;
    let mut wav = Vec::with_capacity(44 + byte_count);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + byte_count) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
    wav.extend_from_slice(&(channels * 2).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(byte_count as u32).to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    StaticSoundData::from_cursor(Cursor::new(wav)).context("decode generated AVG32 voice WAV")
}

#[cfg(not(target_os = "horizon"))]
fn le_u32(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        bytes
            .get(at..at + 4)
            .ok_or_else(|| anyhow::anyhow!("truncated little-endian u32"))?
            .try_into()
            .expect("four bytes"),
    ))
}

#[cfg(not(target_os = "horizon"))]
fn be_u16(bytes: &[u8], at: usize) -> Result<u16> {
    Ok(u16::from_be_bytes(
        bytes
            .get(at..at + 2)
            .ok_or_else(|| anyhow::anyhow!("truncated big-endian u16"))?
            .try_into()
            .expect("two bytes"),
    ))
}

#[cfg(not(target_os = "horizon"))]
fn be_u32(bytes: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_be_bytes(
        bytes
            .get(at..at + 4)
            .ok_or_else(|| anyhow::anyhow!("truncated big-endian u32"))?
            .try_into()
            .expect("four bytes"),
    ))
}

#[cfg(not(target_os = "horizon"))]
fn parse_cd_track(name: &str) -> Result<i32> {
    name.trim()
        .parse()
        .with_context(|| format!("AVG32 CD track name {name:?} is not numeric"))
}

#[cfg(not(target_os = "horizon"))]
fn discover_cd_tracks(game_root: &std::path::Path) -> BTreeMap<i32, CdTrack> {
    let Some(parent) = game_root.parent() else {
        return BTreeMap::new();
    };
    [parent.join("orange"), parent.join("blue")]
        .into_iter()
        .find_map(|directory| {
            let tracks = parse_ccd(&directory).ok()?;
            (!tracks.is_empty()).then_some(tracks)
        })
        .unwrap_or_default()
}

#[cfg(not(target_os = "horizon"))]
fn parse_ccd(directory: &std::path::Path) -> Result<BTreeMap<i32, CdTrack>> {
    let ccd = std::fs::read_to_string(directory.join("IMAGE.CCD"))
        .with_context(|| format!("read CloneCD descriptor in {}", directory.display()))?;
    let mut entries = BTreeMap::new();
    let mut point = None;
    for line in ccd.lines() {
        let line = line.trim();
        if line.starts_with("[Entry ") {
            point = None;
        } else if let Some(value) = line.strip_prefix("Point=0x") {
            point = i32::from_str_radix(value, 16).ok();
        } else if let Some(value) = line.strip_prefix("PLBA=") {
            if let (Some(point), Ok(lba)) = (point, value.parse::<usize>()) {
                entries.insert(point, lba);
            }
        }
    }
    let lead_out = entries.get(&0xa2).copied().unwrap_or_default();
    let image = directory.join("IMAGE.img");
    let mut tracks = BTreeMap::new();
    for track in 2..=99 {
        let Some(&start) = entries.get(&track) else {
            continue;
        };
        let end = ((track + 1)..=99)
            .find_map(|next| entries.get(&next).copied())
            .unwrap_or(lead_out);
        if end > start {
            tracks.insert(
                track,
                CdTrack {
                    image: image.clone(),
                    first_sector: start,
                    sector_count: end - start,
                },
            );
        }
    }
    Ok(tracks)
}

#[cfg(not(target_os = "horizon"))]
fn decode_cd_track(track: &CdTrack) -> Result<StaticSoundData> {
    const CD_SECTOR_BYTES: usize = 2352;
    let start = track
        .first_sector
        .checked_mul(CD_SECTOR_BYTES)
        .ok_or_else(|| anyhow::anyhow!("CD track start overflows"))?;
    let length = track
        .sector_count
        .checked_mul(CD_SECTOR_BYTES)
        .ok_or_else(|| anyhow::anyhow!("CD track length overflows"))?;
    let mut image = std::fs::File::open(&track.image)
        .with_context(|| format!("open CD image {}", track.image.display()))?;
    image.seek(SeekFrom::Start(start as u64))?;
    let mut data = vec![0; length];
    image
        .read_exact(&mut data)
        .context("CD image ended inside requested audio track")?;
    // Red Book audio is exactly 44.1 kHz, signed 16-bit little-endian stereo.
    // Kira's public decoder accepts WAV, so wrap the sector payload rather
    // than relying on its private sample-frame representation.
    let wav = wrap_pcm_as_wav(44_100, 2, 16, &data);
    StaticSoundData::from_cursor(Cursor::new(wav)).context("decode raw CD-DA track")
}

/// Wraps already-PCM-encoded bytes (little-endian, interleaved, as both AVI
/// and Red Book audio already store them) in a minimal WAV header so Kira's
/// public decoder can load them without a private sample-frame API.
#[cfg(not(target_os = "horizon"))]
fn wrap_pcm_as_wav(sample_rate: u32, channels: u16, bits_per_sample: u16, data: &[u8]) -> Vec<u8> {
    let block_align = channels * (bits_per_sample / 8);
    let mut wav = Vec::with_capacity(44 + data.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&((36 + data.len()) as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(data);
    wav
}
