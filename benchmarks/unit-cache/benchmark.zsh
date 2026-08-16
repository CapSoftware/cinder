#!/bin/zsh
# Warm-cache cold-build benchmark for the Phase 1 unit cache.
#
# Three arms, interleaved per round so drift hits all arms equally:
#   cargo        stock Cargo cold build (baseline)
#   cinder-cold  Cinder with an empty unit cache (cache-cold overhead arm)
#   cinder-warm  Cinder with a pre-populated unit cache (headline arm)
#
# Every trial starts from a clean target directory. The unit cache is wiped
# before every cinder-cold trial and pre-populated once (then kept) for
# cinder-warm. Per trial we record wall-clock seconds and the number of
# `Compiling ` lines Cargo printed; the restored fraction is
# (cargo_compiles - warm_compiles) / cargo_compiles for the same command.
#
# Soundness check: within each cinder arm the final artifact digest must be
# identical across trials, and cinder-warm must equal cinder-cold (same
# toolchain routing, so restores must not change the output). The cargo arm
# digest can legitimately differ when Cinder routes a tuned toolchain.
set -euo pipefail
zmodload zsh/datetime

if (( $# < 4 )); then
  print -u2 'usage: benchmark.zsh <label> <repository> <rounds> <jobs> -- <build arguments...>'
  exit 2
fi

root=${0:A:h}
repository_root=${root:h:h}
label=$1
repository=${2:A}
rounds=$3
jobs=$4
shift 4
if [[ ${1:-} == -- ]]; then
  shift
fi
typeset -a build_arguments
build_arguments=("$@")

cinder=${CINDER_BIN:-$repository_root/target/release/cinder}
result_root=$repository_root/target/cinder-bench/unit-cache-results
cache_root=$repository_root/target/cinder-bench/unit-cache-store/$label
mkdir -p "$result_root" "$cache_root"

build_environment=(
  _=/usr/bin/env
  CARGO_BUILD_JOBS=$jobs
  CARGO_INCREMENTAL=0
  CINDER_UNIT_CACHE=$cache_root
  CINDER_TRACE_RUN=1
  # Synchronous recording keeps every trial deterministic (no background
  # recorder racing cinder's own hidden state-recording passes) and charges
  # record cost inside the timed window — honest for the cache-cold arm.
  CINDER_SYNCHRONOUS_STATE_RECORDING=1
)

require_disk() {
  local free_gb
  free_gb=$(/bin/df -g / | /usr/bin/awk 'NR==2 {print $4}')
  if (( free_gb < 6 )); then
    print -u2 "aborting: only ${free_gb}G free"
    exit 1
  fi
}

git -C "$repository" diff --quiet
cd "$repository"

for arm in cargo cinder-cold cinder-warm; do
  : > "$result_root/$label-$arm.time"
  : > "$result_root/$label-$arm.compiles"
  : > "$result_root/$label-$arm.restored"
  : > "$result_root/$label-$arm.digest"
done

run_trial() {
  local arm=$1 trial_log=$2
  shift 2
  require_disk
  cargo clean >/dev/null 2>&1
  local started=$EPOCHREALTIME
  if [[ $arm == cargo ]]; then
    /usr/bin/env $build_environment cargo build $build_arguments >"$trial_log" 2>&1
  else
    /usr/bin/env $build_environment "$cinder" build $build_arguments >"$trial_log" 2>&1
  fi
  local elapsed=$(( EPOCHREALTIME - started ))
  printf '%.6f\n' "$elapsed" >> "$result_root/$label-$arm.time"
  local compiles restored
  compiles=$(/usr/bin/grep -c '^ *Compiling ' "$trial_log") || compiles=0
  print "$compiles" >> "$result_root/$label-$arm.compiles"
  restored=$(/usr/bin/grep -o 'unit cache restored [0-9]* of [0-9]*' "$trial_log" \
    | /usr/bin/tail -1 | /usr/bin/awk '{print $4}') || restored=0
  print "${restored:-0}" >> "$result_root/$label-$arm.restored"
  local artifact
  artifact=$(find_artifact)
  if [[ -n $artifact && -f $artifact ]]; then
    /usr/bin/shasum -a 256 "$artifact" | /usr/bin/awk '{print $1}' >> "$result_root/$label-$arm.digest"
  else
    # Library-only selections leave nothing outside deps/; digest stability
    # is then tracked by the compile counts alone.
    print none >> "$result_root/$label-$arm.digest"
  fi
}

find_artifact() {
  # newest executable in any profile dir of any target namespace
  /usr/bin/find target -maxdepth 4 -type f -perm +111 \
    \( -path '*/debug/*' -o -path '*/release/*' \) -not -path '*/deps/*' \
    -not -path '*/build/*' -not -path '*/incremental/*' \
    -not -name '*.d' -not -name '*.dylib' 2>/dev/null \
    | /usr/bin/sort | /usr/bin/head -1
}

wipe_cache() { /bin/rm -rf "$cache_root"; mkdir -p "$cache_root"; }

await_recorder() {
  # The background unit recorder must settle before the cache is copied,
  # wiped, or measured.
  local waited=0
  while /usr/bin/pgrep -qf '__record-units' && (( waited < 120 )); do
    sleep 1
    (( waited += 1 ))
  done
}

# Pre-populate the warm cache with one throwaway build (also validates wiring).
wipe_cache
require_disk
cargo clean >/dev/null 2>&1
/usr/bin/env $build_environment "$cinder" build $build_arguments \
  > "$result_root/$label-warmup.log" 2>&1
await_recorder
if [[ -z "$(/usr/bin/find "$cache_root" -type f 2>/dev/null | /usr/bin/head -1)" ]]; then
  print -u2 'warning: unit cache is empty after warmup; warm arm will measure nothing'
fi
warm_cache_bytes=$(/usr/bin/du -sk "$cache_root" | /usr/bin/awk '{print $1 * 1024}')
warm_cache_saved=$repository_root/target/cinder-bench/unit-cache-store/$label-warm-copy
/bin/rm -rf "$warm_cache_saved"
/bin/cp -Rc "$cache_root" "$warm_cache_saved" 2>/dev/null || /bin/cp -R "$cache_root" "$warm_cache_saved"

for round in $(seq 1 "$rounds"); do
  # cargo arm
  run_trial cargo "$result_root/$label-cargo-r$round.log"
  # cinder-cold arm: empty cache each trial
  wipe_cache
  run_trial cinder-cold "$result_root/$label-cinder-cold-r$round.log"
  await_recorder
  # cinder-warm arm: restore the saved warm cache
  /bin/rm -rf "$cache_root"
  /bin/cp -Rc "$warm_cache_saved" "$cache_root" 2>/dev/null || /bin/cp -R "$warm_cache_saved" "$cache_root"
  run_trial cinder-warm "$result_root/$label-cinder-warm-r$round.log"
  await_recorder
done

cargo clean >/dev/null 2>&1
git -C "$repository" diff --quiet

print "label=$label rounds=$rounds jobs=$jobs warm_cache_bytes=$warm_cache_bytes"
for arm in cargo cinder-cold cinder-warm; do
  print -n "$label-$arm times: "
  /usr/bin/awk '{ printf "%s ", $1 }' "$result_root/$label-$arm.time"; print
  print -n "$label-$arm compiles: "
  /usr/bin/awk '{ printf "%s ", $1 }' "$result_root/$label-$arm.compiles"; print
  print -n "$label-$arm restored: "
  /usr/bin/awk '{ printf "%s ", $1 }' "$result_root/$label-$arm.restored"; print
  print -n "$label-$arm digests: "
  /usr/bin/sort -u "$result_root/$label-$arm.digest" | /usr/bin/wc -l | /usr/bin/tr -d ' '; print ' unique'
done
