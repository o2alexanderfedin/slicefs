#!/usr/bin/env bash
set -euo pipefail

MOUNT_DIR="${1:?Usage: $0 <mount-dir> [results-dir]}"
RESULTS_DIR="${2:-./results}"

mkdir -p "$RESULTS_DIR"
DATE=$(date +%Y%m%d-%H%M%S)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

for job in "$SCRIPT_DIR"/*.fio; do
    name=$(basename "$job" .fio)
    echo "Running: $name"
    fio "$job" --directory="$MOUNT_DIR" \
        --output-format=json \
        --output="$RESULTS_DIR/${name}_${DATE}.json"
    echo "  -> $RESULTS_DIR/${name}_${DATE}.json"
done

echo "All benchmarks complete. Results in $RESULTS_DIR/"
