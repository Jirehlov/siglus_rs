//! Bounded, headless AVG32 bytecode smoke probe.
//!
//! By default this only validates the resource/scene/VM boundary on one
//! scenario (no renderer). Set `AVG32_RENDER=1` to also feed every yielded
//! action through `Avg32Renderer`, which additionally exercises the graphics
//! buffer bank — useful for regression-checking scenes that crashed the
//! desktop player (buffer-empty errors, decode desyncs, and so on) without
//! needing a window.

use std::env;

use anyhow::{Context, Result, bail};
use avg32::game::Avg32Game;
use avg32::render::Avg32Renderer;
use avg32::vm::{Avg32Program, Avg32Vm, Input, VmAction, VmStop};

fn main() -> Result<()> {
    let root = env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: avg32_vm_probe <game-root> [scene-name]"))?;
    let scene_name = env::args().nth(2);
    let game = Avg32Game::open(&root)?;
    let scene_name = scene_name
        .or_else(|| game.configured_start_scene().map(str::to_owned))
        .ok_or_else(|| anyhow::anyhow!("AVG32 Gameexe.ini has no readable SEEN_START"))?;
    if scene_name == "SWEEP" {
        // Replays every scene independently (from its own entry point, with
        // blank flags) through the VM and, if AVG32_RENDER is set, the
        // renderer too. Bounded and best-effort: it exists to surface
        // decode/render regressions across an entire installed title in one
        // pass, not to fully play the game.
        let render = env::var_os("AVG32_RENDER").is_some();
        let skip_waits = true;
        let mut failures = 0usize;
        let mut names: Vec<String> = game.scene_names().map(str::to_owned).collect();
        names.sort();
        for name in names {
            let Ok(bytes) = game.read_scene(&name) else {
                continue;
            };
            let Ok(program) = Avg32Program::parse(bytes) else {
                continue;
            };
            let code = program.code().to_vec();
            let mut vm = Avg32Vm::new(program).with_extended_text();
            let mut renderer = if render {
                Some(Avg32Renderer::new()?)
            } else {
                None
            };
            let mut input = Input::Skip;
            let mut error = None;
            for _ in 0..1_024 {
                let outcome = match vm.run(input, 4_096) {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        error = Some(format!("{err}; pc={:#x}", vm.pc()));
                        break;
                    }
                };
                if let (Some(renderer), VmStop::Yield(action)) = (renderer.as_mut(), &outcome)
                    && let Err(err) = renderer.apply(action, &game.resources, &game.config)
                {
                    error = Some(format!(
                        "{err:#}; pc={:#x}: {action:?}",
                        vm.last_opcode_pc()
                    ));
                    break;
                }
                match outcome {
                    VmStop::Ended | VmStop::Yield(VmAction::End) => break,
                    VmStop::Yield(VmAction::ChangeScene { .. }) => break,
                    VmStop::Yield(VmAction::LoadArea { .. }) => input = Input::None,
                    VmStop::Yield(VmAction::WaitForPointer | VmAction::WaitForInput { .. }) => {
                        break;
                    }
                    VmStop::Yield(VmAction::Wait { .. }) if skip_waits => input = Input::Skip,
                    VmStop::Yield(VmAction::Wait { .. }) => break,
                    VmStop::Yield(_) => input = Input::None,
                }
            }
            let _ = code;
            if let Some(error) = error {
                failures += 1;
                println!("{name}: {error}");
            }
        }
        println!("sweep done, {failures} scene(s) errored");
        return Ok(());
    }
    let program = Avg32Program::parse(game.read_scene(&scene_name)?)
        .with_context(|| format!("failed to parse {scene_name}"))?;
    let code = program.code().to_vec();
    if let Some(offset) = env::var("AVG32_DUMP_PC")
        .ok()
        .and_then(|value| usize::from_str_radix(value.trim_start_matches("0x"), 16).ok())
    {
        let start = offset.saturating_sub(32);
        let end = (offset + 96).min(code.len());
        println!(
            "{scene_name} bytecode {start:#x}..{end:#x}: {:02x?}",
            &code[start..end]
        );
        return Ok(());
    }
    let mut vm = Avg32Vm::new(program).with_extended_text();
    let mut pointers = env::var("AVG32_POINTER")
        .ok()
        .map(|value| {
            value
                .split(';')
                .filter_map(|pair| {
                    let (x, y) = pair.split_once(',')?;
                    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
                })
                .collect::<Vec<(i32, i32)>>()
        })
        .unwrap_or_default()
        .into_iter();
    let mut input = Input::Skip;
    let trace = env::var_os("AVG32_TRACE").is_some();
    let skip_waits = env::var_os("AVG32_SKIP_WAITS").is_some();
    let render = env::var_os("AVG32_RENDER").is_some();
    let mut renderer = render.then(Avg32Renderer::new).transpose()?;
    let button = env::var("AVG32_BUTTON")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    for step in 0..1_024 {
        let outcome = vm.run(input, 4_096).map_err(|error| {
            let pc = vm.pc();
            let start = pc.saturating_sub(16);
            let end = (pc + 16).min(code.len());
            anyhow::anyhow!("{error}; bytecode near {pc:#x}: {:02x?}", &code[start..end])
        })?;
        if let (Some(renderer), VmStop::Yield(action)) = (renderer.as_mut(), &outcome) {
            renderer
                .apply(action, &game.resources, &game.config)
                .with_context(|| {
                    format!(
                        "{scene_name}: render action at scene bytecode {:#x}: {action:?}",
                        vm.last_opcode_pc()
                    )
                })?;
        }
        if trace {
            let values = (0..=200)
                .map(|index| (index, vm.flags().value(index)))
                .filter(|(_, value)| *value != 0)
                .collect::<Vec<_>>();
            eprintln!(
                "{scene_name} pc={:#x}: {outcome:?}; nonzero V[0..200]={values:?}",
                vm.last_opcode_pc()
            );
        }
        match outcome {
            VmStop::Ended | VmStop::Yield(VmAction::End) => {
                println!("{scene_name}: ended after {step} yielded actions");
                return Ok(());
            }
            VmStop::Yield(VmAction::ChangeScene { scene, .. }) => {
                println!(
                    "{scene_name}: reached scene transition to {scene:03} after {step} yielded actions"
                );
                return Ok(());
            }
            VmStop::Yield(VmAction::LoadArea { definition, .. }) => {
                let map = avg32::ard::AreaMap::parse(&game.resources.read("ARD", &definition)?)?;
                println!(
                    "{scene_name}: loaded area map {definition}; areas={:?}",
                    map.area_ids()
                        .into_iter()
                        .map(|id| {
                            let point = map.first_point_for(id);
                            (id, point, point.map(|(x, y)| map.area_at(x, y)))
                        })
                        .collect::<Vec<_>>()
                );
                vm.set_area_map(map);
                input = Input::None;
            }
            VmStop::Yield(VmAction::WaitForPointer) if pointers.len() > 0 => {
                let (x, y) = pointers.next().expect("guard checked pointers");
                input = Input::Pointer { x, y, button };
            }
            VmStop::Yield(VmAction::WaitForPointer | VmAction::WaitForInput { .. }) => {
                println!("{scene_name}: reached interactive wait after {step} yielded actions");
                return Ok(());
            }
            VmStop::Yield(VmAction::Wait { .. }) if skip_waits => {
                input = Input::Skip;
            }
            VmStop::Yield(VmAction::Wait { microseconds, .. }) => {
                println!(
                    "{scene_name}: reached timed wait ({microseconds}us) after {step} yielded actions"
                );
                return Ok(());
            }
            VmStop::Yield(_) => input = Input::None,
        }
    }
    bail!("{scene_name}: exceeded 1024 yielded actions without an ending or scene transition")
}
