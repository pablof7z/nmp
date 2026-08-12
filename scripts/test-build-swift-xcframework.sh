#!/usr/bin/env bash
set -euo pipefail

SCRIPT=$(cd "$(dirname "$0")" && pwd)/build-swift-xcframework.sh
CHECKER=$(cd "$(dirname "$0")" && pwd)/check-macos-deployment-target.sh
TOOL_HELPER=$(cd "$(dirname "$0")" && pwd)/lib/require-commands.sh
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

REPO="$TMP/repo"
BIN="$TMP/bin"
mkdir -p "$REPO/scripts/lib" "$REPO/Packages/NMP" "$BIN"
cp "$SCRIPT" "$REPO/scripts/"
cp "$CHECKER" "$REPO/scripts/"
cp "$TOOL_HELPER" "$REPO/scripts/lib/"
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
  fetch)
    ;;
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
    stem=$(basename "$library")
    stem=${stem#lib}
    stem=${stem%.*}
    mkdir -p "$out_dir"
    printf '%s\n' 'import Foundation' > "$out_dir/$stem.swift"
    : > "$out_dir/${stem}FFI.h"
    : > "$out_dir/${stem}FFI.modulemap"
    ;;
  *) exit 64 ;;
esac
SHIM

cat > "$BIN/rustup" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'rustup' >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf '\n' >> "$CALL_LOG"
case "${1:-} ${2:-}" in
  'show active-toolchain')
    printf '%s\n' \
      "nightly-fixture-aarch64-apple-darwin (overridden by '/fixture/rust-toolchain.toml')"
    ;;
  'target list')
    installed=${RUSTUP_INSTALLED_TARGETS-}
    # shellcheck disable=SC2086 # target triples never contain whitespace
    printf '%s\n' $installed
    ;;
  'target add')
    if [[ ${RUSTUP_ADD_FAILS:-0} != 0 ]]; then
      echo "error: fixture rustup cannot reach the toolchain manifest" >&2
      exit 1
    fi
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
      RUSTUP_INSTALLED_TARGETS="${RUSTUP_INSTALLED_TARGETS-}" \
      RUSTUP_ADD_FAILS="${RUSTUP_ADD_FAILS-0}" \
      scripts/build-swift-xcframework.sh "$@"
  )
}

# The first line of the log that matches a pattern, or the empty string.
first_call() {
  grep -n -- "$2" "$1" | head -1 | cut -d: -f1
}

added_targets() {
  sed -n 's/^rustup target add //p' "$1"
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
grep -Fq 'cargo build --frozen -p nmp-ffi --no-default-features --all-features --release --target aarch64-apple-darwin' "$mac_log"
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

# #1240: the builder installs the Rust targets it is about to build for, on the
# toolchain rust-toolchain.toml selects, before Cargo runs. Without that a clean
# consumer clone meets `can't find crate for core` and reads it as an API break.
RUSTUP_INSTALLED_TARGETS=
RUSTUP_ADD_FAILS=0

install_log="$TMP/install-macos.log"
run_script "$install_log" "$TMP/install-macos-target" --macos-only >/dev/null
[[ "$(added_targets "$install_log")" == 'aarch64-apple-darwin' ]]
install_line=$(first_call "$install_log" '^rustup target add ')
cargo_line=$(first_call "$install_log" '^cargo ')
[[ -n "$install_line" && -n "$cargo_line" && "$cargo_line" -gt "$install_line" ]] || {
  echo 'targets were not installed before Cargo ran:' >&2
  cat "$install_log" >&2
  exit 1
}

install_sim_log="$TMP/install-sim.log"
run_script "$install_sim_log" "$TMP/install-sim-target" --sim-only >/dev/null
[[ "$(added_targets "$install_sim_log")" == \
   'aarch64-apple-darwin aarch64-apple-ios-sim x86_64-apple-ios' ]]

install_all_log="$TMP/install-all.log"
run_script "$install_all_log" "$TMP/install-all-target" >/dev/null
[[ "$(added_targets "$install_all_log")" == \
   'aarch64-apple-darwin aarch64-apple-ios-sim x86_64-apple-ios aarch64-apple-ios' ]]

# Only the missing ones are installed, and an already-provisioned toolchain is
# left untouched.
RUSTUP_INSTALLED_TARGETS='aarch64-apple-darwin x86_64-apple-ios'
partial_log="$TMP/install-partial.log"
run_script "$partial_log" "$TMP/install-partial-target" --sim-only >/dev/null
[[ "$(added_targets "$partial_log")" == 'aarch64-apple-ios-sim' ]]

RUSTUP_INSTALLED_TARGETS='aarch64-apple-darwin aarch64-apple-ios-sim x86_64-apple-ios'
provisioned_log="$TMP/install-provisioned.log"
run_script "$provisioned_log" "$TMP/install-provisioned-target" --sim-only >/dev/null
! grep -q '^rustup target add' "$provisioned_log"
echo 'ok - every missing target is installed before Cargo, and no installed one is re-added'

# When the install cannot happen the build refuses, names the exact missing
# target and the command that supplies it, and never reaches Cargo.
refuse_log="$TMP/install-refused.log"
: > "$refuse_log"
if refusal=$(
  cd "$REPO"
  PATH="$BIN:$PATH" \
    CALL_LOG="$refuse_log" \
    CARGO_TARGET_DIR="$TMP/install-refused-target" \
    RUSTUP_INSTALLED_TARGETS= \
    RUSTUP_ADD_FAILS=1 \
    scripts/build-swift-xcframework.sh --sim-only 2>&1
); then
  echo 'the builder continued without the targets it needs' >&2
  exit 1
fi
grep -Fq 'no Rust standard library for: aarch64-apple-darwin aarch64-apple-ios-sim x86_64-apple-ios' \
  <<< "$refusal"
grep -Fq 'rustup target add aarch64-apple-darwin aarch64-apple-ios-sim x86_64-apple-ios' \
  <<< "$refusal"
grep -Fq 'nightly-fixture-aarch64-apple-darwin' <<< "$refusal"
! grep -q '^cargo ' "$refuse_log"

# The same refusal without rustup at all: a toolchain manager is what selects
# the pinned toolchain, so its absence cannot be silently built through.
norustup_log="$TMP/no-rustup.log"
: > "$norustup_log"
NO_RUSTUP_BIN="$TMP/no-rustup-bin"
mkdir -p "$NO_RUSTUP_BIN"
for tool in cargo otool lipo xcodebuild; do
  cp "$BIN/$tool" "$NO_RUSTUP_BIN/"
done
if missing_rustup=$(
  cd "$REPO"
  PATH="$NO_RUSTUP_BIN:/usr/bin:/bin" \
    CALL_LOG="$norustup_log" \
    CARGO_TARGET_DIR="$TMP/no-rustup-target" \
    scripts/build-swift-xcframework.sh --macos-only 2>&1
); then
  echo 'the builder continued with no toolchain manager present' >&2
  exit 1
fi
grep -Fq 'rustup is required' <<< "$missing_rustup"
grep -Fq 'aarch64-apple-darwin' <<< "$missing_rustup"
! grep -q '^cargo ' "$norustup_log"
echo 'ok - an unavailable target install refuses by name instead of failing inside Cargo'

RUSTUP_INSTALLED_TARGETS=
RUSTUP_ADD_FAILS=0

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
