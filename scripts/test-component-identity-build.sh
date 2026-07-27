#!/usr/bin/env bash
# #952 release-boundary falsifier. A native component release must not mint an
# identity outside the isolated supported builders, because only those builders
# fix the Cargo roots and target directory before the build starts.

set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
cd "$ROOT"

TMP=$(mktemp -d "${TMPDIR:-/tmp}/nmp-component-build.XXXXXX")
CORE_ARTIFACT_DIR=
PAIR_ARTIFACT_DIR=
LOCK_HOLDER_PID=
cleanup() {
  if [[ -n "$LOCK_HOLDER_PID" ]]; then
    printf '%s\n' release >"$TMP/lock-release" 2>/dev/null || true
    kill "$LOCK_HOLDER_PID" 2>/dev/null || true
    wait "$LOCK_HOLDER_PID" 2>/dev/null || true
  fi
  rm -r "$TMP"
  for directory in "$CORE_ARTIFACT_DIR" "$PAIR_ARTIFACT_DIR"; do
    if [[ -n "$directory" && -d "$directory" ]]; then
      chmod -R u+w "$directory" 2>/dev/null || true
      rm -r "$directory"
    fi
  done
}
trap cleanup EXIT

TARGET_DIR_VALUE=${CARGO_TARGET_DIR:-target}
if [[ "$TARGET_DIR_VALUE" == /* ]]; then
  BASE_TARGET_DIR=$TARGET_DIR_VALUE
else
  BASE_TARGET_DIR="$ROOT/$TARGET_DIR_VALUE"
fi
HOST_TARGET=$(rustc -vV | sed -n 's/^host: //p')
[[ -n "$HOST_TARGET" ]] || {
  echo "component-identity-build: rustc did not report a host target" >&2
  exit 1
}
cargo fetch --locked
CORE_TARGET_DIR="$BASE_TARGET_DIR/nmp-component-build/core"
PAIR_TARGET_DIR="$BASE_TARGET_DIR/nmp-component-build/nip46"
mkdir -p "$CORE_TARGET_DIR"
printf '%s\n' stale-file-with-no-live-kernel-lock > "$CORE_TARGET_DIR/.builder-lock"
CORE_ARTIFACT_DIR=$(
  scripts/build-component-release.sh "$BASE_TARGET_DIR" "nmp-ffi" "$HOST_TARGET"
)
PAIR_ARTIFACT_DIR=$(
  scripts/build-component-release.sh \
    "$BASE_TARGET_DIR" "nmp-ffi nmp-nip46-ffi" "$HOST_TARGET"
)

assert_unmanaged_refused() {
  local label=$1
  local target_dir=$2
  shift 2
  local output="$TMP/$label-output"

  if env -u NMP_FFI_COMPONENT_AUTH \
    CARGO_TARGET_DIR="$target_dir" \
    NMP_FFI_COMPONENT_ROOT="$target_dir" \
    "$@" >"$output" 2>&1; then
    echo "component-identity-build: unmanaged $label build unexpectedly succeeded" >&2
    exit 1
  fi

  grep -qF \
    "release component target has no live builder authorization" \
    "$output" || {
      cat "$output" >&2
      echo "component-identity-build: $label failed for the wrong reason" >&2
      exit 1
    }
}

assert_unmanaged_refused core "$CORE_TARGET_DIR" \
  cargo build --locked -p nmp-ffi --release --target "$HOST_TARGET"
assert_unmanaged_refused pair "$PAIR_TARGET_DIR" \
  cargo build --locked -p nmp-ffi -p nmp-nip46-ffi --release --target "$HOST_TARGET"
assert_unmanaged_refused workspace "$PAIR_TARGET_DIR" \
  cargo build --locked --workspace --release --target "$HOST_TARGET"
assert_unmanaged_refused all-targets "$CORE_TARGET_DIR" \
  cargo build --locked -p nmp-ffi --all-targets --release --target "$HOST_TARGET"
assert_unmanaged_refused test "$CORE_TARGET_DIR" \
  cargo test --locked -p nmp-ffi --release --target "$HOST_TARGET" --no-run
assert_unmanaged_refused clippy "$CORE_TARGET_DIR" \
  cargo clippy --locked -p nmp-ffi --release --target "$HOST_TARGET" --no-deps
assert_unmanaged_refused bench "$CORE_TARGET_DIR" \
  cargo build --locked -p nmp-ffi --profile bench --target "$HOST_TARGET"

for artifact_dir in "$CORE_ARTIFACT_DIR" "$PAIR_ARTIFACT_DIR"; do
  [[ -d "$artifact_dir" ]] || {
    echo "component-identity-build: managed builder returned no sealed artifact snapshot: $artifact_dir" >&2
    exit 1
  }
done

mkfifo "$TMP/lock-ready" "$TMP/lock-release"
perl -MFcntl=:flock -e '
  my ($lock_file, $ready_pipe, $release_pipe) = @ARGV;
  open(my $lock, ">>", $lock_file) or die "open $lock_file: $!\n";
  flock($lock, LOCK_EX) or die "lock $lock_file: $!\n";
  open(my $ready, ">", $ready_pipe) or die "open $ready_pipe: $!\n";
  print {$ready} "ready\n";
  close($ready);
  open(my $release, "<", $release_pipe) or die "open $release_pipe: $!\n";
  <$release>;
' "$CORE_TARGET_DIR/.builder-lock" "$TMP/lock-ready" "$TMP/lock-release" &
LOCK_HOLDER_PID=$!
IFS= read -r lock_ready <"$TMP/lock-ready"
[[ "$lock_ready" == ready ]] || {
  echo "component-identity-build: lock-holder process did not report readiness" >&2
  exit 1
}
if scripts/build-component-release.sh \
  "$BASE_TARGET_DIR" "nmp-ffi" "$HOST_TARGET" >"$TMP/concurrent-output" 2>&1
then
  echo "component-identity-build: concurrent managed build unexpectedly succeeded" >&2
  exit 1
fi
printf '%s\n' release >"$TMP/lock-release"
wait "$LOCK_HOLDER_PID" 2>/dev/null || true
LOCK_HOLDER_PID=
grep -qF \
  "another supported core build is already using $CORE_TARGET_DIR" \
  "$TMP/concurrent-output" || {
    cat "$TMP/concurrent-output" >&2
    echo "component-identity-build: concurrent build refusal was not specific" >&2
    exit 1
  }

echo "component-identity-build: every post-build authorization was revoked and sealed snapshots survived the refusal matrix"
