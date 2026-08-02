#!/usr/bin/env bash
set -euo pipefail

ROOT=$(git rev-parse --show-toplevel)
HARNESS="$ROOT/tools/nip29-consumer-harness/harness.sh"
TEMP_ROOT=$(mktemp -d)
RUN_DIR="$TEMP_ROOT/run"
FAKE_BIN="$TEMP_ROOT/bin"
relay_pid=

cleanup() {
    if [[ -n "$relay_pid" ]]; then
        kill "$relay_pid" 2>/dev/null || true
        wait "$relay_pid" 2>/dev/null || true
    fi
    rm -r "$TEMP_ROOT"
}
trap cleanup EXIT

fail() {
    printf 'NIP-29 consumer harness test: %s\n' "$*" >&2
    exit 1
}

mkdir -p "$RUN_DIR/bin" "$RUN_DIR/witness" "$FAKE_BIN"
: > "$RUN_DIR/.nmp-nip29-consumer-harness"
printf '%s\n' '{"backend":"host"}' > "$RUN_DIR/state.json"

cat > "$RUN_DIR/bin/croissant" <<'SH'
#!/usr/bin/env bash
trap 'sleep 1; exit 0' TERM
while true; do
    sleep 1
done
SH
chmod +x "$RUN_DIR/bin/croissant"

# The lifecycle command only checks that nak exists, so this inert fixture keeps
# the test independent of the seeding tool.
cat > "$FAKE_BIN/nak" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod +x "$FAKE_BIN/nak"

nohup "$RUN_DIR/bin/croissant" \
    > "$RUN_DIR/witness/relay-b.log" 2>&1 </dev/null &
relay_pid=$!
printf '%s\n' "$relay_pid" > "$RUN_DIR/relay-b.pid"

PATH="$FAKE_BIN:$PATH" NMP_NIP29_HARNESS_BACKEND=host \
    "$HARNESS" relay-down b "$RUN_DIR" >/dev/null

if kill -0 "$relay_pid" 2>/dev/null; then
    fail "relay-down returned before the separately-owned process exited"
fi
[[ ! -e "$RUN_DIR/relay-b.pid" ]] \
    || fail "relay-down retained the PID file after confirmed exit"

relay_pid=
printf '%s\n' \
    'NIP-29 consumer harness test: cross-shell host shutdown passed'
