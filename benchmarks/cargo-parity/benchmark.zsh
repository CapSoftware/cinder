#!/bin/zsh
set -euo pipefail
zmodload zsh/datetime

if (( $# < 6 )); then
  print -u2 'usage: benchmark.zsh <check|test> <label> <repository> <trials> <jobs> -- <cargo arguments...>'
  exit 2
fi

root=${0:A:h}
repository_root=${root:h:h}
mode=$1
label=$2
repository=${3:A}
trials=$4
jobs=$5
shift 5
if [[ ${1:-} == -- ]]; then
  shift
fi
typeset -a cargo_arguments command
cargo_arguments=("$@")
case $mode in
  check)
    command=(check)
    state_kind=check
    marker='Cinder reused the validated check'
    ;;
  test)
    command=(test --no-run)
    state_kind=test
    marker='Cinder reused the validated test build'
    ;;
  *)
    print -u2 "unsupported mode: $mode"
    exit 2
    ;;
esac

cinder=${CINDER_BIN:-$repository_root/target/release/cinder}
result_root=$repository_root/target/cinder-bench/parity-results
mkdir -p "$result_root"
cinder_times=$result_root/$label-cinder-$mode.time
cargo_times=$result_root/$label-cargo-$mode.time
benchmark_log=$result_root/$label-$mode.log
build_environment=(
  _=/usr/bin/env
  CARGO_PROFILE_DEV_DEBUG=0
  CARGO_BUILD_JOBS=$jobs
  CINDER_SYNCHRONOUS_STATE_RECORDING=1
)

git -C "$repository" diff --quiet
cd "$repository"
: > "$cinder_times"
: > "$cargo_times"
: > "$benchmark_log"

timed_run() {
  local output=$1
  shift
  local started=$EPOCHREALTIME
  "$@"
  local elapsed=$(( EPOCHREALTIME - started ))
  printf '%.6f\n' "$elapsed" >> "$output"
}

/usr/bin/env $build_environment "$cinder" $command $cargo_arguments >>"$benchmark_log" 2>&1

state_project=
for workspace_file in "$TMPDIR"/cinder/state/*/workspace(N); do
  if [[ "$(<"$workspace_file")" == "$repository" && -f "${workspace_file:h}/$state_kind/artifact" ]]; then
    state_project=${workspace_file:h}
    break
  fi
done
if [[ -z $state_project ]]; then
  print -u2 'Cinder did not capture state. Force one honest compiler invocation with `cinder clean -p <package>` or by touching the selected source, then retry.'
  exit 1
fi
artifact="$(<"$state_project/$state_kind/artifact")"
test -f "$artifact"
artifact_digest=$(/usr/bin/shasum -a 256 "$artifact" | /usr/bin/awk '{print $1}')

for trial in $(seq 1 "$trials"); do
  trial_log=$result_root/$label-cinder-$mode-trial-$trial.log
  timed_run "$cinder_times" \
    /usr/bin/env $build_environment "$cinder" $command $cargo_arguments >"$trial_log" 2>&1
  /usr/bin/grep -q "$marker" "$trial_log"
  test "$(/usr/bin/shasum -a 256 "$artifact" | /usr/bin/awk '{print $1}')" = "$artifact_digest"
done

/usr/bin/env $build_environment cargo $command $cargo_arguments >>"$benchmark_log" 2>&1
for trial in $(seq 1 "$trials"); do
  timed_run "$cargo_times" \
    /usr/bin/env $build_environment cargo $command $cargo_arguments >>"$benchmark_log" 2>&1
  test "$(/usr/bin/shasum -a 256 "$artifact" | /usr/bin/awk '{print $1}')" = "$artifact_digest"
done

git -C "$repository" diff --quiet
print -n "$label-cinder-$mode "
/usr/bin/awk '{ printf "%s ", $1 }' "$cinder_times"
print
print -n "$label-cargo-$mode "
/usr/bin/awk '{ printf "%s ", $1 }' "$cargo_times"
print
