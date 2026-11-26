#!/usr/bin/env bash
# Runs VIctory's Windows build under Wine on Linux, in a bubblewrap sandbox
# without network. See docs/linux.md.
#
#   scripts/run-linux.sh run <title folder> <title exe> [NAME=value ...] [-- title arguments]
#   scripts/run-linux.sh shaders <archive or folder>... [--jobs N] [--list FILE] [NAME=value ...]
#   scripts/run-linux.sh warm [--used] [--jobs N] [--limit N] [--again] [NAME=value ...]
#
# NAME=value arguments set the sandbox's environment over the defaults below
# (later ones win; an empty value unsets the name). All three commands get the
# same settings, DLLs and caches, so the shader cache key, the pipeline book
# and the driver caches the warm-ups fill are the ones a run looks up.
#
# Paths and tools (environment):
#   RUN_LINUX_EXE        victory.exe (default target/x86_64-pc-windows-msvc/diag/victory.exe)
#   RUN_LINUX_STATE      default ${XDG_DATA_HOME:-~/.local/share}/victory/linux
#   RUN_LINUX_PREFIX     Wine prefix (default $RUN_LINUX_STATE/prefix-proton, or /prefix for system Wine)
#   RUN_LINUX_DATA       VIctory's data folder, saves included (default $RUN_LINUX_STATE/data)
#   RUN_LINUX_CACHE      VIctory's, vkd3d-proton's and Mesa's caches
#                        (default ${XDG_CACHE_HOME:-~/.cache}/victory/linux)
#   RUN_LINUX_WINE       "proton" (default): Proton's Wine; "wine": the system's
#   RUN_LINUX_WINESERVER a wineserver built for Proton's Wine, used in its place in the sandbox
#   RUN_LINUX_NTSYNC     "1" (default): the host's /dev/ntsync is in the sandbox; "0": left out
#   RUN_LINUX_CONSOLE    the console model `run` asks for (default series-x)
#   PROTON_WINE_LIBS     .../Proton - Experimental/files/lib/wine (DXVK's dxgi, and vkd3d-proton
#                        unless VKD3D_DLLS is set)
#   VKD3D_DLLS           folder with the d3d12.dll and d3d12core.dll pair to use
#   D3DCOMPILER          a native d3dcompiler_47.dll (default: winetricks' cache)
#   RADV_DEBUG           default "zerovram"; set it empty to go without
set -euo pipefail

repo=$(realpath "$(dirname "${BASH_SOURCE[0]}")/..")
die() { echo "run-linux.sh: $*" >&2; exit 1; }
usage() { sed -n '5,7s/^#   //p' "${BASH_SOURCE[0]}" >&2; exit 2; }

(($# > 0)) || usage
command=$1
shift

# scripts/play.ps1's settings without -Safe, and the renderer ports of the
# tested Linux runs (docs/linux.md). VICTORY_PORT_CONSTANTS stays off: with
# it on, Proton showed a black picture.
declare -A settings=()
order=()
set_setting() {
  [[ -v settings[$1] ]] || order+=("$1")
  settings[$1]=$2
}
for setting in \
  VICTORY_TEXTURE_NO_WAIT=1 VICTORY_RELAY=2 VICTORY_WIDE_BACK=1 VICTORY_CARD_REHASH_RUNS=1 \
  VICTORY_JOIN_COPIES=1 VICTORY_INDIRECT_WINDOW=1 VICTORY_RELAY_COPY=1 VICTORY_CARD_STEPS=2 \
  VICTORY_INDIRECT_BATCH=2 VICTORY_MAP_THREADS=1 VICTORY_TITLE_PRIORITIES=0 VICTORY_CARD_AWAKE=1 \
  VICTORY_SHOW_AFTER=1 VICTORY_AMBIENT=0.8 VICTORY_D3D12_DRED=light \
  VICTORY_THIN=1 VICTORY_JOIN_PARTS=8 VICTORY_UPLOAD_AHEAD=1 \
  VICTORY_PORT_CONSTANTS=0 VICTORY_PORT_DESCRIPTORS=1 VICTORY_PORT_CBV=1 VICTORY_PORT_GUARD=1 \
  VICTORY_PORT_JOURNAL=1 VICTORY_PIPELINE_BOOK=1 VICTORY_OUTPUT=1920x1080 \
  "RADV_DEBUG=${RADV_DEBUG-zerovram}"; do
  set_setting "${setting%%=*}" "${setting#*=}"
done

# Only the arguments reach the sandbox: it starts from an empty environment.
inherited=$(compgen -e | grep '^VICTORY_' || true)
[[ -z $inherited ]] || echo "run-linux.sh: not passed on (give them as NAME=value arguments):" $inherited >&2

# What the sandbox binds beyond the common set, and the command's arguments.
binds=()
args=()
is_setting() { [[ $1 =~ ^[A-Za-z_][A-Za-z0-9_]*= ]]; }
case $command in
  run)
    (($# >= 2)) || usage
    title=$(realpath "$1")
    title_exe=$2
    shift 2
    [[ -f $title/$title_exe ]] || die "no $title_exe in $title"
    binds+=(--ro-bind "$title" /title)
    args=(run --console "${RUN_LINUX_CONSOLE:-series-x}" --execute "Z:\\title\\${title_exe//\//\\}")
    while (($#)); do
      case $1 in
        --) args+=("$@"); break ;;
        *) is_setting "$1" || die "run: $1 is not NAME=value (title arguments go after --)" ;;
      esac
      set_setting "${1%%=*}" "${1#*=}"
      shift
    done
    ;;
  shaders)
    args=(shaders)
    inputs=0
    while (($#)); do
      if is_setting "$1"; then
        set_setting "${1%%=*}" "${1#*=}"
      elif [[ $1 == --list ]]; then
        (($# >= 2)) || die "shaders: --list needs a file"
        # Written by the sandbox: the file is made here and bound in.
        : >"$2"
        binds+=(--bind "$(realpath "$2")" /output/list.txt)
        args+=(--list 'Z:\output\list.txt')
        shift
      elif [[ $1 == --jobs ]]; then
        (($# >= 2)) || die "shaders: --jobs needs a number"
        args+=("$1" "$2")
        shift
      elif [[ $1 == -* ]]; then
        args+=("$1")
      else
        [[ -e $1 ]] || die "shaders: no $1"
        name=$(basename "$(realpath "$1")")
        binds+=(--ro-bind "$(realpath "$1")" "/input/$inputs/$name")
        args+=("Z:\\input\\$inputs\\$name")
        inputs=$((inputs + 1))
      fi
      shift
    done
    ((inputs > 0)) || die "shaders: name the title's shader archives or folders"
    ;;
  warm)
    args=(warm)
    for arg in "$@"; do
      if is_setting "$arg"; then
        set_setting "${arg%%=*}" "${arg#*=}"
      else
        args+=("$arg")
      fi
    done
    ;;
  *) usage ;;
esac

exe=$(realpath "${RUN_LINUX_EXE:-$repo/target/x86_64-pc-windows-msvc/diag/victory.exe}")
proton=${PROTON_WINE_LIBS:?set PROTON_WINE_LIBS to Proton\'s files/lib/wine folder}
vkd3d=${VKD3D_DLLS:-$proton/vkd3d-proton/x86_64-windows}
dxgi=$proton/dxvk/x86_64-windows/dxgi.dll
compiler=${D3DCOMPILER:-$HOME/.cache/winetricks/d3dcompiler_47/d3dcompiler_47.dll}
for file in "$exe" "$vkd3d/d3d12.dll" "$vkd3d/d3d12core.dll" "$dxgi" "$compiler"; do
  [[ -f $file ]] || die "no $file"
done
wine_kind=${RUN_LINUX_WINE:-proton}
state=${RUN_LINUX_STATE:-${XDG_DATA_HOME:-$HOME/.local/share}/victory/linux}
data=${RUN_LINUX_DATA:-$state/data}
cache=${RUN_LINUX_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/victory/linux}
wineserver=${RUN_LINUX_WINESERVER:-}

case $wine_kind in
  wine)
    [[ -z $wineserver ]] || die "RUN_LINUX_WINESERVER needs RUN_LINUX_WINE=proton"
    prefix=${RUN_LINUX_PREFIX:-$state/prefix}
    wine=wine
    wine_setup=(--unshare-ipc)
    display=()
    ;;
  proton)
    # Proton's Wine finds its libraries beside it. It needs the host's IPC
    # namespace (X11 shared-memory images) and a prefix of its own.
    prefix=${RUN_LINUX_PREFIX:-$state/prefix-proton}
    files=$(realpath "$proton/../..")
    wine=$files/bin/wine
    [[ -x $wine ]] || die "no Proton Wine at $wine"
    wine_setup=(--ro-bind "$files" "$files"
      --setenv LD_LIBRARY_PATH "$files/lib/x86_64-linux-gnu:$files/lib/i386-linux-gnu")
    if [[ -n $wineserver ]]; then
      # Wine starts the wineserver beside its own binary before it looks at
      # WINESERVER, so the replacement is mounted over Proton's, in the
      # sandbox only. The final `wineserver -w` waits for it too.
      [[ -f $wineserver && -x $wineserver ]] || die "RUN_LINUX_WINESERVER: no executable file at $wineserver"
      wine_setup+=(--ro-bind "$(realpath "$wineserver")" "$files/bin/wineserver")
    fi
    # It draws through X11 (Xwayland). The warm-ups need the display too: without
    # one, explorer.exe restarts over and over and Vulkan start-up waits for it.
    display=(--ro-bind /tmp/.X11-unix /tmp/.X11-unix --setenv DISPLAY "${DISPLAY:-:0}")
    if [[ -n ${XAUTHORITY:-} ]]; then
      display+=(--ro-bind "$XAUTHORITY" /run/session/Xauthority --setenv XAUTHORITY /run/session/Xauthority)
    fi
    ;;
  *) die "RUN_LINUX_WINE is proton or wine" ;;
esac

# Wine keeps events, semaphores and mutexes in the kernel through /dev/ntsync
# when it can open it. `--dev /dev` below leaves the device out, and then every
# wait and signal is a request to the one Wine server process.
ntsync=()
case ${RUN_LINUX_NTSYNC:-1} in
  1)
    ntsync=(--dev-bind-try /dev/ntsync /dev/ntsync)
    [[ -e /dev/ntsync ]] || echo "run-linux.sh: no /dev/ntsync: Wine synchronizes through its server (modprobe ntsync)" >&2
    ;;
  0) ;;
  *) die "RUN_LINUX_NTSYNC is 1 or 0" ;;
esac

if [[ $command == run ]]; then
  runtime=${XDG_RUNTIME_DIR:?}
  display+=(--ro-bind-try "$runtime/${WAYLAND_DISPLAY:-wayland-0}" /run/session/wayland-0
    --ro-bind-try "$runtime/pulse/native" /run/session/pulse-native
    --setenv XDG_RUNTIME_DIR /run/session --setenv WAYLAND_DISPLAY wayland-0
    --setenv PULSE_SERVER unix:/run/session/pulse-native)
fi

environment=()
for name in "${order[@]}"; do
  if [[ -n ${settings[$name]} ]]; then
    environment+=(--setenv "$name" "${settings[$name]}")
  else
    environment+=(--unsetenv "$name")
  fi
done

mkdir -p "$prefix" "$data" "$cache/victory" "$cache/vkd3d" "$cache/mesa"
# The Wine server is waited for at the end: a prefix made by this start is
# only written out when it ends, and the sandbox's end would cut it off.
exec bwrap --unshare-net --unshare-pid --new-session --die-with-parent --clearenv \
  --ro-bind /usr /usr --ro-bind /etc /etc --ro-bind /sys /sys \
  --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib /lib64 \
  --proc /proc --dev /dev --dev-bind /dev/dri /dev/dri "${ntsync[@]}" --tmpfs /tmp \
  --dir "$HOME" \
  --bind "$prefix" /state/prefix \
  --bind "$data" /state/data \
  --bind "$cache" /state/cache \
  --ro-bind "$exe" /application/victory.exe \
  --ro-bind "$vkd3d/d3d12.dll" /application/d3d12.dll \
  --ro-bind "$vkd3d/d3d12core.dll" /application/d3d12core.dll \
  --ro-bind "$dxgi" /application/dxgi.dll \
  --ro-bind "$compiler" /application/d3dcompiler_47.dll \
  "${binds[@]}" \
  "${wine_setup[@]}" \
  "${display[@]}" \
  --setenv PATH /usr/bin --setenv LANG C.UTF-8 --setenv HOME "$HOME" \
  --setenv WINEPREFIX /state/prefix \
  --setenv WINEDEBUG -all,err+virtual,err+seh \
  --setenv WINEDLLOVERRIDES 'd3d12,d3d12core,dxgi=n,b;d3dcompiler_47=n' \
  --setenv VICTORY_DATA_DIR 'Z:\state\data' \
  --setenv VICTORY_CACHE_DIR 'Z:\state\cache\victory' \
  --setenv MESA_SHADER_CACHE_DIR /state/cache/mesa \
  --setenv VKD3D_SHADER_CACHE_PATH /state/cache/vkd3d \
  "${environment[@]}" \
  --chdir /state \
  -- /usr/bin/bash -c '"$1" "${@:3}"; status=$?; "$2" -w; exit $status' \
  victory "$wine" "${wine%wine}wineserver" /application/victory.exe "${args[@]}"
