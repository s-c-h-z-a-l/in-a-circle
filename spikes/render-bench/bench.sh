#!/usr/bin/env bash
# Runs every backend/mode combination N times and collects the `M ...` lines.
# Usage: ./bench.sh [runs] > results.txt
set -euo pipefail
cd "$(dirname "$0")"
RUNS=${1:-3}
BIN=target/release

cargo build -q --release

vram_of() {
    nvidia-smi | awk -v p="$1" '$5 == p { print $(NF-1) }'
}

run() {
    local bin=$1; shift
    local t0 out pid
    t0=$(date +%s%N)
    exec 3< <("$BIN/$bin" "$@" --cycles 5 --frames 240 --idle 10)
    while IFS= read -r line <&3; do
        case $line in
            "M cold "*)
                local rt=${line##*first_frame_realtime_ns=}
                echo "M exec_to_first_frame_us=$(( (rt - t0) / 1000 ))"
                ;;
            "M idle_begin "*)
                pid=${line##*pid=}
                sleep 2
                echo "M vram=$(vram_of "$pid")"
                ;;
        esac
        echo "$line"
    done
    exec 3<&-
}

ONLY=${2:-}
for i in $(seq 1 "$RUNS"); do
    for size in "" "--full"; do
        echo "=== run=$i cpu $size"
        [[ -z $ONLY || $ONLY == cpu ]] && run cpu $size
        for gpu in ${ONLY:-gles gpu-wgpu}; do
            [[ $gpu == cpu ]] && continue
            for mode in "" "--lazy"; do
                echo "=== run=$i $gpu $size $mode"
                run "$gpu" $size $mode
            done
        done
    done
done
