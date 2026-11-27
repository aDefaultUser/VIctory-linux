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

## Updating from the previous launcher

Put the path to the Windows executable in `RUN_LINUX_EXE`, then use
`scripts/run-linux.sh run <title folder> <title exe>`. Point
`RUN_LINUX_STATE` at your existing state directory to reuse its prefix
and data folders, or set each path under
[Where things are kept](#where-things-are-kept). The cache has its own
`RUN_LINUX_CACHE` path. The launcher now defaults to Proton's Wine.
Use `RUN_LINUX_WINE=wine` to keep running with system Wine.

Run `shaders` and `warm --used` as separate commands for the old
launch-time warm-up. Put settings such as `VICTORY_BAKE_TEXTURES_MS=1500`
after the title executable name. After updating, the renderer translates
shaders under a new cache key.

With `VICTORY_STREAM_NOTICE=0`, the default `VICTORY_PAGE_WRITES` setting
also queues early-uploaded textures again when a plain storage read
finishes. `VICTORY_HEAP_RECHECK_EVERY` enables further checks for late
texture changes and is off by default.

## Tested result

One machine, November 2026: CachyOS, Radeon RX 9070 XT, Ryzen 9 9950X3D,
RADV (Mesa 26.3.1), Proton Experimental `experimental-11.0-20261112`, a
private `wineserver` build in place of Proton's (`RUN_LINUX_WINESERVER`),
`/dev/ntsync`, an app-local, patched vkd3d-proton build matched to that
Proton, and a native `d3dcompiler_47.dll`. Logs, images and saves are not
published. The vkd3d-proton build is upstream 2576c0b4 (Meson version 3.2.0,
build id `v3.1-56-g2576c0b4+`) with dxil-spirv at 114e664e. For the
renderer's shaders it adds a local backport of e084e35c, whose two fixups
touch dxil-spirv's legacy structurizer. The tested wineserver indexes views
by address using the approach in
[Wine's memory-view change](https://github.com/wine-mirror/wine/commit/3217449b),
backported to the Wine revision in that Proton build.

### Series S at 1920x1080

A plain release build of d9dfc31 (see "What it needs"), as a Series S, with
three settings after the script's defaults:

```sh
RUN_LINUX_EXE=target/x86_64-pc-windows-msvc/release/victory.exe \
RUN_LINUX_CONSOLE=series-s \
  scripts/run-linux.sh run "$HOME/Games/Title" Title.exe \
  VICTORY_CARD_BUFFERS_MIN_MB=1 VICTORY_UNDER_FUSED=0 VICTORY_PLACEHOLDER_RANGES=1
```

- **First room:** a median of 29.9 FPS across six readings after camera
  input, and 30.0 FPS across twelve readings after stepping and turning.
  Readings were logged about five seconds apart.
- **Faces rendered** in that camera check. Other camera views in the
  first room measured about 24 to 26 FPS. In some replays of the same
  checkpoint, faces were missing in both the opening and first room.
- The card's buffer budget was left to its automatic size, which was 2 GiB
  on this card. `VICTORY_CARD_BUFFERS_MB` was not set.

In an A/B with one diagnostics build, `VICTORY_PLACEHOLDER_RANGES=1` raised
the median from 22.5 to 28.45 frames per second and cut the per-frame
refresh time the diagnostics report from 25.6 to 9.6 ms.

The three settings are optional.

- `VICTORY_PLACEHOLDER_RANGES=1` has the runtime tell the guards which
  memory is a placeholder, since Wine will not answer that question. It is
  off by default.
- `VICTORY_UNDER_FUSED=0` was used in runs that restored faces. The two
  buffer update paths produce identical outputs in unit tests, and faces
  can still be missing with this setting.
- `VICTORY_CARD_BUFFERS_MIN_MB=1` makes eligible buffers of at least 1 MiB
  candidates for GPU memory, within the card's buffer budget. The automatic
  minimum with a 2 GiB budget is 4 MiB.

#### Brown haze in a day room

`VICTORY_FOG_INPUT=radiance` is an opt-in workaround, measured in one scene
on one machine.

The title's fog composite reads a quarter-resolution radiance texture.
This setting makes that one input read as zeros, only in the composite's
own freshly made descriptor table. It applies only when the program's
resources and views match the recorded layout, and otherwise logs why once
and changes nothing.

As a Series S with a 1280x720 internal picture shown at 1920x1080, on the
RX 9070 XT above, the setting cleared a dense haze in a day room with a
fixed camera and the normal scattering left on. The haze stayed when only
the scattered light was zeroed. The setting removes one full
quarter-resolution radiance contribution, so any correct light from that
input is lost as well. Why that input is too bright is still under
investigation.

#### Large stray triangles

A heavier shot than the first-room readings above drew large stray
triangles in some frames. The A/B used one executable and one fixed
camera at 6,915 draws per frame, with the script's `VICTORY_CARD_STEPS=2`
and two added settings.
`VICTORY_CARD_STEPS_GUARD=1` makes a submission wait as before when one of
its copies needs flights landed first, and `VICTORY_CARD_STEPS_SRV=1` keeps
the buffer between readable by shaders. The table adds at most one wait to
those.

| Wait | Frames with large triangles | Median frame time per phase |
|---|---|---|
| neither | 9 of 666 | 38.9–42.3 ms |
| `VICTORY_CARD_STEPS_WAIT=1` | 0 of 340 | 48.6 ms |
| `VICTORY_UNDER_WRITES_WAIT=1` | 0 of 733 | 57.8–61.8 ms |

No captured frame showed the triangles with either wait, and the card-steps
wait cost less. `VICTORY_CARD_STEPS=0` was also measured, with no
triangles in 520 frames at about 20 to 21 FPS. A run with the guard, the
shader-readable buffer and the shorter wait looks like this.

```sh
RUN_LINUX_EXE=target/x86_64-pc-windows-msvc/release/victory.exe \
RUN_LINUX_CONSOLE=series-s \
  scripts/run-linux.sh run "$HOME/Games/Title" Title.exe \
  VICTORY_CARD_STEPS_GUARD=1 VICTORY_CARD_STEPS_SRV=1 VICTORY_CARD_STEPS_WAIT=1
```

All three settings are opt-in and were measured in this one shot on this
one machine.

#### A scoped quality profile

The fog workaround and the three card-steps settings were tested together
as one opt-in profile, in the environment above on the one Radeon PC. A
plain Windows release build of 6f85e21 ran as a Series S with a 1280x720
internal picture at 1920x1080, with the ambient light and the scattering at
their defaults. The four settings follow the three 1080p performance
settings above.

```sh
RUN_LINUX_EXE=target/x86_64-pc-windows-msvc/release/victory.exe \
RUN_LINUX_CONSOLE=series-s \
  scripts/run-linux.sh run "$HOME/Games/Title" Title.exe \
  VICTORY_CARD_BUFFERS_MIN_MB=1 VICTORY_UNDER_FUSED=0 VICTORY_PLACEHOLDER_RANGES=1 \
  VICTORY_FOG_INPUT=radiance VICTORY_CARD_STEPS_GUARD=1 VICTORY_CARD_STEPS_SRV=1 \
  VICTORY_CARD_STEPS_WAIT=1
```

With a fixed camera, the day room stayed clear of the brown haze and none
of 1,040 captured frames over 52 seconds showed large triangles, at 7,392
draws per frame. The median was 19.8 FPS across 11 readings about five
seconds apart. In the first room the median was 23.5 FPS across six
readings after moving and turning.

The 29.9 and 30.0 FPS first-room medians above came from earlier, lighter
tests at the checkpoint. Holding 30 FPS in the heavier shot is still an
open goal.

Faces were missing in this replay, so it shows nothing of how they move.
Mouths were seen moving again in earlier replays that had face meshes
visible and the d16 fix in 1430f61, which reads a view's kind from its
table. Jaws in those replays can still look distorted, and some face meshes
still go missing at times. In a diagnostics build in the same session,
some face meshes were already missing with the fog workaround off and
stayed missing after it was turned on and off again. The workaround
removes the whole quarter-resolution radiance term, and where each of these
symptoms starts is still being narrowed down.

### Earlier result, Series X

The script's defaults, as a Series X: medians of 23.1 frames per second over
14 readings in the opening and 22.3 over six readings in the first
room, starting from a private checkpoint in a save the tester made. The
camera responded to input, and **face textures were missing**.

### Validation of the main update

Proton Experimental replays on Xwayland exercised keyboard movement, mouse
look and buttons, click-to-capture and Esc release. A menu replay of
`2892a9e` also exercised activation-loss release with a test window manager.
On the same Radeon RX 9070 XT, `e145f70` passed seven synthetic device
checks for heap descriptors and root constant buffers, plus a CPU check for
the d16 kind decision.

The scene and frame-rate results above belong to their stated builds. A
full gameplay replay of the combined update is pending. Jaw alignment,
intermittent missing face meshes, asset level of detail and shimmer remain
open.

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
  `cargo xwin build --locked --release -p victory --target x86_64-pc-windows-msvc`
  makes the plain release build in
  `target/x86_64-pc-windows-msvc/release/victory.exe`.
- **Proton** (`PROTON_WINE_LIBS`, its `files/lib/wine` folder). Proton's Wine
  runs the commands, and DXVK's `dxgi.dll` comes from it. With
  `RUN_LINUX_WINE=wine` the system's Wine runs them instead, and Proton still
  provides the DLLs.
- **vkd3d-proton** (`VKD3D_DLLS`, a folder with `d3d12.dll` and
  `d3d12core.dll`). The default is the copy inside Proton. The tested runs
  used a separate, patched build matched to the Proton above (see "Tested
  result").
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
changes that (see "Tested result").

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
`VICTORY_CACHE_DIR=Z:\state\cache\victory`, where it cannot tell whether
they are in a checkout. So before anything is written, the script resolves
the three folders on the host (through symbolic links too). It stops if one
is in a VIctory checkout, which it finds by a `.victory-root` in that folder
or a folder above it. It also stops if one holds or lies in the checkout the
script runs from, the title's folder, a shader input, `victory.exe`, a DLL or
Proton. A writable folder can contain a separate checkout.

`shaders --list FILE` writes FILE on the host, only when the command
succeeds: the list is made in the cache and then replaces FILE. Until then
an existing FILE is left as it is. The script refuses a FILE that is a
symbolic link or not a plain file, is in a checkout, or is or lies in one of
the shader inputs.

`scripts/check-run-linux.sh` checks these rules with a stand-in `bwrap`,
without Wine or a title.

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

The [mouse controls](README-fxceless.md#input) work through Wine. While
captured, VIctory clips the cursor to the title's window. Wine grabs the
pointer and moves the cursor by the device's relative motion. This also
works on Xwayland, where a program cannot move the host pointer directly.

When an X11 window outside Wine takes the keyboard focus while the mouse is
captured, Wine (Proton Experimental, Wine 11) may still report the title's
window as the foreground window and keep VIctory's clip. With a window
manager running, Wine still sends the title's window `WM_CANCELMODE` on the
X11 focus loss, and VIctory ends the capture on it. This was seen with small
test window managers, among them one that advertises `_NET_ACTIVE_WINDOW`
(as KWin and Mutter do) but does not follow such a switch, where nothing
else changed; KWin and Mutter themselves were not tried. On an X server
without a window manager, as in a bare private Xwayland, Wine sends nothing:
the X11 focus there follows the pointer, Wine ignores those focus events, and
the capture holds until Esc reaches the title's window again. The
settings (`VICTORY_MOUSE=0`, `VICTORY_MOUSE_SPEED=2`, ...) go on the command
line as `NAME=value` arguments, like the others; Proton started without the
script reads them from its environment.

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

- Only the opening and the first room of one title have been tested, on one
  machine.
- A Series S at 1280x720 crashed in the same test.
- As a Series X, face textures were missing in the earlier runs.
- Cold runs can behave differently from the measured ones.
- System Wine and the stock vkd3d-proton inside Proton were not part of the
  tested result.
