#!/usr/bin/env bash
# Runs a title under Wine on Linux, in a bubblewrap sandbox without network.
# See docs/linux.md for what each setting is for.
#
#   scripts/run-linux.sh <victory.exe> <title folder> <title exe name> [VICTORY_X=value ...]
#
# Settings (environment):
#   VICTORY_STATE     folder for the Wine prefix, caches and saves
#                     (default ~/.local/share/victory/linux)
#   PROTON_WINE_LIBS  .../Proton - Experimental/files/lib/wine (vkd3d-proton and DXVK come from it)
#   VKD3D_DLLS        folder with another d3d12.dll and d3d12core.dll pair (optional)
#   D3DCOMPILER       a native d3dcompiler_47.dll (winetricks keeps one in its cache)
#   RADV_DEBUG        default "zerovram"; set it empty to go without
#   VICTORY_BAKE_TEXTURES_MS  default 60 with VICTORY_WINE=proton, 1500 otherwise
#   VICTORY_WINE      "wine" (default: the system's) or "proton": Proton's own Wine
#                     (PROTON_WINE_LIBS/../../bin/wine) in a prefix of its own
#   VICTORY_WARM      "1": `victory warm` in the same sandbox before the launch;
#                     "only": the warm-up alone, no title is started. Off by default.
#   VICTORY_WARM_SECONDS  the warm-up takes no new work after this (default 600)
#   VICTORY_WARM_ARGS     arguments for `victory warm` (default "--pipelines")
set -euo pipefail

exe=$(realpath "$1")
title=$(realpath "$2")
title_exe=$3
shift 3

state=${VICTORY_STATE:-$HOME/.local/share/victory/linux}
proton=${PROTON_WINE_LIBS:?set PROTON_WINE_LIBS to Proton\'s files/lib/wine folder}
vkd3d=${VKD3D_DLLS:-$proton/vkd3d-proton/x86_64-windows}
compiler=${D3DCOMPILER:-$HOME/.cache/winetricks/d3dcompiler_47/d3dcompiler_47.dll}
warm=${VICTORY_WARM:-}
warm_seconds=${VICTORY_WARM_SECONDS:-600}
read -r -a warm_args <<<"${VICTORY_WARM_ARGS---pipelines}"
mkdir -p "$state/vkd3d-cache" "$state/mesa-cache"

case ${VICTORY_WINE:-wine} in
  wine)
    wine=wine
    wine_setup=(--unshare-ipc --setenv WINEPREFIX /state/prefix)
    display=()
    bake_ms=1500
    ;;
  proton)
    # Proton's Wine finds its libraries beside it. It needs the host's IPC
    # namespace (X11 shared-memory images) and a prefix of its own.
    files=$(realpath "$proton/../..")
    wine=$files/bin/wine
    [[ -x $wine ]] || { echo "no Proton Wine at $wine" >&2; exit 1; }
    wine_setup=(--ro-bind "$files" "$files" --setenv WINEPREFIX /state/prefix-proton
      --setenv LD_LIBRARY_PATH "$files/lib/x86_64-linux-gnu:$files/lib/i386-linux-gnu")
    # It draws through X11 (Xwayland).
    display=(--ro-bind /tmp/.X11-unix /tmp/.X11-unix --setenv DISPLAY "${DISPLAY:-:0}")
    if [[ -n ${XAUTHORITY:-} ]]; then
      display+=(--ro-bind "$XAUTHORITY" /run/session/Xauthority --setenv XAUTHORITY /run/session/Xauthority)
    fi
    # The shorter bake budget was measured only under Proton (docs/linux.md).
    bake_ms=60
    ;;
  *) echo "VICTORY_WINE is wine or proton" >&2; exit 1 ;;
esac

# Command-line settings come last, so they win over the environment.
settings=(--setenv RADV_DEBUG "${RADV_DEBUG-zerovram}"
  --setenv VICTORY_BAKE_TEXTURES_MS "${VICTORY_BAKE_TEXTURES_MS:-$bake_ms}")
for setting in "$@"; do
  settings+=(--setenv "${setting%%=*}" "${setting#*=}")
done

# What the warm-up and the run share: the same DLLs, caches and switches, so
# that the warm-up's translations and pipelines are the ones the run looks up.
sandbox=(bwrap --unshare-net --unshare-pid --new-session --clearenv
  --ro-bind /usr /usr --ro-bind /etc /etc --ro-bind /sys /sys
  --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib /lib64
  --proc /proc --dev /dev --dev-bind /dev/dri /dev/dri --tmpfs /tmp
  --dir "$HOME"
  --bind "$state" /state
  --ro-bind "$title" /title
  --ro-bind "$exe" /application/victory.exe
  --ro-bind "$vkd3d/d3d12.dll" /application/d3d12.dll
  --ro-bind "$vkd3d/d3d12core.dll" /application/d3d12core.dll
  --ro-bind "$proton/dxvk/x86_64-windows/dxgi.dll" /application/dxgi.dll
  --ro-bind "$compiler" /application/d3dcompiler_47.dll
  "${wine_setup[@]}"
  --setenv PATH /usr/bin --setenv LANG C.UTF-8 --setenv HOME "$HOME"
  --setenv WINEDEBUG -all,err+virtual,err+seh
  --setenv WINEDLLOVERRIDES 'd3d12,d3d12core,dxgi=n,b;d3dcompiler_47=n'
  --setenv VICTORY_DATA_DIR 'Z:\state\data'
  --setenv VICTORY_CACHE_DIR 'Z:\state\cache'
  --setenv MESA_SHADER_CACHE_DIR /state/mesa-cache
  --setenv VKD3D_SHADER_CACHE_PATH /state/vkd3d-cache
  "${settings[@]}"
  --chdir /state)

if [[ -n $warm ]]; then
  [[ $warm == 1 || $warm == only ]] || { echo "VICTORY_WARM is 1 or only" >&2; exit 1; }
  # Ended a minute past its own limit if a driver call never returns. The
  # Wine server is waited for: a prefix made by a short warm-up is only
  # written when it ends, and the sandbox's end would cut it off (a Proton
  # prefix left without its registry hangs every later start).
  status=0
  timeout --kill-after=10 $((warm_seconds + 60)) "${sandbox[@]}" --die-with-parent "${display[@]}" -- \
    /usr/bin/bash -c '"$1" /application/victory.exe warm "${@:3}"; status=$?; "$2" -w; exit $status' \
    warm "$wine" "${wine%wine}wineserver" --seconds "$warm_seconds" "${warm_args[@]}" || status=$?
  if [[ $warm == only ]]; then
    exit "$status"
  fi
  ((status == 0)) || echo "warm-up ended with status $status; launching anyway" >&2
  # The run keeps the containers it translates for the next warm-up.
  sandbox+=(--setenv VICTORY_SHADER_SOURCES 1)
fi

runtime=${XDG_RUNTIME_DIR:?}
exec "${sandbox[@]}" \
  --ro-bind "$runtime/${WAYLAND_DISPLAY:-wayland-0}" /run/session/wayland-0 \
  --ro-bind "$runtime/pulse/native" /run/session/pulse-native \
  --setenv XDG_RUNTIME_DIR /run/session --setenv WAYLAND_DISPLAY wayland-0 \
  --setenv PULSE_SERVER unix:/run/session/pulse-native \
  "${display[@]}" \
  -- "$wine" /application/victory.exe run "Z:\\title\\$title_exe" --execute
