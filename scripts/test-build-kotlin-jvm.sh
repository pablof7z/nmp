#!/usr/bin/env bash
set -euo pipefail

SCRIPT=$(cd "$(dirname "$0")" && pwd)/build-kotlin-jvm.sh
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

REPO="$TMP/repo"
BIN="$TMP/bin"
mkdir -p "$REPO/scripts" "$BIN"
REPO=$(cd "$REPO" && pwd -P)
BIN=$(cd "$BIN" && pwd -P)
cp "$SCRIPT" "$REPO/scripts/"
git -C "$REPO" init -q

cat > "$BIN/uname" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
case "${1:-}" in
  -s) printf '%s\n' Linux ;;
  -m) printf '%s\n' x86_64 ;;
  *) exit 64 ;;
esac
SHIM

cat > "$BIN/cargo" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'cargo' >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf '\n' >> "$CALL_LOG"

target_dir=${CARGO_TARGET_DIR:-target}
if [[ $target_dir != /* ]]; then
  target_dir="$PWD/$target_dir"
fi

case "${1:-}" in
  build)
    mkdir -p "$target_dir/release"
    if [[ ${FAKE_OMIT_LIBRARY:-0} != 1 ]]; then
      printf '%s\n' "$target_dir" > "$target_dir/release/libnmp_ffi.so"
    fi
    if [[ ${FAKE_OMIT_BINDGEN:-0} != 1 ]]; then
      cat > "$target_dir/release/uniffi-bindgen" <<'BINDGEN'
#!/usr/bin/env bash
set -euo pipefail
printf 'bindgen %q' "$0" >> "$CALL_LOG"
printf ' %q' "$@" >> "$CALL_LOG"
printf '\n' >> "$CALL_LOG"
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
mkdir -p "$out_dir/uniffi/nmp_ffi"
printf 'generated from %s\n' "$library" > "$out_dir/uniffi/nmp_ffi/nmp_ffi.kt"
BINDGEN
      chmod +x "$target_dir/release/uniffi-bindgen"
    fi
    ;;
  *) exit 64 ;;
esac
SHIM
chmod +x "$BIN/uname" "$BIN/cargo"

run_script() {
  local log=$1 target_dir=${2:-}
  : > "$log"
  if [[ -n $target_dir ]]; then
    (
      cd "$REPO"
      PATH="$BIN:$PATH" \
        CALL_LOG="$log" \
        CARGO_TARGET_DIR="$target_dir" \
        scripts/build-kotlin-jvm.sh >/dev/null
    )
  else
    (
      cd "$REPO"
      PATH="$BIN:$PATH" \
        CALL_LOG="$log" \
        env -u CARGO_TARGET_DIR scripts/build-kotlin-jvm.sh >/dev/null
    )
  fi
}

run_failure() {
  local log=$1 output=$2 target_dir=$3 omission=$4
  : > "$log"
  (
    cd "$REPO"
    PATH="$BIN:$PATH" \
      CALL_LOG="$log" \
      CARGO_TARGET_DIR="$target_dir" \
      env "$omission=1" scripts/build-kotlin-jvm.sh >"$output" 2>&1
  )
}

assert_single_plan() {
  local log=$1
  [[ $(grep -c '^cargo build ' "$log") -eq 1 ]]
  [[ $(grep -c '^bindgen ' "$log") -eq 1 ]]
  ! grep -q '^cargo run ' "$log"
}

assert_contains() {
  local expected=$1 file=$2
  grep -Fq -- "$expected" "$file" || {
    echo "missing expected text: $expected" >&2
    cat "$file" >&2
    exit 1
  }
}

assert_outputs() {
  local target_dir=$1
  local generated="$REPO/Packages/NMPKotlin/src/main/kotlin/uniffi/nmp_ffi/nmp_ffi.kt"
  local resource="$REPO/Packages/NMPKotlin/src/main/resources/linux-x86-64/libnmp_ffi.so"
  assert_contains "generated from $target_dir/release/libnmp_ffi.so" "$generated"
  assert_contains "$target_dir" "$resource"
}

# With no override, preserve the historical repository-local target directory.
default_log="$TMP/default.log"
run_script "$default_log"
assert_single_plan "$default_log"
assert_contains "$REPO/target/release/libnmp_ffi.so" "$default_log"
assert_contains "$REPO/target/release/uniffi-bindgen" "$default_log"
assert_outputs "$REPO/target"
echo 'ok - default target directory remains repository-local'

# An absolute shared cache supplies both the release library and bindgen's
# Cargo target. No lookup or fallback build may touch the repository target.
absolute_log="$TMP/absolute.log"
absolute_target="$TMP/shared-cache"
rm -rf "$REPO/target" "$REPO/Packages" "$REPO/gen-kotlin"
run_script "$absolute_log" "$absolute_target"
assert_single_plan "$absolute_log"
assert_contains "$absolute_target/release/libnmp_ffi.so" "$absolute_log"
assert_contains "$absolute_target/release/uniffi-bindgen" "$absolute_log"
! grep -Fq "$REPO/target/release" "$absolute_log"
[[ ! -e $REPO/target ]]
assert_outputs "$absolute_target"
echo 'ok - absolute CARGO_TARGET_DIR is used without fallback or duplicate build'

# Cargo resolves a relative override from the repository root because that is
# the working directory for both invocations. Artifact lookup must match.
relative_log="$TMP/relative.log"
relative_value=relative-cache
relative_target="$REPO/$relative_value"
rm -rf "$REPO/target" "$REPO/Packages" "$REPO/gen-kotlin" "$relative_target"
run_script "$relative_log" "$relative_value"
assert_single_plan "$relative_log"
assert_contains "$relative_target/release/libnmp_ffi.so" "$relative_log"
assert_contains "$relative_target/release/uniffi-bindgen" "$relative_log"
! grep -Fq "$REPO/target/release" "$relative_log"
[[ ! -e $REPO/target ]]
assert_outputs "$relative_target"
echo 'ok - relative CARGO_TARGET_DIR lookup matches Cargo resolution'

# A successful Cargo exit without either required release artifact must fail
# before generation and name the exact resolved path that is missing.
missing_library_log="$TMP/missing-library.log"
missing_library_output="$TMP/missing-library.out"
missing_library_target="$TMP/missing-library-target"
if run_failure \
  "$missing_library_log" "$missing_library_output" \
  "$missing_library_target" FAKE_OMIT_LIBRARY; then
  echo 'missing release library unexpectedly passed' >&2
  exit 1
fi
assert_contains \
  "error: expected $missing_library_target/release/libnmp_ffi.so after cargo build" \
  "$missing_library_output"
[[ $(grep -c '^cargo build ' "$missing_library_log") -eq 1 ]]
! grep -q '^bindgen ' "$missing_library_log"

missing_bindgen_log="$TMP/missing-bindgen.log"
missing_bindgen_output="$TMP/missing-bindgen.out"
missing_bindgen_target="$TMP/missing-bindgen-target"
if run_failure \
  "$missing_bindgen_log" "$missing_bindgen_output" \
  "$missing_bindgen_target" FAKE_OMIT_BINDGEN; then
  echo 'missing release bindgen unexpectedly passed' >&2
  exit 1
fi
assert_contains \
  "error: expected executable $missing_bindgen_target/release/uniffi-bindgen after cargo build" \
  "$missing_bindgen_output"
[[ $(grep -c '^cargo build ' "$missing_bindgen_log") -eq 1 ]]
! grep -q '^bindgen ' "$missing_bindgen_log"
echo 'ok - missing release artifacts fail early with resolved-path diagnostics'
