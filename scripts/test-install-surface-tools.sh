#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin" "$TMP/program/scripts" "$TMP/program/tools"

# The installer reads the toolchain definition that sits next to it and takes
# no path from its caller (#1186), so exercising it against a fixture
# definition means giving it a fixture program directory -- which is also
# exactly the shape CI runs it in: a scratch copy extracted from the base.
cp "$SCRIPT_DIR/install-surface-tools.sh" "$TMP/program/scripts/"
chmod +x "$TMP/program/scripts/install-surface-tools.sh"
INSTALL="$TMP/program/scripts/install-surface-tools.sh"

archive='fake cargo-public-api archive'
printf '%s\n' "$archive" > "$TMP/archive"
if command -v sha256sum >/dev/null; then
  checksum=$(sha256sum "$TMP/archive" | awk '{print $1}')
else
  checksum=$(shasum -a 256 "$TMP/archive" | awk '{print $1}')
fi
cat > "$TMP/program/tools/surface-toolchain.env" <<ENV
CARGO_PUBLIC_API_VERSION=9.9.9
CARGO_PUBLIC_API_CRATE_SHA256=$checksum
SURFACE_RUST_TOOLCHAIN=nightly-test
UNIFFI_VERSION=0.0.0
ENV

cat > "$TMP/bin/rustup" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'rustup %s\n' "$*" >> "$CALL_LOG"
SHIM
cat > "$TMP/bin/cargo-public-api" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
if [[ ${VERSION_MODE:-correct} == wrong ]]; then
  echo 'cargo-public-api 0.0.1'
else
  echo 'cargo-public-api 9.9.9'
fi
SHIM
cat > "$TMP/bin/cargo" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
printf 'cargo %s\n' "$*" >> "$CALL_LOG"
mkdir -p "$CARGO_HOME/registry/cache/fake-index"
printf 'fake cargo-public-api archive\n' > \
  "$CARGO_HOME/registry/cache/fake-index/cargo-public-api-9.9.9.crate"
SHIM
chmod +x "$TMP/bin/"*

run_installer() {
  PATH="$TMP/bin:$PATH" \
  CALL_LOG="$1" \
  CARGO_HOME="$2" \
  VERSION_MODE="$3" \
  "$INSTALL"
}

# Wrong/missing binary: exact toolchain and --locked are mandatory.
normal_log="$TMP/normal.log"
run_installer "$normal_log" "$TMP/cargo-normal" wrong
grep -Fxq 'rustup toolchain install nightly-test --profile minimal' "$normal_log"
grep -Fxq 'cargo +nightly-test install --locked cargo-public-api@9.9.9' "$normal_log"
echo "ok - installer pins toolchain and locked package"

# Correct binary but pruned registry cache: refetch with the same exact
# toolchain/locked package, then verify the recovered archive checksum.
recovery_log="$TMP/recovery.log"
run_installer "$recovery_log" "$TMP/cargo-recovery" correct
grep -Fxq 'cargo +nightly-test install --locked --force cargo-public-api@9.9.9' "$recovery_log"
echo "ok - missing registry cache recovers through pinned locked install"

# The installer takes nothing from its caller, so it must behave identically
# from any working directory -- including one that is not a Git worktree, which
# is where CI runs it (#1186). Before this, resolving the toolchain definition
# through `git rev-parse --show-toplevel` made that exact case die with git's
# raw exit 128.
mkdir -p "$TMP/not-a-worktree"
cwd_log="$TMP/cwd.log"
(cd "$TMP/not-a-worktree" && run_installer "$cwd_log" "$TMP/cargo-cwd" wrong)
grep -Fxq 'rustup toolchain install nightly-test --profile minimal' "$cwd_log"
echo "ok - the installer does not depend on the directory it runs in"

# Nothing the installer does is a statement about a proposed head, so every way
# it fails is a gate malfunction: exit 70, under its own prefix, never a raw
# status a reporter cannot classify (#1264, #1170).
expect_malfunction() {
  local label=$1 reason=$2
  shift 2
  local output status=0
  output=$("$@" 2>&1) || status=$?
  if (( status != 70 )); then
    echo "FAIL: $label exited $status; a malfunction is exit 70" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
  if ! printf '%s\n' "$output" | grep -Fq "surface-tools-malfunction: $reason"; then
    echo "FAIL: $label did not name the malfunction: $reason" >&2
    printf '%s\n' "$output" >&2
    exit 1
  fi
  echo "ok - $label"
}

# A program directory with no toolchain definition beside it.
mkdir -p "$TMP/no-definition/scripts"
cp "$SCRIPT_DIR/install-surface-tools.sh" "$TMP/no-definition/scripts/"
chmod +x "$TMP/no-definition/scripts/install-surface-tools.sh"
expect_malfunction "a missing toolchain definition is a malfunction" \
  "this program has no toolchain definition" \
  env PATH="$TMP/bin:$PATH" CALL_LOG="$TMP/nodef.log" \
    CARGO_HOME="$TMP/cargo-nodef" VERSION_MODE=wrong \
    "$TMP/no-definition/scripts/install-surface-tools.sh"

# An unplanned failure inside the installer -- here the pinned toolchain
# install itself -- is routed the same way rather than escaping as whatever
# status the failing program chose.
cat > "$TMP/bin/rustup" <<'SHIM'
#!/usr/bin/env bash
exit 128
SHIM
chmod +x "$TMP/bin/rustup"
expect_malfunction "an unplanned failure inside the installer is a malfunction" \
  "the surface tool install did not complete" \
  env PATH="$TMP/bin:$PATH" CALL_LOG="$TMP/broken.log" \
    CARGO_HOME="$TMP/cargo-broken" VERSION_MODE=wrong "$INSTALL"
