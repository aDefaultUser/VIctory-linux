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
set -euo pipefail

exe=$(realpath "$1")
title=$(realpath "$2")
title_exe=$3
shift 3

state=${VICTORY_STATE:-$HOME/.local/share/victory/linux}
proton=${PROTON_WINE_LIBS:?set PROTON_WINE_LIBS to Proton\'s files/lib/wine folder}
vkd3d=${VKD3D_DLLS:-$proton/vkd3d-proton/x86_64-windows}
compiler=${D3DCOMPILER:-$HOME/.cache/winetricks/d3dcompiler_47/d3dcompiler_47.dll}
runtime=${XDG_RUNTIME_DIR:?}
mkdir -p "$state/vkd3d-cache" "$state/mesa-cache"

settings=(--setenv RADV_DEBUG "${RADV_DEBUG-zerovram}")
for setting in "$@"; do
  settings+=(--setenv "${setting%%=*}" "${setting#*=}")
done

exec bwrap --unshare-net --unshare-pid --unshare-ipc --new-session --clearenv \
  --ro-bind /usr /usr --ro-bind /etc /etc --ro-bind /sys /sys \
  --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib /lib64 \
  --proc /proc --dev /dev --dev-bind /dev/dri /dev/dri --tmpfs /tmp \
  --dir "$HOME" \
  --bind "$state" /state \
  --ro-bind "$title" /title \
  --ro-bind "$exe" /application/victory.exe \
  --ro-bind "$vkd3d/d3d12.dll" /application/d3d12.dll \
  --ro-bind "$vkd3d/d3d12core.dll" /application/d3d12core.dll \
  --ro-bind "$proton/dxvk/x86_64-windows/dxgi.dll" /application/dxgi.dll \
  --ro-bind "$compiler" /application/d3dcompiler_47.dll \
  --ro-bind "$runtime/${WAYLAND_DISPLAY:-wayland-0}" /run/session/wayland-0 \
  --ro-bind "$runtime/pulse/native" /run/session/pulse-native \
  --setenv PATH /usr/bin --setenv LANG C.UTF-8 --setenv HOME "$HOME" \
  --setenv XDG_RUNTIME_DIR /run/session --setenv WAYLAND_DISPLAY wayland-0 \
  --setenv PULSE_SERVER unix:/run/session/pulse-native \
  --setenv WINEPREFIX /state/prefix \
  --setenv WINEDEBUG -all,err+virtual,err+seh \
  --setenv WINEDLLOVERRIDES 'd3d12,d3d12core,dxgi=n,b;d3dcompiler_47=n' \
  --setenv VICTORY_DATA_DIR 'Z:\state\data' \
  --setenv VICTORY_CACHE_DIR 'Z:\state\cache' \
  --setenv MESA_SHADER_CACHE_DIR /state/mesa-cache \
  --setenv VKD3D_SHADER_CACHE_PATH /state/vkd3d-cache \
  "${settings[@]}" \
  --chdir /state \
  -- wine /application/victory.exe run "Z:\\title\\$title_exe" --execute
