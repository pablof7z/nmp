#!/usr/bin/env bash
set -Eeuo pipefail

usage() {
    printf '%s\n' \
        'usage: run-capstone.sh RUN_DIR EVIDENCE_DIR' \
        '' \
        'Builds the current macOS NMP FFI, starts the unrestricted relay fixture,' \
        'runs all Swift wrapper phases, stages outages/restarts, and tears down.'
}

die() {
    printf 'nmp Swift consumer runner: %s\n' "$*" >&2
    exit 1
}

[[ ${1:-} != --help && ${1:-} != -h ]] || {
    usage
    exit 0
}
[[ $# == 2 ]] || {
    usage >&2
    exit 2
}

repo_root=$(git rev-parse --show-toplevel)
run_dir=$1
evidence_dir=$2
harness="$repo_root/tools/nip29-consumer-harness/harness.sh"
package="$repo_root/tools/nip29-consumer-swift"
[[ -x "$harness" ]] || die "relay harness is missing or not executable: $harness"
[[ ! -e "$run_dir" ]] || die "run directory already exists: $run_dir"
[[ ! -e "$evidence_dir" ]] || die "evidence directory already exists: $evidence_dir"
mkdir -p "$evidence_dir"

cleanup_needed=1
cleanup() {
    if [[ $cleanup_needed == 1 && -f "$run_dir/.nmp-nip29-consumer-harness" ]]; then
        "$harness" stop "$run_dir" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT INT TERM

wait_for_ready() {
    local ready_file=$1
    local pid=$2
    local label=$3
    for _ in $(seq 1 1200); do
        [[ -f "$ready_file" ]] && return 0
        if ! kill -0 "$pid" 2>/dev/null; then
            wait "$pid" || true
            die "$label exited before reaching its staged boundary"
        fi
        sleep 0.1
    done
    die "$label did not reach its staged boundary"
}

print_proof_lines() {
    sed -n -e '/^PROOF/p' -e '/^PASS/p' -e '/^FAIL/p' "$@"
}

if [[ ${NMP_NIP29_SKIP_SWIFT_FFI_BUILD:-0} == 1 ]]; then
    printf '%s\n' 'reused XCFramework built earlier in this job' > "$evidence_dir/build-ffi.log"
else
    "$repo_root/scripts/build-swift-xcframework.sh" --macos-only \
        >"$evidence_dir/build-ffi.log" 2>&1
fi
swift build --package-path "$package" 2>&1 | tee "$evidence_dir/swift-build.log"

"$harness" start "$run_dir" | tee "$evidence_dir/harness-start.log"
manifest="$run_dir/manifest.json"
relay_a=$(jq -er '.relays.a' "$manifest")
relay_b=$(jq -er '.relays.b' "$manifest")
viewer=$(jq -er '.identities.viewer' "$manifest")
followed=$(jq -er '.identities.followed' "$manifest")
outsider=$(jq -er '.identities.outsider' "$manifest")
writer_secret_file="$run_dir/secrets/writer"

common=(
    --relay-a "$relay_a"
    --relay-b "$relay_b"
    --viewer "$viewer"
    --followed "$followed"
    --outsider "$outsider"
    --writer-secret-file "$writer_secret_file"
    --settle-secs 30
)

swift run --skip-build --package-path "$package" NIP29Consumer online \
    "${common[@]}" --store "$evidence_dir/online.redb" \
    | tee "$evidence_dir/online.log"

stage_dir="$evidence_dir/live-stages"
mkdir -p "$stage_dir"
swift run --skip-build --package-path "$package" NIP29Consumer live-adversarial \
    "${common[@]}" --store "$evidence_dir/adversarial.redb" --stage-dir "$stage_dir" \
    >"$evidence_dir/live-adversarial.log" 2>&1 &
adversarial_pid=$!
wait_for_ready "$stage_dir/mutate-live-inputs.ready" "$adversarial_pid" live-adversarial
"$harness" metadata-conflict "$run_dir" | tee "$evidence_dir/metadata-conflict.log"
"$harness" follow-remove "$run_dir" | tee "$evidence_dir/follow-remove.log"
"$harness" chat-append "$run_dir" | tee "$evidence_dir/chat-append.log"
: > "$stage_dir/mutate-live-inputs.continue"
wait_for_ready "$stage_dir/restore-follow.ready" "$adversarial_pid" live-adversarial
"$harness" follow-add "$run_dir" | tee "$evidence_dir/follow-add.log"
: > "$stage_dir/restore-follow.continue"
wait "$adversarial_pid"
print_proof_lines "$evidence_dir/live-adversarial.log"

"$harness" relay-down b "$run_dir" | tee "$evidence_dir/conflict-relay-b-down.log"
conflict_ready="$evidence_dir/restart-conflict.ready"
swift run --skip-build --package-path "$package" NIP29Consumer restart-conflict \
    "${common[@]}" --store "$evidence_dir/adversarial.redb" --ready-file "$conflict_ready" \
    >"$evidence_dir/restart-conflict.log" 2>&1 &
conflict_pid=$!
wait_for_ready "$conflict_ready" "$conflict_pid" restart-conflict
"$harness" relay-up b "$run_dir" | tee "$evidence_dir/conflict-relay-b-up.log"
wait "$conflict_pid"
print_proof_lines "$evidence_dir/restart-conflict.log"

"$harness" relay-down b "$run_dir" | tee "$evidence_dir/relay-b-down.log"
growth_ready="$evidence_dir/provenance-growth.ready"
swift run --skip-build --package-path "$package" NIP29Consumer provenance-growth \
    "${common[@]}" --store "$evidence_dir/growth.redb" --ready-file "$growth_ready" \
    >"$evidence_dir/provenance-growth.log" 2>&1 &
growth_pid=$!
wait_for_ready "$growth_ready" "$growth_pid" provenance-growth
"$harness" relay-up b "$run_dir" | tee "$evidence_dir/relay-b-up.log"
wait "$growth_pid"
print_proof_lines "$evidence_dir/provenance-growth.log"

"$harness" relay-down a "$run_dir" | tee "$evidence_dir/restart-relay-a-down.log"
"$harness" relay-down b "$run_dir" | tee "$evidence_dir/restart-relay-b-down.log"
restart_ready="$evidence_dir/restart.ready"
swift run --skip-build --package-path "$package" NIP29Consumer restart \
    "${common[@]}" --store "$evidence_dir/online.redb" --ready-file "$restart_ready" \
    >"$evidence_dir/restart.log" 2>&1 &
restart_pid=$!
wait_for_ready "$restart_ready" "$restart_pid" restart
"$harness" relay-up a "$run_dir" | tee "$evidence_dir/restart-relay-a-up.log"
"$harness" relay-up b "$run_dir" | tee "$evidence_dir/restart-relay-b-up.log"
wait "$restart_pid"
print_proof_lines "$evidence_dir/restart.log"

"$harness" stop "$run_dir" | tee "$evidence_dir/harness-stop.log"
cleanup_needed=0
print_proof_lines \
    "$evidence_dir/online.log" \
    "$evidence_dir/live-adversarial.log" \
    "$evidence_dir/restart-conflict.log" \
    "$evidence_dir/provenance-growth.log" \
    "$evidence_dir/restart.log" \
    > "$evidence_dir/proof-lines.txt"
printf 'NMP NIP-29 Swift capstone passed; evidence: %s\n' "$evidence_dir"
