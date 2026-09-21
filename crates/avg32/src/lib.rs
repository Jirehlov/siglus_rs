//! Format-first support for VisualArt's pre-RealLive AVG32 games.
//!
//! AVG32 is a distinct virtual machine from SiglusEngine.  In particular its
//! `SEEN.TXT` scenarios are `TPC32` bytecode stored in `PACL` archives, and its
//! primary bitmap formats are `PDT10` and `PDT11`.  This crate deliberately
//! owns those formats instead of routing them through the Siglus scene VM.

pub mod animation;
pub mod animation_player;
pub mod ard;
pub mod archive;
pub mod audio;
pub mod config;
pub mod game;
pub mod pdt;
pub mod render;
pub mod resource;
pub mod runtime;
pub mod scene;
pub mod surface;
pub mod text;
pub mod vm;

pub use archive::{PaclArchive, PaclEntry};
pub use game::{Avg32Game, EngineKind, GameLayout, detect_game_root};
pub use pdt::{PdtImage, decode_pdt};
pub use scene::{Avg32SceneHeader, SceneMenu, SceneSubmenu, SceneValue, ValueKind};
