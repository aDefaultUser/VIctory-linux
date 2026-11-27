#!/usr/bin/env bash
# Checks scripts/run-linux.sh without Wine, a title or a GPU: a stand-in
# bwrap writes down the command line it is given, and every file is made in
# a temporary folder.
#
#   scripts/check-run-linux.sh [REFERENCE]
#
# With REFERENCE (another run-linux.sh, for example one from
# `git show <commit>:scripts/run-linux.sh`), the commands that are accepted
# must give bwrap the same command line as it does.
set -euo pipefail

script=$(realpath "$(dirname "${BASH_SOURCE[0]}")/run-linux.sh")
reference=${1:+$(realpath "$1")}
work=$(realpath "$(mktemp -d "${TMPDIR:-/tmp}/check-run-linux.XXXXXX")")
trap 'rm -rf -- "$work"' EXIT
cd "$work"
# run-linux.sh rightly refuses every folder in a checkout.
dir=$work
while [[ $dir != / ]]; do
  [[ ! -f $dir/.victory-root ]] || { echo "check-run-linux.sh: set TMPDIR outside the checkout $dir" >&2; exit 2; }
  dir=$(dirname "$dir")
done

mkdir -p bin home runtime build proton/files/bin title/sub shaders/folder clone \
  proton/files/lib/wine/dxvk/x86_64-windows proton/files/lib/wine/vkd3d-proton/x86_64-windows
touch build/victory.exe d3dcompiler_47.dll title/Title.exe title/sub/Other.exe clone/.victory-root \
  proton/files/lib/wine/dxvk/x86_64-windows/dxgi.dll \
  proton/files/lib/wine/vkd3d-proton/x86_64-windows/{d3d12,d3d12core}.dll
install -m 755 /dev/null proton/files/bin/wine
install -m 755 /dev/null wineserver
echo archive >shaders/a.pak
cat >bin/bwrap <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" >"$BWRAP_ARGV"
while (($# >= 3)); do
  [[ $1 != --bind || $3 != /output/list.txt ]] || echo listed >"$2"
  shift
done
exit "${BWRAP_STATUS:-0}"
EOF
chmod +x bin/bwrap

failures=0
fail() { echo "FAIL: $case: $*"; sed 's/^/  | /' "$work/out"; failures=$((failures + 1)); }

# launch SCRIPT ARGS...: runs it with the environment in `with` added.
with=()
launch() {
  rm -f "$work/argv"
  status=0
  env -i PATH="$work/bin:/usr/bin:/bin" HOME="$work/home" XDG_DATA_HOME="$work/home/data" \
    XDG_CACHE_HOME="$work/home/cache" XDG_RUNTIME_DIR="$work/runtime" DISPLAY=:9 \
    PROTON_WINE_LIBS="$work/proton/files/lib/wine" D3DCOMPILER="$work/d3dcompiler_47.dll" \
    RUN_LINUX_EXE="$work/build/victory.exe" RUN_LINUX_NTSYNC=0 BWRAP_ARGV="$work/argv" \
    "${with[@]}" bash "$@" >"$work/out" 2>&1 || status=$?
}

# The host path of the list differs by design; the rest must not.
normalized() { awk '{ line[NR] = $0 } $0 == "/output/list.txt" { line[NR - 1] = "LIST" }
  END { for (i = 1; i <= NR; i++) print line[i] }' "$1"; }

accepted() {
  case=$*
  launch "$script" "$@"
  ((status == 0)) && [[ -f argv ]] || { fail "refused (status $status)"; return; }
  [[ -z $reference ]] && return
  mv argv argv.new
  launch "$reference" "$@"
  [[ -f argv ]] || { fail "the reference did not start bwrap"; return; }
  diff <(normalized argv) <(normalized argv.new) >out || fail "command line differs from the reference"
  mv argv.new argv
}

refused() {
  case=$*
  launch "$script" "$@"
  ((status != 0)) && [[ ! -f argv ]] || fail "accepted (status $status)"
}

has_line() { grep -qxF -- "$1" argv || fail "no '$1' on the command line"; }
same() { [[ $(cat "$1") == "$2" ]] || fail "$1 is now '$(cat "$1")', not '$2'"; }
absent() { [[ ! -e $1 ]] || fail "$1 was made"; }

# Valid commands.
accepted run title Title.exe
has_line "$work/title" && has_line 'Z:\title\Title.exe'
accepted run title sub/Other.exe VICTORY_OUTPUT=2560x1440 VICTORY_THIN= -- -windowed
has_line 'Z:\title\sub\Other.exe' && has_line -windowed
accepted shaders shaders/a.pak shaders/folder --jobs 4 VICTORY_PORT_CBV=0
has_line /input/0/a.pak && has_line /input/1/folder
accepted warm --used --jobs 2
with=(RUN_LINUX_WINE=wine)
accepted warm
with=(RUN_LINUX_WINESERVER="$work/wineserver")
accepted run title Title.exe
with=()

# The list replaces FILE only after a run that succeeded.
echo old >kept.txt
accepted shaders shaders/a.pak --list kept.txt
same kept.txt listed
same shaders/a.pak archive
[[ -z $(find home/cache -name '.shaders-list.*') ]] || fail "a staged list is left in the cache"
echo old >kept.txt
with=(BWRAP_STATUS=3)
case="a failed run keeps the list"
launch "$script" shaders shaders/a.pak --list kept.txt
((status == 3)) || fail "status $status, not 3"
same kept.txt old
[[ -z $(find home/cache -name '.shaders-list.*'; find . -maxdepth 1 -name '.kept.txt.*') ]] || fail "a staged list is left"
with=()

# Lists that must not be written.
refused shaders shaders/a.pak --list shaders/a.pak
same shaders/a.pak archive
refused shaders missing.pak --list kept.txt
same kept.txt old
echo victim >victim.txt
ln -s victim.txt link.txt
refused shaders shaders/a.pak --list link.txt
same victim.txt victim
refused shaders shaders/folder --list shaders/folder/list.txt
absent shaders/folder/list.txt
refused shaders shaders/a.pak --list clone/list.txt
absent clone/list.txt
refused shaders shaders/a.pak --list shaders/a.pak/
refused shaders shaders/a.pak --list no-folder/list.txt
refused shaders shaders/a.pak --list shaders
refused shaders shaders/a.pak --list one.txt --list two.txt

# Writable folders that alias the checkout, the title or what is read.
with=(RUN_LINUX_DATA="$work/title")
refused run title Title.exe
with=(RUN_LINUX_DATA="$work/title/saves")
refused run title Title.exe
absent title/saves
with=(RUN_LINUX_CACHE="$work/clone/cache")
refused warm
absent clone/cache
with=(RUN_LINUX_PREFIX="$work")
refused warm
with=(RUN_LINUX_DATA="$work/shaders")
refused shaders shaders/a.pak
with=(RUN_LINUX_CACHE="$work/build")
refused warm
# (A folder that holds this checkout: one in it is the clone's case above.)
with=(RUN_LINUX_DATA="$(dirname "$(dirname "$(dirname "$script")")")")
refused warm
with=()
refused run title ../build/victory.exe

# A copy of saved data of its own, through a link too, is fine.
mkdir -p title-saves/4d530000/saves
echo save >title-saves/4d530000/saves/one
ln -s title-saves saves-link
with=(RUN_LINUX_DATA="$work/title-saves")
accepted run title Title.exe
has_line "$work/title-saves"
# (The reference binds the link itself.)
saved_reference=$reference reference=
with=(RUN_LINUX_DATA="$work/saves-link")
accepted run title Title.exe
reference=$saved_reference
has_line "$work/title-saves"
same title-saves/4d530000/saves/one save

if ((failures)); then
  echo "$failures failed"
  exit 1
fi
echo "run-linux.sh: all checks passed${reference:+ (command lines as $1)}"
