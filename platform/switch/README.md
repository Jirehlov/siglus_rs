# Switch platform port

The Switch frontend reuses the existing `siglus_scene_vm` host, Scene VM,
resource loader, script runtime, image manager, and audio engine.  It replaces
only the desktop platform layer: libnx owns lifecycle/input/audio, while a
Switch-native deko3d backend presents the engine's `RenderFrame`.  The Switch
artifact contains no `winit`, `wgpu`, or desktop-window dependency. GLSL
sources are compiled with devkitPro's `uam` into native `.dksh` modules and
embedded in the NRO RomFS.

`./platform/switch/build_switch.sh` produces:

`platform/switch/runtime/siglus_switch.nro`

The native runtime links the Switch-targeted `libsiglus_scene_vm.a` directly:
libnx startup mounts SD storage, the existing host opens GameData and advances
the VM, controller input reaches the shared VM input API, the Kira Switch
backend feeds audren, and deko3d presents the composed frame through the new
vertex/fragment shaders. Desktop WGPU code stays target-gated and is not part
of this build.

## Deploying GameData

Copy the NRO to `sdmc:/switch/siglus_rs/siglus_switch.nro` and place the
unmodified game directory at `sdmc:/switch/siglus_rs/game/`. The runtime passes
that directory to the existing `SiglusHostConfig`, so standard engine resource
lookup remains responsible for locating the game's `Gameexe` data, `Scene.pck`,
archives, movies, and audio assets.

For a self-contained NRO, use:

```sh
./platform/switch/package_game_nro.sh /path/to/game /path/to/game.nro
```

The script copies the supplied game directory into a temporary RomFS staging
area, builds the normal Switch runtime and embeds the copied assets in the
output NRO. The original game directory is never modified. At startup the
runtime prefers `romfs:/game` and only falls back to the SD-card path above
when no embedded `Scene.pck` exists. Full game packages larger than 4 GiB need
an exFAT-formatted SD card because FAT32 cannot hold the resulting NRO.

Some emulators cannot mount a multi-gigabyte NRO RomFS: their storage adapter
uses a signed 32-bit buffer range even though the NRO/RomFS format uses
64-bit offsets. For that case, make an SD-card deployment bundle instead:

```sh
./platform/switch/package_game_nro.sh --sdmc /path/to/game /path/to/siglus_rs
```

It produces `siglus_switch.nro` and a complete `game/` directory under the
specified `siglus_rs` directory. Copy its contents to
`sdmc:/switch/siglus_rs/`. This uses the same engine and native renderer; only
the asset storage location changes.

## Toolchain

Install devkitA64, libnx, deko3d, and switch-tools.  The expected installation prefix is `/opt/devkitpro`; the build script sets the corresponding tool paths.

## Layout

- `runtime/` — deployable libnx/deko3d frontend linked to the existing Rust engine; `source/*.glsl` are the new Switch shader sources.
- `build_switch.sh` — packages `runtime/siglus_switch.nro`.
- `package_game_nro.sh` — embeds a chosen game directory in a standalone NRO.
- `window/` — retained Rust/libnx CPU bootstrap experiment; it is not the production NRO path.
- `compat/`, `std/`, and `patches/` — Horizon compatibility work required to compile the existing engine dependency graph.
- `ROADMAP.md` — historical staged-port notes.
