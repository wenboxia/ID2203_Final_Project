# ID2203 Final Project: Battle-testing OmniPaxos with Jepsen

## Project Overview

This project verifies that the [OmniPaxos](https://omnipaxos.com) distributed KV store preserves **linearizability** under aggressive network partitioning and node failures. It implements a complete Jepsen-style test suite using [Maelstrom](https://github.com/jepsen-io/maelstrom) and a custom Rust test harness with a WGL linearizability checker.

> **Hypothesis**: "The OmniPaxos implementation preserves linearizability under aggressive network partitioning and node failures."

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

---

## Requirements Implementation

### 1. Testable Shim

**File**: `src/bin/maelstrom_node.rs`

A Maelstrom-compatible binary that wraps OmniPaxos consensus, exposing a programmable API over **stdin/stdout JSON** (the Maelstrom protocol). Each node:
- Reads JSON messages from stdin (line 432–448) and writes responses via `send_msg()` (line 117–122)
- Dispatches `read`, `write`, and `cas` requests in `handle_client_request()` (line 201–232)
- Wraps each operation as a `MaelstromCommand` (line 37–46) and appends it to the OmniPaxos consensus log
- Forwards requests to the current leader if the local node is not the leader (line 235–263)

### 2. Client & Generator

**File**: `src/bin/jepsen_harness.rs`

- **Generator** (line 716–757): Produces a mix of random operations per tick — 40% read, 40% write, 20% CAS — targeting random keys and random nodes
- **Indeterminate state handling** (line 641–682, `record_response()`):
  - `*_ok` responses → recorded as `"ok"` (definitely succeeded)
  - Error code 20 (key not found) or 22 (CAS precondition failed) → `"fail"` (definitely failed)
  - Error code 11 or other → `"info"` (indeterminate: the operation may or may not have taken effect during a partition)
- The WGL checker treats `"info"` entries conservatively — their completion time is set to infinity, allowing them to linearize at any point in history

Additionally, Maelstrom's built-in `lin-kv` workload serves as an independent client and generator with its own Knossos-based verification.

### 3. Fault Injection (Nemesis)

**File**: `src/bin/jepsen_harness.rs`, `NetworkSimulator` struct (line 349–421)

**Network Partitions**:
- `isolate_node()` (line 376–386): Isolates a single node by adding all its bidirectional links to the partition table
- `split_brain()` (line 388–404): Splits the cluster into two halves — all cross-partition messages are dropped
- `deliver_messages()` (line 621–639): Checks `is_partitioned()` before routing; partitioned messages are silently dropped

**Node Crashes**:
- Kill (line 774–779): Calls `process.kill()` on the OS process
- Restart (line 802–821): Re-spawns the process and sends `init` message; node joins the cluster with fresh state

Maelstrom's `--nemesis partition` flag provides equivalent partition injection controlled by the Maelstrom framework.

### 4. Verification of Linearizability

**Checker 1 — Knossos (via Maelstrom)**: When running the Maelstrom `lin-kv` workload, Knossos automatically analyzes the operation history. Output `"No anomalies found"` = PASS.

**Checker 2 — Custom WGL (Rust implementation)** in `src/bin/jepsen_harness.rs`:
- `check_linearizability()` (line 141–235): Pairs invoke/complete events by operation ID, groups operations by key
- `try_linearize()` (line 238–272): Backtracking search over all possible linearizations
- `KVModel.apply()` (line 77–121): Simulates the KV state machine to validate each candidate ordering

### Linearizable Reads Design

**Problem**: Local reads in a consensus system are NOT linearizable — a node may serve stale values if it hasn't received the latest decided entries, or if it's in a minority partition still believing it's the leader.

**Solution**: All read operations go through the OmniPaxos consensus log.

1. `handle_client_request()` (line 208): `KVOp::Read` is appended to the log just like write/CAS
2. OmniPaxos replicates and orders the read among all other operations via majority quorum
3. `apply_and_respond()` (line 329–355): Only after the read is **decided** (majority-confirmed) does the coordinator return the value

**Guarantee**: When a read is decided, all writes proposed before it are also decided and applied. The value returned is always up-to-date — no stale reads possible. The trade-off is increased read latency (one round of consensus), but this ensures full linearizability.

### CAS (Compare-and-Swap) Design

CAS operations are linearizable because:
1. The CAS command is appended to the OmniPaxos log
2. All nodes apply commands in the same total order
3. The condition check (`current == expected`) happens at **apply-time** (line 370–378), not propose-time
4. Two concurrent CAS operations are totally ordered by OmniPaxos — the first decided succeeds, the second fails with precondition error

---

## Quick Start

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install)
- [Java](https://openjdk.java.net/) (for Maelstrom)
- [Graphviz](https://graphviz.org/) (optional, for Maelstrom result visualization)

### Option A: Maelstrom Tests (recommended)

```bash
# 1. Install Maelstrom
./scripts/install_maelstrom.sh

# 2. Build
cargo build --release --bin maelstrom-node

# 3. Run tests
./scripts/run_tests.sh basic       # No faults
./scripts/run_tests.sh partition   # Network partitions
./scripts/run_tests.sh kill        # Node crashes
./scripts/run_tests.sh all         # Run all tests
```

### Option B: Rust Test Harness

```bash
# 1. Build
cargo build --release

# 2. Run with partition nemesis
cargo run --release --bin jepsen-harness -- \
  --nodes 3 --duration 30 --rate 10 --concurrency 5 \
  --nemesis partition --keys 5
```

### Option C: Docker

```bash
docker build -f Dockerfile.maelstrom -t omnipaxos-jepsen .
docker run --rm -v $(pwd)/test-results:/app/test-results omnipaxos-jepsen basic

# Or via docker-compose
docker compose -f docker-compose.maelstrom.yml run --rm jepsen-partition
```

---

## Nemesis Types

| Nemesis | Description |
|---------|-------------|
| `none` | No faults. Baseline linearizability test. |
| `partition` | Randomly isolate nodes or split the cluster into halves (split-brain). |
| `kill` | Randomly kill and restart nodes. |

---

## Test Results

Results are saved to `test-results/`:
- `history.json` — Operation history (from Rust harness)
- Maelstrom results in `store/` (includes Knossos linearizability analysis)

### Interpreting Results

- **PASS**: The operation history is linearizable. All reads returned values consistent with a sequential execution respecting real-time ordering.
- **FAIL**: A linearizability violation was found — stale read, lost acknowledged write, or CAS anomaly.

---

## File Structure

```
src/
├── bin/
│   ├── maelstrom_node.rs    # Maelstrom-compatible OmniPaxos node (Testable Shim)
│   └── jepsen_harness.rs    # Test harness + WGL linearizability checker
├── server/                   # Original OmniPaxos KV server
├── client/                   # Original KV client
└── lib.rs                    # Library root

scripts/
├── install_maelstrom.sh      # Maelstrom installation
└── run_tests.sh              # Test runner

Dockerfile.maelstrom          # Docker build for reproducible testing
docker-compose.maelstrom.yml  # Docker Compose test suites
quickanswer.md                # Quick reference for requirement implementation details
testplan.md                   # Acceptance test plan
```

---

## References

- [Jepsen](https://jepsen.io) — Industry standard for distributed systems testing
- [Maelstrom](https://github.com/jepsen-io/maelstrom) — Workbench for learning distributed systems
- [Knossos](https://github.com/jepsen-io/knossos) — Linearizability checker using the WGL algorithm
- [OmniPaxos](https://omnipaxos.com) — Rust consensus library
- Original KV repository: [haraldng/omnipaxos-kv](https://github.com/haraldng/omnipaxos-kv)
