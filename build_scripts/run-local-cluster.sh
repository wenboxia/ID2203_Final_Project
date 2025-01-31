#!/bin/bash

usage="Usage: run-local-cluster.sh"
cluster_size=3

# Clean up child processes
interrupt() {
    pkill -P $$
}
trap "interrupt" SIGINT

local_experiment_dir="../benchmarks/logs/local-run"
mkdir -p "${local_experiment_dir}"

for ((i = 1; i <= cluster_size; i++)); do
    config_path="./server-${i}-config.toml"
    if [ i -eq 3 ]; then
        RUST_LOG=debug CONFIG_FILE="$config_path" cargo run --release --manifest-path="../Cargo.toml" --bin server &
    else
        RUST_LOG=info CONFIG_FILE="$config_path" cargo run --release --manifest-path="../Cargo.toml" --bin server &
    fi

done
wait

