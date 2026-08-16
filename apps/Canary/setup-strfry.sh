#!/usr/bin/env bash
# Builds a real strfry relay binary for the Canary lab, pinned to an exact
# commit for reproducibility. Not vendored in git -- see the investigation
# report's rationale (a committed macOS-arm64-only 20MB blob is dead
# weight forever in git history; this script reproduces the identical
# binary on demand instead).
#
# HONESTY NOTE: on the machine this was developed on, `lmdb`, `secp256k1`,
# `flatbuffers`, `libuv`, `zstd`, `libtool`, and `zlib` were ALREADY
# installed via Homebrew, left over from an unrelated sibling project. The
# `brew install` line below was therefore never actually timed end-to-end
# in this session. A genuinely clean machine WILL pay that cost --
# several minutes and real disk beyond strfry's own ~250MB -- budget for
# it; do not assume this script is as fast on a fresh checkout as it was
# here.
set -euo pipefail

STRFRY_COMMIT="ca48c518d9aabf6912cbcb41f3d810cde7e0acb7"
CACHE_DIR="${RELAY_LAB_CACHE_DIR:-$HOME/Library/Caches/nmp-canary-relay-lab}"
STRFRY_DIR="$CACHE_DIR/strfry"

if [[ -x "$STRFRY_DIR/strfry" ]]; then
    installed_commit="$(git -C "$STRFRY_DIR" rev-parse HEAD 2>/dev/null || echo "unknown")"
    if [[ "$installed_commit" == "$STRFRY_COMMIT" ]]; then
        echo "strfry already built at pinned commit ($STRFRY_COMMIT): $STRFRY_DIR/strfry"
        exit 0
    fi
    echo "cached strfry is at $installed_commit, not the pinned $STRFRY_COMMIT -- rebuilding"
    rm -rf "$STRFRY_DIR"
fi

echo "==> Homebrew dependencies (skips anything already installed)"
brew install pkg-config libtool openssl zlib lmdb flatbuffers secp256k1 zstd libuv perl

mkdir -p "$CACHE_DIR"
echo "==> cloning strfry, pinned to $STRFRY_COMMIT"
git clone https://github.com/hoytech/strfry.git "$STRFRY_DIR"
git -C "$STRFRY_DIR" checkout "$STRFRY_COMMIT"

echo "==> fetching submodules (all pinned by strfry's own .gitmodules refs at this commit)"
(cd "$STRFRY_DIR" && git submodule update --init --depth 1)
(cd "$STRFRY_DIR" && make setup-golpe)

echo "==> building (uses Homebrew-installed LMDB/secp256k1/flatbuffers/libuv/zstd/openssl/zlib -- no cargo, no Rust)"
(cd "$STRFRY_DIR" && make -j"$(sysctl -n hw.ncpu)")

echo "==> built: $STRFRY_DIR/strfry"
"$STRFRY_DIR/strfry" --version || true
