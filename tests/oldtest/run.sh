#!/usr/bin/env bash
# Run upstream Neovim's legacy oldtest suite against oxvim.
#
# Each selected test file runs alone in a throwaway copy of testdir with a
# private HOME, XDG tree, and TMPDIR, so a failing test cannot leak state into
# the next one and no run can delete the caller's $HOME. stdin is /dev/null:
# headless oxvim waits for input otherwise.
#
# The suite is driven through `runtest.vim` directly rather than through the
# upstream Makefile. The Makefile's `%.res` rule wraps every file in
# `runnvim.sh`, which starts the test process with `jobstart(..., {'term':
# v:true})` and reads the result back as terminal-buffer lines; oxvim does not
# paint a terminal buffer yet, so that path reports an empty screen and exits
# non-zero before any test runs.
#
# usage: run.sh [--jobs N] [--timeout S] [--out DIR] [test_foo.vim ...]
set -euo pipefail

trap 'exit 130' INT TERM
trap 'status=$?; if ((status != 0 && status != 130)); then printf "oldtest runner exited %d\n" "$status" >&2; fi' EXIT

repo=$(cd "$(dirname "$0")/../.." && pwd)
ref="$repo/.references/neovim"
subject=${OXVIM_BIN:-$repo/target/release/oxvim}
jobs=8
deadline=300
out=$repo/.outline/evidence/oldtest
targets=()

while (($# > 0)); do
  case $1 in
    --jobs) jobs=${2:?--jobs needs a value}; shift 2 ;;
    --timeout) deadline=${2:?--timeout needs a value}; shift 2 ;;
    --out) out=${2:?--out needs a value}; shift 2 ;;
    --) shift; targets+=("$@"); break ;;
    -*) printf 'unknown option: %s\n' "$1" >&2; exit 2 ;;
    *) targets+=("$1"); shift ;;
  esac
done

if [[ ! -x $subject ]]; then
  printf 'oxvim binary missing: %s\n' "$subject" >&2
  exit 1
fi

mkdir -p "$out/logs" "$out/base"

# The committed stale logs must not be readable by any run, and test_alot is
# excluded because it sources every other file into one process.
rsync -a --delete --exclude 'test.log' --exclude 'messages' --exclude 'test.res' \
  --exclude 'message' --exclude 'X-test-tmpdir' --exclude 'XfakeHOME' \
  --exclude 'test_alot.vim' --exclude 'test_largefile.vim' \
  "$ref/test/old/testdir/" "$out/base/"

if ((${#targets[@]} == 0)); then
  mapfile -t targets < <(cd "$out/base" && ls test_*.vim | sort)
fi

run_one() {
  local file=$1 name=${1%.vim} scratch rc=0
  scratch=$(mktemp -d "${TMPDIR:-/tmp}/oxold.XXXXXX")
  if ! cp -a "$out/base" "$scratch/testdir"; then
    printf '%s\tcrash\t0\t0\t0\n' "$file"
    rm -rf -- "${scratch:?}"
    return 0
  fi
  mkdir -p "$scratch/home" "$scratch/xdg"/{config,data,state,cache} "$scratch/tmp"
  (
    cd "$scratch/testdir" || exit 90
    HOME="$scratch/home" TMPDIR="$scratch/tmp" NVIM_TEST=1 LC_ALL=en_US.UTF-8 \
      XDG_CONFIG_HOME="$scratch/xdg/config" XDG_DATA_HOME="$scratch/xdg/data" \
      XDG_STATE_HOME="$scratch/xdg/state" XDG_CACHE_HOME="$scratch/xdg/cache" \
      HISTFILE=/dev/null SHELL=sh SYSTEM_NAME=Linux \
      env -u XDG_DATA_DIRS -u NVIM -u TMUX -u VIM -u VIMRUNTIME \
      timeout "$deadline" "$subject" -u NONE -i NONE --noplugin --headless \
        --cmd 'set shortmess-=F backupdir=. undodir=. viewdir=.' \
        -S runtest.vim "$file" </dev/null >"$scratch/stdout.txt" 2>&1
  ) || rc=$?

  local msg="$scratch/testdir/messages" executed=0 failed=0 skipped=0
  if [[ -f $msg ]]; then
    executed=$(grep -aoE '^Executed [0-9]+ tests?' "$msg" | grep -oE '[0-9]+' | awk '{s+=$1} END{print s+0}')
    failed=$(grep -aoE '^[0-9]+ FAILED:' "$msg" | grep -oE '^[0-9]+' | awk '{s+=$1} END{print s+0}')
    skipped=$(grep -ac '^SKIPPED' "$msg" || true)
    cp -f "$msg" "$out/logs/$name.messages"
  else
    printf 'From %s:\n  harness produced no messages (exit %s)\n' "$file" "$rc" \
      >"$out/logs/$name.messages"
  fi
  local outcome
  if ((rc == 124)); then outcome=timeout
  elif ((rc == 134 || rc == 139)); then outcome=crash
  elif ((executed == 0)); then outcome=blocked
  elif ((failed == 0)); then outcome=ok
  else outcome=partial
  fi
  printf '%s\t%s\t%s\t%s\t%s\n' "$file" "$outcome" "$executed" "$failed" "$skipped"
  rm -rf -- "${scratch:?}"
}
export -f run_one
export subject out deadline

printf '%s\n' "${targets[@]}" | xargs -P "$jobs" -n 1 bash -c 'run_one "$1"' _ |
  tee "$out/census.tsv"

: >"$out/messages"
while IFS= read -r file; do
  name=${file%.vim}
  cat "$out/logs/$name.messages" >>"$out/messages"
done < <(cut -f1 "$out/census.tsv")

executed=$(awk -F'\t' '{s+=$3} END{print s+0}' "$out/census.tsv")
failed=$(awk -F'\t' '{s+=$4} END{print s+0}' "$out/census.tsv")
skipped=$(awk -F'\t' '{s+=$5} END{print s+0}' "$out/census.tsv")
blocked=$(awk -F'\t' '$2=="blocked"' "$out/census.tsv" | wc -l)
timeouts=$(awk -F'\t' '$2=="timeout" || $2=="crash"' "$out/census.tsv" | wc -l)
clean=$(awk -F'\t' '$2=="ok"' "$out/census.tsv" | wc -l)
printf 'oldtest: files=%d executed=%d failed=%d skipped=%d clean=%d blocked=%d timeout-or-crash=%d\n' \
  "${#targets[@]}" "$executed" "$failed" "$skipped" "$clean" "$blocked" "$timeouts"
printf 'results: %s/messages\n' "$out"

if ((executed == 0)); then
  printf 'oldtest executed no tests: the harness result is invalid.\n' >&2
  exit 1
fi
((failed == 0)) || exit 1
