#!/usr/bin/env bash
set -euo pipefail

SCRIPT=$(cd "$(dirname "$0")" && pwd)/build-swift-xcframework.sh
CHECKER=$(cd "$(dirname "$0")" && pwd)/check-macos-deployment-target.sh
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

REPO="$TMP/repo"
BIN="$TMP/bin"
mkdir -p "$REPO/scripts" "$REPO/Packages/NMP" "$BIN"
cp "$SCRIPT" "$REPO/scripts/"
cp "$CHECKER" "$REPO/scripts/"
git -C "$REPO" init -q

cat > "$REPO/Packages/NMP/Package.swift" <<'SWIFT'
// swift-tools-version: 5.9
import PackageDescription
let package = Package(
    name: "Fixture",
    platforms: [
        .macOS(.v13),
    ]
)
SWIFT

cat > "$BIN/cargo" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'cargo' >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf ' deployment=%s' "${MACOSX_DEPLOYMENT_TARGET-unset}" >> "$CALL_LOG"
printf ' cflags=%q' "${CFLAGS-unset}" >> "$CALL_LOG"
printf ' cxxflags=%q' "${CXXFLAGS-unset}" >> "$CALL_LOG"
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

cat > "$BIN/otool" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'otool' >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf '\n' >> "$CALL_LOG"
artifact=
for arg in "$@"; do artifact=$arg; done
case "${OTOOL_FIXTURE:-valid}" in
  valid)
    cat <<EOF
Archive : $artifact
$artifact(first.o):
Load command 0
      cmd LC_BUILD_VERSION
 platform 1
    minos 13.0
$artifact(second.o):
Load command 0
      cmd LC_BUILD_VERSION
 platform 1
    minos 11.0
EOF
    ;;
  newer)
    cat <<EOF
Archive : $artifact
$artifact(newer.o):
Load command 0
      cmd LC_BUILD_VERSION
 platform 1
    minos 14.0
EOF
    ;;
  exact-old)
    cat <<EOF
$artifact:
Load command 0
      cmd LC_BUILD_VERSION
 platform 1
    minos 12.0
EOF
    ;;
  exact)
    cat <<EOF
$artifact:
Load command 0
      cmd LC_BUILD_VERSION
 platform 1
    minos 13.0
EOF
    ;;
  missing)
    cat <<EOF
Archive : $artifact
$artifact(no-target.o):
Load command 0
      cmd LC_SEGMENT_64
EOF
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
chmod +x "$REPO/scripts/"*.sh

run_script() {
  local log=$1 target_dir=$2
  shift 2
  : > "$log"
  (
    cd "$REPO"
    PATH="$BIN:$PATH" \
      CALL_LOG="$log" \
      CARGO_TARGET_DIR="$target_dir" \
      MACOSX_DEPLOYMENT_TARGET=99.0 \
      CFLAGS=-mmacosx-version-min=99.0 \
      CXXFLAGS=-mmacosx-version-min=99.0 \
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
grep -Fq -- '--target aarch64-apple-darwin deployment=13.0' "$mac_log"
grep -Fq 'cflags=-mmacosx-version-min=99.0\ -mmacosx-version-min=13.0' "$mac_log"
grep -Fq 'cxxflags=-mmacosx-version-min=99.0\ -mmacosx-version-min=13.0' "$mac_log"
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
grep -Fq -- '--target aarch64-apple-ios-sim deployment=unset' "$sim_log"
grep -Fq -- '--target x86_64-apple-ios deployment=unset' "$sim_log"
grep -Fq -- '--target aarch64-apple-darwin deployment=13.0' "$sim_log"
! grep -Fq -- '--target aarch64-apple-ios deployment=' "$sim_log"
grep -Fq 'lipo -create' "$sim_log"

default_log="$TMP/default.log"
run_script "$default_log" "$TMP/default-target" >/dev/null
grep -Fq -- '--target aarch64-apple-ios-sim' "$default_log"
grep -Fq -- '--target x86_64-apple-ios' "$default_log"
grep -Fq -- '--target aarch64-apple-darwin' "$default_log"
grep -Fq -- '--target aarch64-apple-ios deployment=unset' "$default_log"
grep -Fq 'lipo -create' "$default_log"
echo 'ok - sim-only and default target sets remain compatible'

# The standalone checker derives the package minimum, accepts older members,
# rejects one newer member or one missing load command, and can require the
# final linked image to encode the exact declared minimum.
checker_log="$TMP/checker.log"
: > "$checker_log"
artifact="$TMP/libfixture.a"
: > "$artifact"
(
  cd "$REPO"
  PATH="$BIN:$PATH" CALL_LOG="$checker_log" \
    scripts/check-macos-deployment-target.sh "$artifact" >/dev/null
)

for fixture in newer missing; do
  if (
    cd "$REPO"
    PATH="$BIN:$PATH" CALL_LOG="$checker_log" OTOOL_FIXTURE="$fixture" \
      scripts/check-macos-deployment-target.sh "$artifact" >/dev/null 2>&1
  ); then
    echo "deployment checker unexpectedly accepted $fixture archive" >&2
    exit 1
  fi
done

if (
  cd "$REPO"
  PATH="$BIN:$PATH" CALL_LOG="$checker_log" OTOOL_FIXTURE=exact-old \
    scripts/check-macos-deployment-target.sh --exact "$artifact" >/dev/null 2>&1
); then
  echo 'exact deployment checker unexpectedly accepted an older minimum' >&2
  exit 1
fi
(
  cd "$REPO"
  PATH="$BIN:$PATH" CALL_LOG="$checker_log" OTOOL_FIXTURE=exact \
    scripts/check-macos-deployment-target.sh --exact "$artifact" >/dev/null
)
echo 'ok - every archive member and exact final-image minimum are enforced'
