# Battle-testing OmniPaxos with Jepsen/Maelstrom

## Overview

This project implements a Jepsen-style test suite for the OmniPaxos distributed KV store,
verifying that the implementation preserves **linearizability** under aggressive network
partitioning and node failures.

### Architecture

```
                    Maelstrom / Jepsen Harness
                    ┌─────────────────────────┐
                    │  Generator (r/w/cas ops) │
                    │  Nemesis (partitions)    │
                    │  Checker (linearizability)│
                    └────────┬────────────────┘
                             │ stdin/stdout JSON
              ┌──────────────┼──────────────┐
              ▼              ▼              ▼
         ┌─────────┐   ┌─────────┐   ┌─────────┐
         │  Node 0  │◄──│  Node 1  │◄──│  Node 2  │
         │(OmniPaxos)│──►│(OmniPaxos)│──►│(OmniPaxos)│
         │  KV Store │   │  KV Store │   │  KV Store │
         └─────────┘   └─────────┘   └─────────┘
              │              │              │
              └──────────────┴──────────────┘
                    OmniPaxos Consensus
```

## Components

### 1. Maelstrom Node (`src/bin/maelstrom_node.rs`)

A Maelstrom-compatible binary that wraps OmniPaxos consensus. Each node:
- Speaks the Maelstrom JSON protocol over stdin/stdout
- Runs an OmniPaxos instance for distributed consensus
- Supports `read`, `write`, and `cas` (compare-and-swap) operations
- Routes OmniPaxos protocol messages via Maelstrom's network layer

### 2. Jepsen Test Harness (`src/bin/jepsen_harness.rs`)

A standalone Rust-based test harness that:
- Spawns multiple maelstrom-node processes
- Acts as the network layer (routing messages between nodes)
- Generates random read/write/CAS operations
- Injects network partitions and node crashes
- Records operation history
- Checks linearizability using the WGL algorithm

### 3. Test Scripts (`scripts/`)

- `install_maelstrom.sh` - Installs Maelstrom and dependencies
- `run_tests.sh` - Runs Maelstrom lin-kv workload tests with various nemesis configurations

## Linearizable Reads Design

**Problem**: Simple local reads from a consensus-based system are NOT linearizable. A node
might serve a stale value if it hasn't received the latest decided entries.

**Solution**: All reads go through the OmniPaxos log. When a client sends a `read` request:
1. The read is wrapped in a `MaelstromCommand` and appended to the OmniPaxos log
2. OmniPaxos replicates and orders the read among all other operations
3. When the read is decided, the coordinator reads the value from its local KV store
4. At this point, all writes that were ordered before the read are guaranteed to be applied

This ensures linearizability at the cost of read latency (reads now require consensus).
Alternative approaches like leader leases or ReadIndex could reduce latency but add complexity.

## CAS (Compare-and-Swap) Design

CAS operations are linearizable because:
1. The CAS command is appended to the OmniPaxos log
2. All nodes apply commands in the same total order
3. The condition check (current value == expected value) happens at apply-time
4. Only the coordinator node sends the response (success or error)

Two concurrent CAS operations on the same key are ordered by OmniPaxos. The first one
decided succeeds (if conditions match), and the second fails with a precondition error.

## Quick Start

### Option A: Using Maelstrom (recommended for full verification)

```bash
# 1. Install Maelstrom
./scripts/install_maelstrom.sh

# 2. Build the node binary
cargo build --release --bin maelstrom-node

# 3. Run tests
./scripts/run_tests.sh basic       # No faults
./scripts/run_tests.sh partition   # Network partitions
./scripts/run_tests.sh kill        # Node crashes
./scripts/run_tests.sh all-faults  # Combined faults
./scripts/run_tests.sh all         # Run all tests
```

### Option B: Using the Rust Test Harness

```bash
# 1. Build all binaries
cargo build --release

# 2. Run the harness
cargo run --release --bin jepsen-harness -- \
  --nodes 3 \
  --duration 30 \
  --rate 10 \
  --concurrency 5 \
  --nemesis partition \
  --keys 5
```

### Option C: Using Docker

```bash
# Build and run basic test
docker build -f Dockerfile.maelstrom -t omnipaxos-jepsen .
docker run --rm -v $(pwd)/test-results:/app/test-results omnipaxos-jepsen basic

# Or use docker-compose for specific test suites
docker compose -f docker-compose.maelstrom.yml run --rm jepsen-partition
docker compose -f docker-compose.maelstrom.yml run --rm jepsen-all
```

## Nemesis Types

| Nemesis | Description |
|---------|-------------|
| `none` | No faults. Baseline linearizability test. |
| `partition` | Randomly isolate nodes or split the cluster into halves (split-brain). |
| `kill` | Randomly kill and restart nodes. |
| `all-faults` | Combined partition and kill. |

## Test Results

Results are saved to `test-results/`:
- `output.log` - Test execution log
- `maelstrom-results/` - Maelstrom's analysis (includes linearizability check via Knossos)
- `history.json` - Operation history (from Rust harness)

### Interpreting Results

**PASS**: The operation history is linearizable. All reads returned values consistent
with a sequential execution that respects real-time ordering.

**FAIL**: A linearizability violation was found. This means either:
- A stale read occurred (reading an old value after a newer write was acknowledged)
- A lost write (acknowledged write not reflected in subsequent reads)
- A CAS anomaly (successful CAS on a value that was already changed)

## Error Codes (Maelstrom Protocol)

| Code | Meaning |
|------|---------|
| 20 | Key does not exist |
| 22 | Precondition failed (CAS value mismatch) |
| 10 | Not supported |
| 11 | Temporarily unavailable |

## File Structure

```
src/
├── bin/
│   ├── maelstrom_node.rs    # Maelstrom-compatible OmniPaxos node
│   └── jepsen_harness.rs    # Standalone test harness + linearizability checker
├── server/                   # Original OmniPaxos KV server
├── client/                   # Original KV client
├── common.rs                 # Shared types
└── lib.rs                    # Library root

scripts/
├── install_maelstrom.sh      # Maelstrom installation
└── run_tests.sh              # Test runner with nemesis configurations

Dockerfile.maelstrom          # Docker build for reproducible testing
docker-compose.maelstrom.yml  # Docker Compose test suites
```
