# Running under Wine on Linux

`scripts/run-linux.sh` runs the Windows build of `victory.exe` under Proton's
Wine (or the system's) in a [bubblewrap](https://github.com/containers/bubblewrap)
sandbox without network. It has three commands that share one set of DLLs,
caches and settings:

```sh
scripts/run-linux.sh shaders <shader archives or folders>... [--jobs N] [--list FILE]
scripts/run-linux.sh warm --used [--jobs N]
scripts/run-linux.sh run <title folder> <title exe>
```

`shaders` and `warm` are `victory shaders` and `victory warm` (see the
README's "How to run it"), run ahead of time in the same sandbox. Both can be
stopped and pick up where they left off later.

## Tested result

One machine, November 2026: CachyOS, Radeon RX 9070 XT, RADV (Mesa 26.3.1),
Proton Experimental `experimental-11.0-20261112`, a private `wineserver`
build in place of Proton's (`RUN_LINUX_WINESERVER`), `/dev/ntsync`, an
app-local vkd3d-proton build matched to that Proton, and a native
`d3dcompiler_47.dll`. The `VICTORY_*` settings were the script's defaults
(below), on a Series X.

- **Opening:** a median of 23.1 frames per second over 14 samples during the
  heavy opening.
- **First room:** a median of 22.3 frames per second over six five-second
  samples. These runs started from a private checkpoint in a save the
  tester made.
- The camera responded to input.
- **Missing face textures remain.**
- A steady 30 frames per second has not been shown.

No other title, scene or machine has been tested. Logs, images and the save
are not published.

## What it needs

None of these ship with the repository. Point the script at your own copies.

- **bubblewrap** (`bwrap`).
- **The Windows build of `victory.exe`.** The script looks for
  `target/x86_64-pc-windows-msvc/diag/victory.exe` (`RUN_LINUX_EXE` names
  another). One way to cross-build it, with clang, lld and llvm installed:

  ```sh
  cargo install --locked cargo-xwin
  cargo xwin build --locked --profile diag -p victory --features diagnostics \
    --target x86_64-pc-windows-msvc
  ```

  The first build downloads Microsoft's CRT and SDK with
  [xwin](https://github.com/Jake-Shadle/xwin), which means accepting
  [Microsoft's license](https://go.microsoft.com/fwlink/?LinkId=2086102).
- **Proton** (`PROTON_WINE_LIBS`, its `files/lib/wine` folder). Proton's Wine
  runs the commands, and DXVK's `dxgi.dll` comes from it. With
  `RUN_LINUX_WINE=wine` the system's Wine runs them instead, and Proton still
  provides the DLLs.
- **vkd3d-proton** (`VKD3D_DLLS`, a folder with `d3d12.dll` and
  `d3d12core.dll`). The default is the copy inside Proton. The tested runs
  used a separate build matched to the Proton above.
- **A native `d3dcompiler_47.dll`** (`D3DCOMPILER`; default
  `~/.cache/winetricks/d3dcompiler_47/d3dcompiler_47.dll`, where winetricks
  keeps it). VIctory compiles HLSL at run time, and every tested run used the
  native compiler.
- **The `ntsync` kernel module** (`modprobe ntsync`). Without
  `/dev/ntsync`, every wait and signal goes through the one `wineserver`
  process. The script says so when the device is missing.
- **Your own dump of the title**, read-only. The script binds only the
  folder you name.

## Settings

The sandbox starts from an empty environment. Its `VICTORY_*` settings are
those of `scripts/play.ps1` without `-Safe`, plus:

| Setting | Value | Why |
|---|---|---|
| `VICTORY_PORT_DESCRIPTORS`, `VICTORY_PORT_CBV`, `VICTORY_PORT_GUARD`, `VICTORY_PORT_JOURNAL` | `1` | renderer ports of the tested runs |
| `VICTORY_PORT_CONSTANTS` | `0` | with `1`, one region opened but the picture was black |
| `VICTORY_PIPELINE_BOOK` | `1` | runs record their pipelines for `warm --used` |
| `VICTORY_OUTPUT` | `1920x1080` | the tested output size |
| `RADV_DEBUG` | `zerovram` | earlier runs drew the 3D world in one flat colour without it |

Change any of them with `NAME=value` arguments after the command's own
arguments, for example
`scripts/run-linux.sh run <folder> <exe> VICTORY_OUTPUT=2560x1440`. Arguments
can set any variable (`VKD3D_CONFIG=...` too). The last one for a name wins,
and an empty value removes the name from the sandbox (`VICTORY_THIN=`). A
`RADV_DEBUG` in your environment replaces the script's default. Any other
`VICTORY_*` variable in your environment is not passed on, and the script
says which ones it left out. Title arguments go after `--`.

Give `shaders` and `warm` the same arguments as `run`. Each `VICTORY_*` the
translator reads is part of the shader cache's key (`VICTORY_PORT_CBV=1` is
one), and `warm` remembers what it made per translator and driver. If a
setting or DLL differs, the run makes its own translations and pipelines
again rather than using the warm-up's.

`run` asks for a Series X (`--console series-x`). `RUN_LINUX_CONSOLE=series-s`
changes that.

## Where things are kept

Everything the commands write stays outside the repository and outside the
title's folder:

| Variable | Default | Holds |
|---|---|---|
| `RUN_LINUX_STATE` | `${XDG_DATA_HOME:-~/.local/share}/victory/linux` | the two below, unless they are set |
| `RUN_LINUX_PREFIX` | `$RUN_LINUX_STATE/prefix-proton` (`prefix` for system Wine) | the Wine prefix |
| `RUN_LINUX_DATA` | `$RUN_LINUX_STATE/data` | VIctory's data folder: title storage and saves |
| `RUN_LINUX_CACHE` | `${XDG_CACHE_HOME:-~/.cache}/victory/linux` | `victory/` (shader cache, pipeline book), `vkd3d/`, `mesa/` |

The title's folder, `victory.exe` and the DLLs are bound read-only. Inside
the sandbox, `victory.exe` sees `VICTORY_DATA_DIR=Z:\state\data` and
`VICTORY_CACHE_DIR=Z:\state\cache\victory`. `shaders --list FILE` writes
that file on the host.

### Existing saves

Saves live in `<data>/<title id>/saves`, the same layout as on Windows
(`%LOCALAPPDATA%\victory\data` there). To use saves you already have, copy
that `<title id>` folder into a data folder of its own and point
`RUN_LINUX_DATA` at it:

```sh
RUN_LINUX_DATA="$HOME/victory-saves-copy" scripts/run-linux.sh run ...
```

The run writes to that folder, so work on a copy and keep the original
somewhere else.

## Wine, Proton and the Wine server

`RUN_LINUX_WINE=proton` (the default) uses Proton's own Wine
(`PROTON_WINE_LIBS/../../bin/wine`). It draws through X11 (Xwayland), so the
script binds the X11 socket of `DISPLAY` and `XAUTHORITY` if set, and shares
the host's IPC namespace for X11 shared-memory images. `RUN_LINUX_WINE=wine`
uses the system's `wine` with an IPC namespace of its own and a separate
prefix.

`RUN_LINUX_WINESERVER=/path/to/wineserver` runs another `wineserver`, built
for that Proton's Wine, in place of Proton's. Wine starts the server beside
its own binary before it looks at `WINESERVER`, so the script mounts the file
over `files/bin/wineserver` inside the sandbox only. The Proton install is
not changed. This works only with Proton's Wine.

Every command ends by waiting for the Wine server (`wineserver -w`). A
prefix made by the first start is only written out when the server exits,
and the end of the sandbox would otherwise cut it off.

## Example

```sh
export PROTON_WINE_LIBS="$HOME/.local/share/Steam/steamapps/common/Proton - Experimental/files/lib/wine"
export VKD3D_DLLS="$HOME/vkd3d-proton-build"           # optional
export RUN_LINUX_WINESERVER="$HOME/wine-build/server/wineserver"  # optional

scripts/run-linux.sh shaders "$HOME/Games/Title/<shader archive>" --jobs 8
scripts/run-linux.sh run "$HOME/Games/Title" Title.exe   # records the pipelines it draws with
scripts/run-linux.sh warm --used --jobs 8                # makes them ahead of the next run
scripts/run-linux.sh run "$HOME/Games/Title" Title.exe
```

The first `warm --used` has nothing to make until a run has recorded
pipelines.

## Known gaps

- Missing face textures in the tested runs.
- A steady 30 frames per second has not been shown. The tested medians are
  22 to 23.
- Only the opening and the first room have been tested, on one AMD card.
- System Wine and the stock vkd3d-proton inside Proton were not part of the
  tested result.
