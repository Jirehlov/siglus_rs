//! Game-root detection and resource opening for AVG32 installations.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use encoding_rs::SHIFT_JIS;
use siglus_assets::gameexe::GameexeConfig;

use crate::archive::PaclArchive;
use crate::config::Avg32Config;
use crate::pdt::{PdtImage, decode_pdt};
use crate::resource::Avg32Resources;
use crate::scene::Avg32SceneHeader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineKind {
    Avg32,
    Siglus,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameLayout {
    pub root: PathBuf,
    pub kind: EngineKind,
    pub gameexe_ini: Option<PathBuf>,
    pub seen_archive: Option<PathBuf>,
    pub pdt_root: Option<PathBuf>,
}

/// Distinguishes AVG32 from Siglus by on-disk formats, not publisher or title.
pub fn detect_game_root(root: impl AsRef<Path>) -> Result<GameLayout> {
    let root = root
        .as_ref()
        .canonicalize()
        .with_context(|| format!("invalid game root {}", root.as_ref().display()))?;
    if !root.is_dir() {
        bail!("{} is not a game directory", root.display());
    }
    let gameexe_ini = find_case_insensitive(&root, "Gameexe.ini");
    if find_case_insensitive(&root, "Scene.pck").is_some()
        || find_case_insensitive(&root, "Gameexe.dat").is_some()
    {
        return Ok(GameLayout {
            root,
            kind: EngineKind::Siglus,
            gameexe_ini,
            seen_archive: None,
            pdt_root: None,
        });
    }

    let seen_archive = [
        root.join("DAT/SEEN.TXT"),
        root.join("DAT/Seen.txt"),
        root.join("SEEN.TXT"),
        root.join("Seen.txt"),
    ]
    .into_iter()
    .find(|path| path.is_file());
    let pdt_root = [root.join("PDT"), root.join("DAT/PDT")]
        .into_iter()
        .find(|path| path.is_dir());
    let is_avg32_seen = seen_archive
        .as_deref()
        .and_then(|path| std::fs::read(path).ok())
        .is_some_and(|bytes| bytes.starts_with(b"PACL"));
    if gameexe_ini.is_some() && is_avg32_seen {
        return Ok(GameLayout {
            root,
            kind: EngineKind::Avg32,
            gameexe_ini,
            seen_archive,
            pdt_root,
        });
    }
    Ok(GameLayout {
        root,
        kind: EngineKind::Unknown,
        gameexe_ini,
        seen_archive,
        pdt_root,
    })
}

#[derive(Debug)]
pub struct Avg32Game {
    pub layout: GameLayout,
    pub gameexe: GameexeConfig,
    pub config: Avg32Config,
    pub resources: Avg32Resources,
    seen: PaclArchive,
}

impl Avg32Game {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let layout = detect_game_root(root)?;
        if layout.kind != EngineKind::Avg32 {
            bail!(
                "{} is not an AVG32 game root ({:?})",
                layout.root.display(),
                layout.kind
            );
        }
        let gameexe_path = layout
            .gameexe_ini
            .as_ref()
            .expect("AVG32 layout has Gameexe.ini");
        let gameexe = GameexeConfig::from_text(&decode_gameexe_ini(
            &std::fs::read(gameexe_path)
                .with_context(|| format!("failed to read {}", gameexe_path.display()))?,
        )?);
        let seen_path = layout
            .seen_archive
            .as_ref()
            .expect("AVG32 layout has SEEN.TXT");
        let seen = PaclArchive::from_path(seen_path)?;
        let resources = Avg32Resources::from_gameexe(&layout.root, &gameexe);
        let config = Avg32Config::from_gameexe(&gameexe);
        Ok(Self {
            layout,
            gameexe,
            config,
            resources,
            seen,
        })
    }

    pub fn scene_names(&self) -> impl Iterator<Item = &str> {
        self.seen.entries().iter().map(|entry| entry.name.as_str())
    }

    pub fn read_scene(&self, name: &str) -> Result<Vec<u8>> {
        self.seen.read(name)
    }

    pub fn scene_header(&self, name: &str) -> Result<Avg32SceneHeader> {
        Avg32SceneHeader::parse(&self.read_scene(name)?)
            .with_context(|| format!("failed to parse AVG32 scene {name}"))
    }

    /// Returns the `#SEEN_START` scene when the configured numeric scene has
    /// one of AVG32's conventional `SEENddd.TXT` names in the scenario archive.
    pub fn configured_start_scene(&self) -> Option<&str> {
        let scene = self.gameexe.get_usize("SEEN_START")?;
        let candidates = [
            format!("SEEN{scene:03}.TXT"),
            format!("SEEN{scene:04}.TXT"),
            format!("SEEN{scene}.TXT"),
        ];
        candidates
            .iter()
            .find_map(|name| self.seen.entry(name).map(|entry| entry.name.as_str()))
    }

    /// Loads a loose AVG32 PDT resource.  `PDT` graphics are intentionally
    /// decoded by this crate rather than by Siglus's unrelated G00 decoder.
    pub fn load_pdt(&self, name: &str) -> Result<PdtImage> {
        if let Ok(bytes) = self.resources.read("PDT", name) {
            return decode_pdt(&bytes);
        }
        let root = self
            .layout
            .pdt_root
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("AVG32 game has no PDT directory"))?;
        let requested = if Path::new(name).extension().is_some() {
            name.to_owned()
        } else {
            format!("{name}.PDT")
        };
        let path = find_case_insensitive(root, &requested)
            .ok_or_else(|| anyhow::anyhow!("AVG32 PDT {requested:?} was not found"))?;
        decode_pdt(
            &std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?,
        )
    }
}

fn decode_gameexe_ini(bytes: &[u8]) -> Result<String> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(text.to_owned());
    }
    let (text, _, had_errors) = SHIFT_JIS.decode(bytes);
    if had_errors {
        bail!("AVG32 Gameexe.ini is neither UTF-8 nor valid Shift-JIS");
    }
    Ok(text.into_owned())
}

fn find_case_insensitive(dir: &Path, wanted: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        entry
            .file_name()
            .to_str()
            .filter(|name| name.eq_ignore_ascii_case(wanted))
            .map(|_| entry.path())
    })
}
