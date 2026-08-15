#!/usr/bin/env bash
set -euo pipefail

# Declared before any external command runs. `nak` is the one that matters:
# only the first phase runs against the fake `nak` this script installs, and
# the seed-readback phase drives the real harness, which needs the real
# binary. Without this the script gets several seconds into a passing run
# before failing, and passes outright on a developer machine that happens to
# have `nak` while failing on a runner that does not.
SCRIPT_PATH=${BASH_SOURCE[0]}
SCRIPT_DIR=${SCRIPT_PATH%/*}
[[ $SCRIPT_DIR != "$SCRIPT_PATH" ]] || SCRIPT_DIR=.
source "$SCRIPT_DIR/lib/require-commands.sh" || exit 2
require_commands git jq mktemp nak || exit 2

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

# --- the seed readback refuses a fixture that did not seed what it claims ----
#
# `write_seed_summary` reads what the relays actually serve, so it is the one
# place a mis-seed can still be caught: a relay can answer a publish with
# `OK: false` without failing nak, and a shared event missing from one relay
# then looks exactly like NMP losing a source. Driven against hand-written
# snapshots, so this needs no relay, no Docker and no network.
#
# The harness runs `main` on load, and `--help` is its one argument that
# returns without doing anything, which is what makes the function reachable
# here without inventing a command for the test's benefit.
# shellcheck source=/dev/null
source "$HARNESS" --help >/dev/null

SEED_RUN="$TEMP_ROOT/seed-run"
mkdir -p "$SEED_RUN/seed"

event_line() {
    printf '{"id":"%s","kind":%s,"tags":[["h","%s"]]}\n' "$1" "$2" "$3"
}

# 14 kind 9 in "bitcoin" per relay, exactly one of them shared, plus one shared
# kind 30023 each side -- the same shape the real seed produces, and the shape
# the consumers assert on.
write_snapshots() {
    local omit=${1:-}
    local suffix index
    for suffix in a b; do
        : > "$SEED_RUN/seed/relay-$suffix.jsonl"
        if [[ $omit != "kind9-b" || $suffix != b ]]; then
            event_line shared-kind-9 9 bitcoin >> "$SEED_RUN/seed/relay-$suffix.jsonl"
        fi
        if [[ $omit != "kind30023-b" || $suffix != b ]]; then
            event_line shared-kind-30023 30023 bitcoin >> "$SEED_RUN/seed/relay-$suffix.jsonl"
        fi
        event_line "chat-$suffix" 9 bitcoin >> "$SEED_RUN/seed/relay-$suffix.jsonl"
        for ((index = 0; index < 12; index += 1)); do
            event_line "stress-$suffix-$index" 9 bitcoin \
                >> "$SEED_RUN/seed/relay-$suffix.jsonl"
        done
    done
}

write_snapshots
write_seed_summary "$SEED_RUN" \
    || fail "a correctly seeded readback must be accepted"
[[ $(jq '.group_bitcoin_kind_9.distinct' "$SEED_RUN/seed/summary.json") == 27 ]] \
    || fail "a correctly seeded readback must record 27 distinct kind 9 rows"

for omission in kind9-b kind30023-b; do
    write_snapshots "$omission"
    if refusal=$(write_seed_summary "$SEED_RUN" 2>&1); then
        fail "a readback missing the shared $omission seed was accepted"
    fi
    case "$omission" in
        kind9-b) missing_class='kind 9' ;;
        kind30023-b) missing_class='kind 30023' ;;
    esac
    [[ $refusal == *"$missing_class events present at both relays"* ]] \
        || fail "the refusal for $omission did not name the missing seed class"
done

printf '%s\n' \
    'NIP-29 consumer harness test: seed readback verification passed'
