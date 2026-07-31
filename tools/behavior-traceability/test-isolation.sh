#!/usr/bin/env bash
set -euo pipefail

[[ $# -eq 1 ]] || {
  echo "usage: $0 <rust-toolchain>" >&2
  exit 2
}

TOOLCHAIN=$1
TOOL_DIR=$(cd "$(dirname "$0")" && pwd)
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

REPO="$TMP/untrusted-repository"
FIXTURE_TOOL="$REPO/tools/behavior-traceability"
MANIFEST="$FIXTURE_TOOL/Cargo.toml"
NEUTRAL="$TMP/neutral-working-directory"
CARGO_HOME_DIR=${TRACE_CARGO_HOME:-$TMP/cargo-home}
TARGET_DIR="$TMP/target"
MARKER="$TMP/untrusted-runner-executed"
RUNNER="$TMP/untrusted-runner.sh"

mkdir -p "$FIXTURE_TOOL/src" "$REPO/.cargo" "$NEUTRAL" "$CARGO_HOME_DIR"
cp "$TOOL_DIR/Cargo.toml" "$TOOL_DIR/Cargo.lock" "$FIXTURE_TOOL/"
cp "$TOOL_DIR"/src/*.rs "$FIXTURE_TOOL/src/"

mkdir "$FIXTURE_TOOL/src/bin"
cat > "$FIXTURE_TOOL/src/bin/untrusted.rs" <<'EOF'
compile_error!("an automatically discovered binary entered the trusted checker graph");
EOF

cat > "$RUNNER" <<EOF
#!/usr/bin/env bash
set -euo pipefail
touch "$MARKER"
exec "\$@"
EOF
chmod +x "$RUNNER"
cat > "$REPO/.cargo/config.toml" <<EOF
[target.'cfg(unix)']
runner = ["$RUNNER"]
EOF

run_cargo() {
  (
    cd "$1"
    shift
    CARGO_HOME="$CARGO_HOME_DIR" \
    CARGO_TARGET_DIR="$TARGET_DIR" \
      cargo "+$TOOLCHAIN" "$@"
  )
}

# The explicit manifest disables every automatic Cargo target. An added source
# below src/bin must therefore stay outside the executable checker graph.
run_cargo "$NEUTRAL" check --locked --all-targets --manifest-path "$MANIFEST"
sed -i.bak 's/autobins = false/autobins = true/' "$MANIFEST"
if run_cargo "$NEUTRAL" check --locked --all-targets --manifest-path "$MANIFEST" \
  >"$TMP/auto-target.out" 2>&1; then
  echo "isolation falsifier: enabling automatic binaries unexpectedly passed" >&2
  exit 1
fi
grep -Fq "automatically discovered binary entered" "$TMP/auto-target.out" || {
  cat "$TMP/auto-target.out" >&2
  echo "isolation falsifier: auto-target mutation failed for the wrong reason" >&2
  exit 1
}
mv "$MANIFEST.bak" "$MANIFEST"

# Prove the fixture's repository Cargo runner is live when Cargo starts inside
# that repository, then prove the exact detached-manifest invocation from a
# neutral directory cannot discover it.
run_cargo "$REPO" run --locked --quiet --manifest-path "$MANIFEST" -- --help \
  >"$TMP/repository-cwd.out" 2>&1 || true
[[ -f "$MARKER" ]] || {
  echo "isolation falsifier: malicious repository runner was not a live mutation" >&2
  exit 1
}
rm "$MARKER"
run_cargo "$NEUTRAL" run --locked --quiet --manifest-path "$MANIFEST" -- --help \
  >"$TMP/neutral-cwd.out" 2>&1 || true
[[ ! -e "$MARKER" ]] || {
  echo "isolation falsifier: detached invocation executed repository Cargo config" >&2
  exit 1
}

# The binary itself fails closed if a future workflow accidentally scopes a
# GitHub token into the head-built checker step.
printf 'nmp-behavior-issue-snapshot-v1\n' > "$TMP/issues.tsv"
if (
  cd "$NEUTRAL"
  GH_TOKEN=must-not-reach-head-code \
  CARGO_HOME="$CARGO_HOME_DIR" \
  CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo "+$TOOLCHAIN" run --locked --quiet --manifest-path "$MANIFEST" -- \
      check --root "$REPO" --base HEAD --head HEAD --issues "$TMP/issues.tsv"
) >"$TMP/token.out" 2>&1; then
  echo "isolation falsifier: head checker accepted a GitHub token" >&2
  exit 1
fi
grep -Fq "refuses GitHub token exposure" "$TMP/token.out" || {
  cat "$TMP/token.out" >&2
  echo "isolation falsifier: token rejection failed for the wrong reason" >&2
  exit 1
}

echo "behavior-traceability isolation: auto-target, repository config, and token probes passed"
