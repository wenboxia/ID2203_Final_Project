# ID2203 Final Project: Battle-testing OmniPaxos with Jepsen

## Project Overview

This project verifies that the [OmniPaxos](https://omnipaxos.com) distributed KV store preserves **linearizability** under aggressive network partitioning and node failures. It implements a complete Jepsen-style test suite using [Maelstrom](https://github.com/jepsen-io/maelstrom) with the Knossos linearizability checker.

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
- Reads JSON messages from stdin and writes responses via `send_msg()`
- Dispatches `read`, `write`, and `cas` requests in `handle_client_request()`
- Wraps each operation as a `MaelstromCommand` and appends it to the OmniPaxos consensus log
- Forwards requests to the current leader if the local node is not the leader

### 2. Client & Generator

**Primary**: Maelstrom's built-in `lin-kv` workload serves as the client and generator, automatically producing a mix of read/write/CAS operations and verifying results with Knossos.

**Indeterminate state handling** (`src/bin/jepsen_harness.rs`, `record_response()`):
- `*_ok` responses → recorded as `"ok"` (definitely succeeded)
- Error code 20 (key not found) or 22 (CAS precondition failed) → `"fail"` (definitely failed)
- Error code 11 or other → `"info"` (indeterminate: the operation may or may not have taken effect during a partition)

### 3. Fault Injection (Nemesis)

Maelstrom's `--nemesis partition` and `--nemesis kill` flags inject faults controlled by the Maelstrom framework.

**Network Partitions** (`src/bin/jepsen_harness.rs`, `NetworkSimulator`):
- `isolate_node()`: Isolates a single node by adding all its bidirectional links to the partition table
- `split_brain()`: Splits the cluster into two halves — all cross-partition messages are dropped
- `deliver_messages()`: Checks `is_partitioned()` before routing; partitioned messages are silently dropped

**Node Crashes**:
- Kill: Calls `process.kill()` on the OS process
- Restart: Re-spawns the process and sends `init` message; node joins the cluster with fresh state

### 4. Verification of Linearizability

**Knossos (via Maelstrom)**: When running the Maelstrom `lin-kv` workload, Knossos automatically analyzes the operation history. Output `"No anomalies found"` = PASS.

### Linearizable Reads Design

**Problem**: Local reads in a consensus system are NOT linearizable — a node may serve stale values if it hasn't received the latest decided entries, or if it's in a minority partition still believing it's the leader.

**Solution**: All read operations go through the OmniPaxos consensus log.

1. `KVOp::Read` is appended to the log in `handle_client_request()`, just like write/CAS
2. OmniPaxos replicates and orders the read among all other operations via majority quorum
3. In `apply_and_respond()`, only after the read is **decided** (majority-confirmed) does the coordinator return the value

**Guarantee**: When a read is decided, all writes proposed before it are also decided and applied. The value returned is always up-to-date — no stale reads possible. The trade-off is increased read latency (one round of consensus), but this ensures full linearizability.

### CAS (Compare-and-Swap) Design

CAS operations are linearizable because:
1. The CAS command is appended to the OmniPaxos log
2. All nodes apply commands in the same total order
3. The condition check (`current == expected`) happens at **apply-time** in `apply_and_respond()`, not propose-time
4. Two concurrent CAS operations are totally ordered by OmniPaxos — the first decided succeeds, the second fails with precondition error

---

## Demo

### Prerequisites

- [Rust](https://www.rust-lang.org/tools/install)
- [Java](https://openjdk.java.net/) (for Maelstrom)
- Graphviz (optional, for result visualization)

### Step 1 — Basic test (no faults)

Covers: Testable Shim, Client & Generator, Knossos verification.

```bash
cargo build --release --bin maelstrom-node
./scripts/run_tests.sh basic
```

Expected: `No anomalies found. ಠ~ಠ` → TEST PASSED

### Step 2 — Partition test (most important)

Covers: Nemesis fault injection, linearizability under partitions.

```bash
./scripts/run_tests.sh partition
```

Maelstrom randomly isolates nodes and creates split-brain scenarios. OmniPaxos re-elects a leader among the majority partition. Knossos verifies no linearizability violations across the entire history.

Expected: `No anomalies found. ಠ~ಠ` → TEST PASSED

### Step 3 — Code walkthrough: linearizable reads

Open `src/bin/maelstrom_node.rs` and show three key points:

| Code | Explanation |
|------|-------------|
| `"read" => KVOp::Read(key)` | Read is wrapped as a log entry, same as write |
| `self.omnipaxos.append(command)` | Read enters the consensus log — NOT a local read |
| `apply_and_respond()` Read branch | Result only returned after OmniPaxos **decides** |

> All reads go through OmniPaxos majority quorum. When a read is decided, all prior writes are already applied — stale reads are impossible.

---

## All Test Commands

```bash
# Install Maelstrom (one-time)
./scripts/install_maelstrom.sh

# Basic test (no faults)
./scripts/run_tests.sh basic

# Network partition test
./scripts/run_tests.sh partition

# Node crash test
./scripts/run_tests.sh kill

# Run all tests sequentially
./scripts/run_tests.sh all
```

### Docker (optional)

```bash
docker build -f Dockerfile.maelstrom -t omnipaxos-jepsen .
docker run --rm -v $(pwd)/test-results:/app/test-results omnipaxos-jepsen partition
```

---

## Nemesis Types

| Nemesis | Description |
|---------|-------------|
| `partition` | Randomly isolate nodes or split the cluster into halves (split-brain). |
| `kill` | Randomly kill and restart nodes. |

---

## Test Results

Results are saved to `test-results/`:
- `history.json` — Operation history
- Maelstrom results in `store/` (includes Knossos linearizability analysis)

### Interpreting Results

- **PASS**: `"No anomalies found. ಠ~ಠ"` — The operation history is linearizable.
- **FAIL**: A linearizability violation was found — stale read, lost acknowledged write, or CAS anomaly.

---

## File Structure

```
src/
├── bin/
│   ├── maelstrom_node.rs    # Maelstrom-compatible OmniPaxos node (Testable Shim)
│   └── jepsen_harness.rs    # Standalone test harness with indeterminate state handling
├── server/                   # Original OmniPaxos KV server (TCP, not used for testing)
├── client/                   # Original KV client (TCP, not used for testing)
└── lib.rs                    # Library root

scripts/
├── install_maelstrom.sh      # Maelstrom installation
└── run_tests.sh              # Test runner

Dockerfile.maelstrom          # Docker build for reproducible testing
docker-compose.maelstrom.yml  # Docker Compose test suites
```

---

## References

- [Jepsen](https://jepsen.io) — Industry standard for distributed systems testing
- [Maelstrom](https://github.com/jepsen-io/maelstrom) — Workbench for learning distributed systems
- [Knossos](https://github.com/jepsen-io/knossos) — Linearizability checker using the WGL algorithm
- [OmniPaxos](https://omnipaxos.com) — Rust consensus library
- Original KV repository: [haraldng/omnipaxos-kv](https://github.com/haraldng/omnipaxos-kv)
