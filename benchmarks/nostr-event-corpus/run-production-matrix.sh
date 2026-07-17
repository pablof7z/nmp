#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <relay-ingest-bench-binary> <shape-corpus> <output-directory>" >&2
  exit 2
fi

binary=$1
shape_corpus=$2
output_directory=$3
mkdir -p "$output_directory"

common=(
  --events 100000
  --queue-capacity 8192
  --verified-cache-capacity 131072
  --verifier-workers 8
  --verify-batch-size 512
  --engine-batch-size 4096
  --engine-batch-bytes 8388608
  --timeout-secs 240
)

for repetition in 1 2 3; do
  "$binary" "${common[@]}" \
    --payload-bytes 128 \
    --output "$output_directory/uniform-$repetition.json" \
    >"$output_directory/uniform-$repetition.stdout"
  "$binary" "${common[@]}" \
    --shape-corpus "$shape_corpus" \
    --output "$output_directory/representative-$repetition.json" \
    >"$output_directory/representative-$repetition.stdout"
done
