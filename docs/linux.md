# Running under Wine on Linux

Tested on one machine (2026-11-25): CachyOS, kernel 7.3.5, Radeon RX 9070
(RADV, Mesa 26.3.1), Wine 11.22, vkd3d-proton 2576c0b4 as shipped with Proton
Experimental `experimental-11.0-20261112`. `scripts/run-linux.sh` starts a
title with the settings below.

State on that machine: boot, menus and loading screens work; the opening
cutscene and the first room are drawn and lit, at 0.3 to 2 frames per second.
Every second or third frame of the 3D world is still black (see "Known
problems").

## What it needs

- **The executable built for Windows.** `cargo build --release -p victory
  --target x86_64-pc-windows-msvc`, cross-compiled with clang-cl and lld-link
  against an MSVC CRT and Windows SDK (xwin fetches both).
- **vkd3d-proton and DXVK's dxgi** as native DLLs beside the executable. The
  script takes them from a Proton install.
- **A native `d3dcompiler_47.dll`.** The layer compiles HLSL of its own at run
  time, and Wine's compiler is reported to refuse it (fork issue #6). Not
  checked here: every run used the native one.
- **`RADV_DEBUG=zerovram`.** Without it the 3D world is one flat colour (white,
  then black). The layer relies on new committed resources reading as zero,
  which Direct3D 12 promises. On this machine vkd3d-proton and RADV hand out
  video memory that still holds what was in it before, and the title's lighting
  reads textures before their first write. With the switch RADV zeroes every
  allocation. Where exactly the zeroing is lost (vkd3d-proton's
  zero-initialisation path, RADV on this card, or the kernel) is not known.

## Wine or Proton's Wine

System Wine 11.22 ran for more than ten minutes at a time. Proton
Experimental's own Wine (11.0, with fsync, through Xwayland) drew the same
picture, and the title crashed after three and a half minutes with a write to
address 0 in its own code, in the one run made. It needs the sandbox to share
the host's IPC namespace (X11 shared-memory images) and a prefix of its own.
`VICTORY_WINE=proton` has the script use it that way (`prefix-proton`, the
X11 socket of `DISPLAY`); only its warm-up has been run through the script so
far.

## Three compute pipelines vkd3d-proton cannot make

`CreateComputePipelineState` does not return for three of the title's compute
programs: dxil-spirv recurses until the worker's stack is gone
([dxil-spirv #376](https://github.com/HansKristian-Work/dxil-spirv/issues/376)).
The layer stops waiting for such a worker after 15 seconds
(`pipeline/make.rs`). The fix is upstream commit e084e35c on the branch
`legacy-struct-remove`. A vkd3d-proton built with that commit makes all three
pipelines in under 200 ms; pass its folder as `VKD3D_DLLS`. The picture does
not depend on it.

## Known problems

- **Black frames.** The scene's brightness before tone mapping swings by a
  factor of about 100 from frame to frame (large, small, zero), and the frame
  where it reaches zero is black. The title measures the picture's brightness
  on the card, reads the result back and sets the next frames' exposure from
  it. The numbers suggest it pairs a measurement with the wrong frame's
  exposure. Not found yet. `VICTORY_LATE_WAIT=0 VICTORY_RELAY=0` (everything
  waited for) was too slow here to tell: 6.5 s a frame.
- **A fresh shader cache takes minutes.** The first launch translates about
  3,200 shaders and vkd3d-proton compiles each pipeline; hundreds of draws a
  frame are dropped meanwhile. Every change to the translator (a new build,
  a switch it reads) keys the cache anew, so the next launch starts cold
  again. The warm-up below does that work before the launch.

## Warm-up before the launch

`VICTORY_WARM=1` runs `victory warm` in the same sandbox, with the same DLLs,
caches and `VICTORY_*` settings, before the title starts; `VICTORY_WARM=only`
runs it alone and starts no title. Off by default. It works the same with
`VICTORY_WINE=proton`; Proton's Wine gets the X11 display there too (without
one, its `explorer.exe` restarts over and over in a prefix made earlier and
Vulkan start-up waits for it), though the warm-up opens no window.

- **Translations (DXIL).** Each shader container is translated into the shader
  cache. The cache key holds the host facts the device hands the translator
  (widest wave, view heap size), so the warm-up makes the host device as a
  run does; its log shows the same `host wave lanes` and `host view heaps`
  lines as a run's.
- **Compute pipelines** (`--pipelines`, in the default `VICTORY_WARM_ARGS`).
  Made as a run's first dispatch makes them: into the pipeline library and
  vkd3d-proton's and Mesa's caches. Graphics pipelines need the title's
  graphics state and are not made here; a run starts the ones it drew with
  before at their creation (`early-pipelines.txt`), as it did already.
- **Where the containers come from.** A run started with `VICTORY_WARM` keeps
  every container it translates in the cache's `shader-sources` folder
  (`VICTORY_SHADER_SOURCES=1`). The title's own files hold few of them as
  plain containers. `victory warm <folder>` also takes `VICTORY_SHADER_SAVE`
  folders; `--only <file>` limits it to the hashes a file names (a run's log
  will do). With no sources it says so and the launch goes on.
- **Bounds.** `VICTORY_WARM_SECONDS` (default 600) stops new work; the
  sandbox is ended a minute later if a driver call never returns. A pipeline
  not back within `--pipeline-seconds` (default 30) is left to its worker,
  which takes no more work; after four are left behind no more pipelines are
  started. Each pipeline is noted before and after it is asked for, by its
  name in the pipeline library (a hash of the translated code and root
  signature), in `cache/pipelines/warm-pipelines-<driver>.txt` (a hash of the
  loaded `d3d12core.dll` and of `VKD3D_CONFIG`, `VKD3D_DISABLE_EXTENSIONS`,
  `VKD3D_FEATURE_LEVEL`, `VKD3D_SHADER_MODEL`, `RADV_DEBUG`, `RADV_PERFTEST`
  and `ACO_DEBUG` when set). The next warm-up with that driver and those
  settings skips the ones that never came back or were refused, until a
  change to the translator, its switches or the host changes what the driver
  is handed. A warm-up that fails does not stop the
  launch. The Wine server is waited for at the end, so that a prefix the
  warm-up made is written out before the sandbox ends.

Measured on the machine above (warm-up only, no title started, 4 translation
threads, 2 pipeline threads):

| case | result |
|---|---|
| cold shader cache, 3,235 containers | 2,839 + 395 translated in 140 s + 20 s (a 20 s limit, then a second warm-up that went on from there) |
| shader cache of an earlier run, same translator | 3,233 of 3,235 found in the cache (the key matched the run's), 3 s |
| compute pipelines, empty driver caches, stock vkd3d-proton 2576c0b4 | 1,133 made in 111 s; the three dxil-spirv #376 programs did not come back within 20 s and were left behind |
| the same again | 1,133 loaded from the library in 1 to 2 s; the three skipped |
| `VKD3D_DLLS` with the dxil-spirv e084e35c backport | 1,136 made in 6.5 s (driver caches of the runs before), the three included |
| Proton Experimental's Wine, new and reused prefix | as with system Wine: same host facts, same driver, 3 to 13 s |

What it does not change: textures, descriptors and streaming. Texture bytes
come from the title at run time and are uploaded as it asks for them, so a
warm cache does not fix missing or late textures in the world or on
characters. Switching `VKD3D_DLLS` between a warm-up and a run makes the run
reject the warm-up's pipeline library (each vkd3d-proton build refuses
another's), which the script avoids by using the same DLLs for both.

## Finding out what a frame does

A build with the `diagnostics` feature writes presented frames and traces
(`crates/victory-gpu/src/capture.rs`):

- `VICTORY_CAPTURE=<folder>` saves frames; `VICTORY_CAPTURE_WORLD=1` with
  `VICTORY_CAPTURE_WORLD_ALSO=100,101` adds frames counted from the world's
  first one, each with a file of its operations in recording order, its clears
  and the order its lists reach the card in.
- `VICTORY_DUMP_TARGETS=1` saves every texture those frames write, and logs
  each one's value range and its texels that are not numbers.

These files are derived from the title. They stay outside the repository
(`docs/asset-policy.md`).
