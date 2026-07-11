#!/usr/bin/env bash
set -euo pipefail

INSTALL=$(cd "$(dirname "$0")" && pwd)/install-surface-tools.sh
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/bin" "$TMP/root"

archive='fake cargo-public-api archive'
printf '%s\n' "$archive" > "$TMP/archive"
if command -v sha256sum >/dev/null; then
  checksum=$(sha256sum "$TMP/archive" | awk '{print $1}')
else
  checksum=$(shasum -a 256 "$TMP/archive" | awk '{print $1}')
fi
cat > "$TMP/toolchain.env" <<ENV
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
  SURFACE_ROOT="$TMP/root" \
  SURFACE_TOOLCHAIN_ENV="$TMP/toolchain.env" \
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
