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
  frame are dropped meanwhile.

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
