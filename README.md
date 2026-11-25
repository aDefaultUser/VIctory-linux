# VIctory fork for Linux

This is a fork of [VIctory](https://github.com/Marvisak/VIctory), which runs
an Xbox Series title natively on Windows from files you supply. The fork runs
the same Windows build on Linux through Wine or Proton, and adds rendering and
performance work for that title.

The repository holds source code only. It contains no game files, code or
assets, and you need your own copy of the game. The project is not affiliated
with or endorsed by the game's developer or publisher.

## Status

Not playable yet. Tested on one Linux machine with a Radeon RX 9070 XT,
Wine 11.22 and Proton Experimental:

- The title boots.
- The intro movie plays at 30 fps (measured with frame capture turned off).
- Some characters and scenes of the opening story render.
- Missing or wrong rendering and GPU resets are still being investigated.

Exact versions and known problems are kept in [docs/linux.md](docs/linux.md).

## Run on Linux

You need:

- Rust (the version in `rust-toolchain.toml`) with the
  `x86_64-pc-windows-msvc` target, `clang-cl`, `lld-link`, and an MSVC CRT and
  Windows SDK (`xwin` can fetch both)
- Wine, or Proton's own Wine with `VICTORY_WINE=proton`
- Proton Experimental, for its vkd3d-proton and DXVK DLLs
- bubblewrap
- a native `d3dcompiler_47.dll` (winetricks keeps one in its cache)

Build the Windows executable and start the title in a sandbox without network:

```sh
cargo build --release -p victory --target x86_64-pc-windows-msvc
PROTON_WINE_LIBS="<Proton Experimental>/files/lib/wine" \
  scripts/run-linux.sh target/x86_64-pc-windows-msvc/release/victory.exe \
  <game folder> <title exe name>
```

The script's header lists its settings. Two are worth knowing first:

- `VICTORY_WARM=1` fills the shader caches before the launch, and
  `VICTORY_WARM=only` does that and stops. It translates the shader sources
  that earlier warm runs captured on your machine, and builds compute
  pipelines with `--pipelines` (the default). Graphics pipelines and streamed
  textures are created as the title reaches them.
- `VKD3D_DLLS` points at another vkd3d-proton build. Three of the title's
  compute shaders stall the compiler in stock vkd3d-proton. A build with the
  matching dxil-spirv backport makes all three. That build is not bundled
  here, and [docs/linux.md](docs/linux.md) names the fix.

## Run on Windows

You need Windows 10 or 11 x64, a CPU with AVX2, a Direct3D 12 GPU with shader
model 6.6 and mesh shaders, and Rust.

```powershell
copy victory.example.toml victory.toml   # point it at your files, outside the repository
cargo run -p victory -- config           # check the paths
cargo build --release -p victory
target\release\victory.exe run <game folder>\<title exe> --execute
```

Saves, settings and `crashes.log` go to `%LOCALAPPDATA%\victory\data`.

Input comes from XInput pads. With none connected, the keyboard acts as a pad
while the window has focus:

| Keys | Pad |
|------|-----|
| W A S D / I J K L | left / right stick |
| arrows | D-pad |
| Enter, Backspace, X, Y | A, B, X, Y |
| Q / E, Z / C | shoulders, triggers |
| F / H | stick clicks |
| Esc, Tab | Menu, View |

## Commands and debugging

`victory --help` lists the commands, and `victory <command> --help` shows
their options. `VICTORY_LOG=<target>=debug,info` raises logging.

A diagnostics build adds frame capture and debug switches:

```sh
cargo build --profile diag -p victory --features diagnostics
```

Each `VICTORY_*` switch is described in a comment where the code reads it.
[docs/linux.md](docs/linux.md) shows how to capture frames.

## Documentation

- [docs/linux.md](docs/linux.md): Wine and Proton setup, the warm-up, known
  problems
- [docs/architecture.md](docs/architecture.md): how the crates fit together
- [docs/asset-policy.md](docs/asset-policy.md): what may and may not be
  committed
- [docs/fork-ray-tracing.md](docs/fork-ray-tracing.md): the ray tracing work
- [AGENTS.md](AGENTS.md): working conventions and validation for contributors
  and coding agents

Run `cargo xtask install-hooks` once after cloning so every commit is checked
against the asset policy.

## Credits

- [VIctory](https://github.com/Marvisak/VIctory) by Marvisak and its
  contributors is the base of this fork: the loader, the runtime services, the
  Direct3D 12 back end and the shader recompiler. Upstream history is kept in
  this repository.
- [XCL](https://github.com/ivall/xcl) by ivall and its contributors
  (GPL-2.0-only) served as a reference for how the title behaves under another
  layer. Fixes it inspired were written anew in VIctory's code.
- Code carried over from other forks and pull requests:
  - the guard over every view of a memory page, by myokex
    ([victory-amd](https://github.com/myokex/victory-amd) `c6dc4d8`), which
    adapts pull request #115 by Spexivus, its correction by joroyt, and part
    of VIctory #1122 by tightfall
  - reading kernel reads into title memory beside it, by Joshua Odom
    (`10ea38e`)
  - descriptor mirroring after pull request #305 by Flyander
  - fixes for texture levels on demand from pull request #747 by AscilCH
  - pull requests #670 and #648 by Jonirulah
  - branches of triippywhale's and asdaasdsaddf's forks
  - DirectStorage page lists taken at the title's submit, after JappertjeV's
    analysis ([PastiiJay/klbf-lab-beta#3](https://github.com/PastiiJay/klbf-lab-beta/issues/3))
    and PastiiJay/klbf-lab
- Every other contribution is credited to its author in the git history.

## License

GPL-3.0-or-later, as upstream. See [LICENSE](LICENSE). The BCPack format
tables are the one exception, described in
[docs/asset-policy.md](docs/asset-policy.md).
