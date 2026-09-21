//! AVG32 `#DIRC.*` resource routing.
//!
//! Unlike Siglus, AVG32 chooses a directory and optional PACL container for
//! every filename extension in `Gameexe.ini`.  Keeping this table explicit is
//! essential: a title can place scenarios, graphics, audio, and animations in
//! different loose directories or archives without changing the VM.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use siglus_assets::gameexe::GameexeConfig;

use crate::archive::PaclArchive;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceRoute {
    pub directory: PathBuf,
    pub archive: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Avg32Resources {
    root: PathBuf,
    routes: BTreeMap<String, ResourceRoute>,
}

impl Avg32Resources {
    pub fn from_gameexe(root: impl AsRef<Path>, gameexe: &GameexeConfig) -> Self {
        let root = root.as_ref().to_path_buf();
        let mut routes = BTreeMap::new();
        for entry in &gameexe.entries {
            let Some(extension) = entry.key.strip_prefix("DIRC.") else {
                continue;
            };
            if extension.is_empty() {
                continue;
            }
            if let Some(route) = parse_route(&root, &entry.value) {
                routes.insert(extension.to_ascii_uppercase(), route);
            }
        }
        Self { root, routes }
    }

    pub fn route(&self, extension: &str) -> Option<&ResourceRoute> {
        self.routes
            .get(&extension.trim_start_matches('.').to_ascii_uppercase())
    }

    /// Reads `name` from the route selected by `extension`. The caller owns
    /// extension semantics; passing `PDT`, `TXT`, `ANM`, `ARD`, `CUR`, or
    /// `WAV` mirrors AVG32's original file manager.
    pub fn read(&self, extension: &str, name: &str) -> Result<Vec<u8>> {
        let extension = extension.trim_start_matches('.').to_ascii_uppercase();
        let route = self.route(&extension).ok_or_else(|| {
            anyhow::anyhow!("avg32: Gameexe.ini has no DIRC route for .{extension}")
        })?;
        let name = with_extension(name, &extension);
        if let Some(archive_name) = &route.archive {
            let archive_path =
                find_case_insensitive(&route.directory, archive_name).ok_or_else(|| {
                    anyhow::anyhow!("avg32: archive {} was not found", archive_name.display())
                })?;
            return PaclArchive::from_path(&archive_path)?
                .read(&name)
                .with_context(|| {
                    format!(
                        "failed to open AVG32 resource {name} in {}",
                        archive_path.display()
                    )
                });
        }
        let path = find_case_insensitive(&route.directory, Path::new(&name)).ok_or_else(|| {
            anyhow::anyhow!(
                "avg32: resource {name} was not found in {}",
                route.directory.display()
            )
        })?;
        std::fs::read(&path)
            .with_context(|| format!("failed to read AVG32 resource {}", path.display()))
    }

    /// Routes with `=N` are loose files. The returned paths are useful to an
    /// audio backend that streams rather than buffering a whole file.
    pub fn loose_path(&self, extension: &str, name: &str) -> Result<PathBuf> {
        let extension = extension.trim_start_matches('.').to_ascii_uppercase();
        let route = self.route(&extension).ok_or_else(|| {
            anyhow::anyhow!("avg32: Gameexe.ini has no DIRC route for .{extension}")
        })?;
        if route.archive.is_some() {
            bail!("avg32: .{extension} is stored in a PACL archive, not a loose file");
        }
        let name = with_extension(name, &extension);
        find_case_insensitive(&route.directory, Path::new(&name)).ok_or_else(|| {
            anyhow::anyhow!(
                "avg32: resource {name} was not found in {}",
                route.directory.display()
            )
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn parse_route(root: &Path, value: &str) -> Option<ResourceRoute> {
    let (directory, storage) = value.split_once('=')?;
    let directory = unquote(directory.trim());
    if directory.is_empty() {
        return None;
    }
    let storage = storage.trim();
    let mode = storage.as_bytes().first().copied()?.to_ascii_uppercase();
    let archive = storage
        .get(1..)
        .and_then(|tail| tail.trim().strip_prefix(':'))
        .map(str::trim)
        .map(unquote)
        .filter(|name| !name.is_empty())
        .map(PathBuf::from);
    // P means a PACL container. N describes a loose-name route; its optional
    // catalogue name is metadata, not an archive to read.
    Some(ResourceRoute {
        directory: root.join(directory),
        archive: (mode == b'P').then_some(archive).flatten(),
    })
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value)
}

fn with_extension(name: &str, extension: &str) -> String {
    if Path::new(name).extension().is_some() {
        name.to_owned()
    } else {
        format!("{name}.{extension}")
    }
}

fn find_case_insensitive(directory: &Path, wanted: &Path) -> Option<PathBuf> {
    let mut current = directory.to_path_buf();
    for component in wanted.components() {
        let component = component.as_os_str().to_str()?;
        current = std::fs::read_dir(&current)
            .ok()?
            .flatten()
            .find_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .filter(|name| name.eq_ignore_ascii_case(component))
                    .map(|_| entry.path())
            })?;
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_loose_and_pacl_routes() {
        let root = Path::new("/game");
        assert_eq!(
            parse_route(root, "\"PDT\" =N:\"ALLPDT.PDL\"").unwrap(),
            ResourceRoute {
                directory: PathBuf::from("/game/PDT"),
                archive: None,
            }
        );
        assert_eq!(
            parse_route(root, "\"DAT\" =P:\"SEEN.TXT\"").unwrap(),
            ResourceRoute {
                directory: PathBuf::from("/game/DAT"),
                archive: Some(PathBuf::from("SEEN.TXT")),
            }
        );
    }

    #[test]
    fn keeps_an_explicit_extension() {
        assert_eq!(with_extension("seen002", "TXT"), "seen002.TXT");
        assert_eq!(with_extension("seen002.txt", "TXT"), "seen002.txt");
    }
}
