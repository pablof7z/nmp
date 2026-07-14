#!/usr/bin/env bash
set -euo pipefail

SCRIPT=$(cd "$(dirname "$0")" && pwd)/build-swift-xcframework.sh
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

REPO="$TMP/repo"
BIN="$TMP/bin"
mkdir -p "$REPO/scripts" "$BIN"
cp "$SCRIPT" "$REPO/scripts/"
git -C "$REPO" init -q

cat > "$BIN/cargo" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'cargo' >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf '\n' >> "$CALL_LOG"

case "${1:-}" in
  build)
    target=
    while [[ $# -gt 0 ]]; do
      if [[ $1 == --target ]]; then
        target=$2
        break
      fi
      shift
    done
    [[ -n $target ]]
    mkdir -p "$CARGO_TARGET_DIR/$target/release"
    : > "$CARGO_TARGET_DIR/$target/release/libnmp_ffi.a"
    ;;
  run)
    out_dir=
    library=
    while [[ $# -gt 0 ]]; do
      case "$1" in
        --out-dir) out_dir=$2; shift 2 ;;
        --library) library=$2; shift 2 ;;
        *) shift ;;
      esac
    done
    [[ -f $library ]]
    mkdir -p "$out_dir"
    : > "$out_dir/nmp_ffi.swift"
    : > "$out_dir/nmp_ffiFFI.h"
    : > "$out_dir/nmp_ffiFFI.modulemap"
    ;;
  *) exit 64 ;;
esac
SHIM

cat > "$BIN/lipo" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'lipo' >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf '\n' >> "$CALL_LOG"
if [[ ${1:-} == -create ]]; then
  while [[ $# -gt 0 ]]; do
    if [[ $1 == -output ]]; then
      mkdir -p "$(dirname "$2")"
      : > "$2"
      break
    fi
    shift
  done
fi
SHIM

cat > "$BIN/xcodebuild" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'xcodebuild' >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf '\n' >> "$CALL_LOG"
while [[ $# -gt 0 ]]; do
  if [[ $1 == -output ]]; then
    mkdir -p "$2"
    break
  fi
  shift
done
SHIM
chmod +x "$BIN/"*

run_script() {
  local log=$1 target_dir=$2
  shift 2
  : > "$log"
  (
    cd "$REPO"
    PATH="$BIN:$PATH" \
      CALL_LOG="$log" \
      CARGO_TARGET_DIR="$target_dir" \
      scripts/build-swift-xcframework.sh "$@"
  )
}

assert_no_calls() {
  [[ ! -s $1 ]] || {
    echo "unexpected tool call:" >&2
    cat "$1" >&2
    exit 1
  }
}

# Help and invalid input must finish before repository discovery or any tool.
help_log="$TMP/help.log"
: > "$help_log"
help_output=$(cd / && PATH="$BIN:$PATH" CALL_LOG="$help_log" \
  "$REPO/scripts/build-swift-xcframework.sh" --help)
grep -Fq -- '--macos-only' <<< "$help_output"
assert_no_calls "$help_log"

for args in '--unknown' '--sim-only --macos-only' 'positional'; do
  invalid_log="$TMP/invalid.log"
  : > "$invalid_log"
  if (cd / && PATH="$BIN:$PATH" CALL_LOG="$invalid_log" \
    "$REPO/scripts/build-swift-xcframework.sh" $args >/dev/null 2>&1); then
    echo "invalid invocation unexpectedly passed: $args" >&2
    exit 1
  fi
  assert_no_calls "$invalid_log"
done
echo 'ok - help and argument rejection are side-effect-free'

# macOS-only uses the shared target directory for build, bindgen input,
# headers, and the one xcframework library. It performs no simulator work.
mac_log="$TMP/macos.log"
shared_target="$TMP/shared-cache"
run_script "$mac_log" "$shared_target" --macos-only >/dev/null
grep -Fq 'cargo build -p nmp-ffi --release --target aarch64-apple-darwin' "$mac_log"
grep -Fq "$shared_target/aarch64-apple-darwin/release/libnmp_ffi.a" "$mac_log"
grep -Fq "$shared_target/ios-ffi-headers" "$mac_log"
! grep -Fq 'apple-ios' "$mac_log"
! grep -Fq 'lipo' "$mac_log"
[[ $(grep -c '^xcodebuild ' "$mac_log") -eq 1 ]]
echo 'ok - macOS-only plan uses the caller target directory and no simulator'

# Relative CARGO_TARGET_DIR resolves from the repository root for both Cargo
# and packaging lookups.
relative_log="$TMP/relative.log"
run_script "$relative_log" relative-target --macos-only >/dev/null
grep -Fq "$REPO/relative-target/aarch64-apple-darwin/release/libnmp_ffi.a" "$relative_log"
echo 'ok - relative CARGO_TARGET_DIR artifact lookup matches Cargo'

# Preserve the historical sim-only and default target sets.
sim_log="$TMP/sim.log"
run_script "$sim_log" "$TMP/sim-target" --sim-only >/dev/null
grep -Fq -- '--target aarch64-apple-ios-sim' "$sim_log"
grep -Fq -- '--target x86_64-apple-ios' "$sim_log"
grep -Fq -- '--target aarch64-apple-darwin' "$sim_log"
! grep -Eq -- 'cargo build .* --target aarch64-apple-ios$' "$sim_log"
grep -Fq 'lipo -create' "$sim_log"

default_log="$TMP/default.log"
run_script "$default_log" "$TMP/default-target" >/dev/null
grep -Fq -- '--target aarch64-apple-ios-sim' "$default_log"
grep -Fq -- '--target x86_64-apple-ios' "$default_log"
grep -Fq -- '--target aarch64-apple-darwin' "$default_log"
grep -Eq -- 'cargo build .* --target aarch64-apple-ios$' "$default_log"
grep -Fq 'lipo -create' "$default_log"
echo 'ok - sim-only and default target sets remain compatible'
