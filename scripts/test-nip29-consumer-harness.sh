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
printf '%s\n' '{"identities":{"owner_b":"fixture-owner"}}' > "$RUN_DIR/manifest.json"

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

cat > "$FAKE_BIN/ps" <<'SH'
#!/usr/bin/env bash
count=0
if [[ -f "$NMP_TEST_PS_COUNTER" ]]; then
    IFS= read -r count < "$NMP_TEST_PS_COUNTER"
fi
count=$((count + 1))
printf '%s\n' "$count" > "$NMP_TEST_PS_COUNTER"
case "${NMP_TEST_PS_MODE:-}" in
    miss-first)
        ((count == 1)) && exit 1
        ;;
    miss-second)
        ((count == 2)) && exit 1
        ;;
esac
exec /bin/ps "$@"
SH
chmod +x "$FAKE_BIN/ps"

cat > "$FAKE_BIN/curl" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod +x "$FAKE_BIN/curl"

nohup "$RUN_DIR/bin/croissant" \
    > "$RUN_DIR/witness/relay-b.log" 2>&1 </dev/null &
relay_pid=$!
printf '%s\n' "$relay_pid" > "$RUN_DIR/relay-b.pid"

PS_COUNTER="$TEMP_ROOT/stop-ps-counter"
PATH="$FAKE_BIN:$PATH" NMP_NIP29_HARNESS_BACKEND=host \
    NMP_TEST_PS_COUNTER="$PS_COUNTER" NMP_TEST_PS_MODE=miss-second \
    "$HARNESS" relay-down b "$RUN_DIR" >/dev/null

if kill -0 "$relay_pid" 2>/dev/null; then
    fail "relay-down returned before the separately-owned process exited"
fi
[[ ! -e "$RUN_DIR/relay-b.pid" ]] \
    || fail "relay-down retained the PID file after confirmed exit"

relay_pid=

jq '.relays.b = {port: 19889, http: "http://127.0.0.1:19889"}' \
    "$RUN_DIR/state.json" > "$RUN_DIR/state.json.next"
mv "$RUN_DIR/state.json.next" "$RUN_DIR/state.json"
PS_COUNTER="$TEMP_ROOT/start-ps-counter"
PATH="$FAKE_BIN:$PATH" NMP_NIP29_HARNESS_BACKEND=host \
    NMP_TEST_PS_COUNTER="$PS_COUNTER" NMP_TEST_PS_MODE=miss-first \
    "$HARNESS" relay-up b "$RUN_DIR" >/dev/null

IFS= read -r relay_pid < "$RUN_DIR/relay-b.pid"
kill -0 "$relay_pid" 2>/dev/null \
    || fail "relay-up did not retain the separately-owned process"

printf '%s\n' \
    'NIP-29 consumer harness test: cross-shell restart lifecycle passed'
