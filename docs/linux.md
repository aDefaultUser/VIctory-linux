# Running under Wine on Linux

Tested on one machine (2026-11-25): CachyOS, kernel 7.3.5, Radeon RX 9070 XT
(RADV, Mesa 26.3.1), system Wine 11.22, and Proton Experimental
`experimental-11.0-20261112` (Wine 11.0, vkd3d-proton 2576c0b4).
`scripts/run-linux.sh` starts a title with the settings below.

## Latest tested runs

The renderer at commit `7ec6682` (`e9c3bf8` after it only adds a test) reached
the movement and camera prompt in the first room of the opening, with the
player's hair and clothes, the walls and the wired glass textured:

| Wine | time running | result |
|---|---|---|
| Proton Experimental 11.0 | 520 s | first room, camera input worked, no GPU fault |
| System Wine 11.22 (initialized private prefix) | 420 s | first room, no GPU fault |

Both runs used a direct launcher and a private Xwayland server with these
settings.

- a vkd3d-proton build with the matching dxil-spirv backport (see "Three
  compute pipelines vkd3d-proton cannot make")
- `RADV_DEBUG=zerovram`
- `VICTORY_TEXTURE_NO_WAIT=1 VICTORY_RELAY=0 VICTORY_LATE_WAIT=0`
- `VICTORY_HEAP_SYNC_TABLES` unset, so only table slots whose host slot does
  not show their record are mirrored (the default)

These runs cover the opening. Later scenes, native Windows behavior and
performance with default settings remain unverified.

The command below applies those settings through the script. A separate
140-second smoke test with the Proton script warmed the cache and entered the
prologue without a GPU fault. Set `VKD3D_DLLS` to the folder holding your
patched build's `d3d12.dll` and `d3d12core.dll`.

```sh
VICTORY_WARM=1 VICTORY_WINE=proton \
VKD3D_DLLS="$HOME/vkd3d-proton-e084e35c" \
PROTON_WINE_LIBS="$HOME/.local/share/Steam/steamapps/common/Proton - Experimental/files/lib/wine" \
  scripts/run-linux.sh target/x86_64-pc-windows-msvc/release/victory.exe \
  "$HOME/Games/Title" "Title.exe" \
  VICTORY_TEXTURE_NO_WAIT=1 VICTORY_RELAY=0 VICTORY_LATE_WAIT=0
```

Drop `VICTORY_WINE=proton` for system Wine.

What was seen on screen, besides the first room:

- **Logo movie.** Near 30 frames per second in sampled intervals with frame
  capture turned off, under both system Wine and Proton (earlier builds).
- **Loading screen.** Texture preparation for the character bake still
  causes stutter, less under Proton with the script's shorter bake budget
  (next section).
- **Bright frames in the intro.** Some of them are an authored title card. A
  short bright fade of the window has not been shown to be a bug.

## Texture uploads before the character bake

While it loads, the title bakes its characters' skin and decorations, and
just before each bake draw the layer uploads textures the bake may pick.
`VICTORY_BAKE_TEXTURES_MS` is how many milliseconds of upload work that pass
may take there. The script's default is 60 with `VICTORY_WINE=proton` and
1500 under system Wine, which is also the layer's own default without the
script. A value in the environment replaces the script's default, and a
`VICTORY_BAKE_TEXTURES_MS=...` argument after the executable name wins over
both. `0` turns the pass off.

In one matched Proton Experimental A/B with frame capture off, 60 against
1500 raised the slowest five-second sample of the loading screen from 4.2 to
14.9 frames per second and the median from 20 to 23, and the first room
looked the same (the player's hair and clothes, the walls and the wired
glass) with camera input working. The setting limits the upload pass. In the
separate Proton script smoke test, one loading interval still dipped to
1.6 frames per second.

System Wine keeps 1500 because 60 is unproven there. One run with 60 reached
the first room with camera control, but its loading screen still dropped to
2.5 frames per second, and dark patches on clothing were more visible than in
a run with 1500. There is no matched frame-rate comparison under system Wine,
and the patches have not been traced to the setting.

## What it needs

- **The executable built for Windows**, cross-built as described in the next
  section.
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

## Building the Windows executable

A plain `cargo build --target x86_64-pc-windows-msvc` on Linux works only when
a C compiler, librarian and linker for that target and an MSVC CRT and Windows
SDK are already set up. [cargo-xwin](https://github.com/rust-cross/cargo-xwin)
does that setup:

```sh
# clang (clang-cl), lld (lld-link) and llvm (llvm-lib) from the distribution,
# for example on Arch: sudo pacman -S clang lld llvm
cargo install --locked cargo-xwin
cargo xwin build --release -p victory --target x86_64-pc-windows-msvc
```

The first build downloads the CRT and SDK with
[xwin](https://github.com/Jake-Shadle/xwin) into cargo-xwin's cache
(`XWIN_CACHE_DIR` moves it), which means accepting
[Microsoft's license](https://go.microsoft.com/fwlink/?LinkId=2086102). The
Windows target comes with the toolchain that `rust-toolchain.toml` pins. The
executable lands in `target/x86_64-pc-windows-msvc/release/victory.exe`.

The builds on the machine above were made by hand from the same pieces: a
CRT and SDK fetched with xwin, `CC_x86_64_pc_windows_msvc=clang-cl`,
`AR_x86_64_pc_windows_msvc=llvm-lib`, and `CFLAGS_x86_64_pc_windows_msvc`
giving clang-cl `/vctoolsdir` and `/winsdkdir` for the CRT and SDK folders.
The cargo-xwin route has not been run on that machine yet.

## Wine or Proton's Wine

Proton Experimental's own Wine (11.0, with fsync, through Xwayland) needs the
sandbox to share the host's IPC namespace (X11 shared-memory images) and a
prefix of its own. `VICTORY_WINE=proton` has the script use it that way
(`prefix-proton`, the X11 socket of `DISPLAY`).

Earlier runs on the same machine on November 25, 2026.

- System Wine 11.22 entered the opening story (the prologue).
- Proton Experimental, started outside the script with its own
  `proton runinprefix`, entered the prologue too.
- `VICTORY_WINE=proton VICTORY_WARM=1` through the script, with `DISPLAY` set
  to a private Xwayland server, ran the warm-up and then the title; the intro
  movie played at 30 frames per second in the three intervals measured.

These runs still showed missing or wrong rendering, and some of them reset
the GPU. The script's Proton run with the warm-up is the only one of these
started by `scripts/run-linux.sh`. An earlier run with Proton's Wine crashed
after three and a half minutes with a write to address 0 in the title's own
code; that crash has not been looked into since.

## Three compute pipelines vkd3d-proton cannot make

`CreateComputePipelineState` does not return for three of the title's compute
programs: dxil-spirv recurses until the worker's stack is gone
([dxil-spirv #376](https://github.com/HansKristian-Work/dxil-spirv/issues/376)).
The layer stops waiting for such a worker after 15 seconds
(`pipeline/make.rs`). The tested DLLs use vkd3d-proton `2576c0b4` and
dxil-spirv `114e664e` with a backport of upstream commit `e084e35c` from
`legacy-struct-remove`. The patched build made all three pipelines in under
200 ms in offline tests.

## Known problems

- **Missing or wrong rendering.** Earlier builds drew parts of scenes and
  characters wrong or not at all. With `7ec6682` the first room renders as
  described above. Later parts of the game remain unverified.
- **GPU resets (earlier builds).** Some earlier runs reset the GPU, for a cause
  not known. The two `7ec6682` runs showed no fault in 520 and 420 seconds.
- **Loading-screen stutter.** Stalls can still occur with the shorter bake
  budget. See [Texture uploads before the character bake](#texture-uploads-before-the-character-bake).
- **Black frames (earlier builds).** Not seen in later samples. Earlier runs
  showed every second or third frame of the 3D world black. In those runs the
  scene's brightness before tone mapping swung by a factor of about 100 from
  frame to frame (large, small, zero), and the black frames were the ones
  where it reached zero. The title measures the picture's brightness on the
  card, reads it back and sets later frames' exposure from it, so a
  measurement paired with the wrong frame is one possible cause. It has not
  been confirmed. On those builds `VICTORY_LATE_WAIT=0 VICTORY_RELAY=0`
  (everything waited for) was too slow to tell: 6.5 s a frame.
- **A fresh shader cache takes minutes.** The first launch translates about
  3,260 shaders and vkd3d-proton compiles each pipeline; hundreds of draws a
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

The patched vkd3d-proton build and the shader fixes in `7e789a1` translated
all 3,264 saved containers in 50.2 s on a cold Proton cache with four
translation workers. Two workers compiled 1,137 compute pipelines in
76.1 s. System Wine reused those shader translations in 0.8 s and the
compute pipelines in 0.9 s. The test card supports both 32- and 64-lane
waves. An offline run forced to the 32-lane host setting still refused
one different shader.

A cold-cache comparison of 512 loose shader files under Proton took 34.1 s
with `--jobs 1` and 8.4 s with `--jobs 4`. Both runs produced byte-identical
cached translations.

Earlier warm-up measurements configured four translation workers and two
pipeline workers. That version processed loose shader files one at a time.

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
