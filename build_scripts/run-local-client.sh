#!/bin/bash

# usage="Usage: run-local-client.sh first_clients_server_id second_clients_server_id"
# [ -z "$1" ] &&  echo "No first_clients_server_id given! $usage" && exit 1
# [ -z "$2" ] &&  echo "No second_clients_server_id given! $usage" && exit 1
# server_id=$1
# other_id=$2
server_id=1
other_id=2

# Clean up child processes
interrupt() {
    pkill -P $$
}
trap "interrupt" SIGINT

client1_config_path="./client-${server_id}-config.toml"
client2_config_path="./client-${other_id}-config.toml"
local_experiment_dir="../benchmarks/logs/local-run"
mkdir -p "${local_experiment_dir}"

RUST_LOG=debug CONFIG_FILE="$client1_config_path"  cargo run --release --manifest-path="../Cargo.toml" --bin client &
RUST_LOG=debug CONFIG_FILE="$client2_config_path"  cargo run --release --manifest-path="../Cargo.toml" --bin client
