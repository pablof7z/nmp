#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <release-binary> <capture-directory>" >&2
  exit 2
fi

binary=$1
capture_directory=$2
first_window=1783641600  # 2026-07-10T00:00:00Z
end_exclusive=1784246400 # 2026-07-17T00:00:00Z
stride_seconds=1200     # one fixed two-second interval every twenty UTC minutes
window_seconds=2        # stays below observed relay response caps (100/500)
per_window_limit=5000

mkdir -p "$capture_directory"

capture() {
  "$binary" capture-relay \
    "$1" "$2" \
    "$first_window" "$end_exclusive" \
    "$stride_seconds" "$window_seconds" "$per_window_limit" \
    "$capture_directory"
}

capture damus wss://relay.damus.io &
damus_pid=$!
capture nos-lol wss://nos.lol &
nos_lol_pid=$!
capture primal wss://relay.primal.net &
primal_pid=$!
capture nostr-mom wss://nostr.mom &
nostr_mom_pid=$!
capture offchain wss://offchain.pub &
offchain_pid=$!
capture wirednet-jp wss://relay.nostr.wirednet.jp &
wirednet_pid=$!

wait "$damus_pid"
wait "$nos_lol_pid"
wait "$primal_pid"
wait "$nostr_mom_pid"
wait "$offchain_pid"
wait "$wirednet_pid"
