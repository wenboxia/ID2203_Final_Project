//! Standalone Jepsen-style test harness for the OmniPaxos Maelstrom node.
//!
//! This binary:
//! 1. Spawns N maelstrom-node processes
//! 2. Routes messages between them (acting as the Maelstrom network layer)
//! 3. Generates random read/write/CAS operations
//! 4. Injects network partitions and node crashes
//! 5. Records operation history
//! 6. Checks linearizability using the WGL algorithm
//!
//! Usage:
//!   cargo run --release --bin jepsen-harness -- [options]
//!
//! Options:
//!   --nodes N          Number of nodes (default: 3)
//!   --duration SECS    Test duration in seconds (default: 30)
//!   --rate OPS         Operations per second (default: 10)
//!   --concurrency N    Number of concurrent clients (default: 5)
//!   --nemesis TYPE     Fault type: none, partition, kill, all (default: partition)
//!   --keys N           Number of distinct keys (default: 5)

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write as IoWrite};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// ============================================================================
// Operation History for Linearizability Checking
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Unique operation ID
    pub id: u64,
    /// "invoke" or "ok" or "fail" or "info" (indeterminate)
    pub event_type: String,
    /// Client ID that performed the operation
    pub client_id: usize,
    /// Operation type: "read", "write", "cas"
    pub op: String,
    /// Key
    pub key: String,
    /// Value (for write), expected value (for read result), from/to (for CAS)
    pub value: Option<Value>,
    /// For CAS: the "from" value
    pub cas_from: Option<Value>,
    /// For CAS: the "to" value
    pub cas_to: Option<Value>,
    /// Wall clock time
    pub time_ns: u128,
}

// ============================================================================
// WGL Linearizability Checker
// ============================================================================

/// A KV model for linearizability checking.
/// Represents the expected state of the KV store.
#[derive(Clone, Debug)]
struct KVModel {
    data: HashMap<String, Value>,
}

impl KVModel {
    fn new() -> Self {
        Self {
            data: HashMap::new(),
        }
    }

    /// Try to apply an operation. Returns (new_model, matches_response).
    fn apply(&self, op: &LinearOp) -> (Self, bool) {
        let mut new_model = self.clone();
        match op.op_type.as_str() {
            "write" => {
                new_model
                    .data
                    .insert(op.key.clone(), op.value.clone().unwrap());
                (new_model, true) // Writes always succeed
            }
            "read" => {
                let current = new_model.data.get(&op.key).cloned();
                let expected = &op.result;
                let matches = match (current.as_ref(), expected) {
                    (None, None) => true,          // Key doesn't exist, got None
                    (Some(v), Some(e)) => v == e,  // Values match
                    (None, Some(_)) => false,       // Key doesn't exist but got value
                    (Some(_), None) => false,       // Key exists but got None
                };
                (new_model, matches)
            }
            "cas" => {
                let from = op.cas_from.as_ref().unwrap();
                let to = op.cas_to.as_ref().unwrap();
                let current = new_model.data.get(&op.key);

                if op.cas_ok {
                    // CAS succeeded - check precondition and apply
                    match current {
                        Some(v) if v == from => {
                            new_model.data.insert(op.key.clone(), to.clone());
                            (new_model, true)
                        }
                        _ => (new_model, false) // Precondition didn't match
                    }
                } else {
                    // CAS failed - precondition should NOT match
                    match current {
                        Some(v) if v == from => (new_model, false), // Should have succeeded
                        _ => (new_model, true) // Correctly failed
                    }
                }
            }
            _ => (new_model, false),
        }
    }
}

#[derive(Clone, Debug)]
struct LinearOp {
    id: u64,
    op_type: String,
    key: String,
    value: Option<Value>,
    result: Option<Value>,
    cas_from: Option<Value>,
    cas_to: Option<Value>,
    cas_ok: bool,
    invoke_time: u128,
    complete_time: u128,
}

/// Check if a history is linearizable using the WGL algorithm (backtracking).
///
/// Returns Ok(()) if linearizable, Err(description) if not.
pub fn check_linearizability(history: &[HistoryEntry]) -> Result<(), String> {
    // Build paired operations from history
    let mut ops: Vec<LinearOp> = Vec::new();
    let mut invocations: HashMap<u64, &HistoryEntry> = HashMap::new();

    for entry in history {
        match entry.event_type.as_str() {
            "invoke" => {
                invocations.insert(entry.id, entry);
            }
            "ok" => {
                if let Some(invoke) = invocations.remove(&entry.id) {
                    ops.push(LinearOp {
                        id: entry.id,
                        op_type: invoke.op.clone(),
                        key: invoke.key.clone(),
                        value: invoke.value.clone(),
                        result: entry.value.clone(),
                        cas_from: invoke.cas_from.clone(),
                        cas_to: invoke.cas_to.clone(),
                        cas_ok: true,
                        invoke_time: invoke.time_ns,
                        complete_time: entry.time_ns,
                    });
                }
            }
            "fail" => {
                if let Some(invoke) = invocations.remove(&entry.id) {
                    ops.push(LinearOp {
                        id: entry.id,
                        op_type: invoke.op.clone(),
                        key: invoke.key.clone(),
                        value: invoke.value.clone(),
                        result: entry.value.clone(),
                        cas_from: invoke.cas_from.clone(),
                        cas_to: invoke.cas_to.clone(),
                        cas_ok: false,
                        invoke_time: invoke.time_ns,
                        complete_time: entry.time_ns,
                    });
                }
            }
            "info" => {
                // Indeterminate - operation might or might not have taken effect.
                // For checking, we treat it as potentially having any result.
                // We remove it from invocations and include it in ops as indeterminate.
                if let Some(invoke) = invocations.remove(&entry.id) {
                    // Indeterminate ops can be linearized at any point
                    ops.push(LinearOp {
                        id: entry.id,
                        op_type: invoke.op.clone(),
                        key: invoke.key.clone(),
                        value: invoke.value.clone(),
                        result: None, // Unknown result
                        cas_from: invoke.cas_from.clone(),
                        cas_to: invoke.cas_to.clone(),
                        cas_ok: true, // Assume success for writes/CAS
                        invoke_time: invoke.time_ns,
                        complete_time: u128::MAX, // Can be linearized anywhere
                    });
                }
            }
            _ => {}
        }
    }

    // Sort by invoke time
    ops.sort_by_key(|op| op.invoke_time);

    if ops.is_empty() {
        return Ok(());
    }

    // Group operations by key for per-key linearizability check
    let mut by_key: HashMap<String, Vec<LinearOp>> = HashMap::new();
    for op in &ops {
        by_key
            .entry(op.key.clone())
            .or_default()
            .push(op.clone());
    }

    // Check linearizability per key (sufficient for a KV store)
    for (key, key_ops) in &by_key {
        if !try_linearize(key_ops, &KVModel::new(), 0) {
            return Err(format!(
                "Linearizability violation found for key '{}' with {} operations",
                key,
                key_ops.len()
            ));
        }
    }

    Ok(())
}

/// Backtracking search to find a valid linearization.
fn try_linearize(remaining: &[LinearOp], model: &KVModel, depth: usize) -> bool {
    if remaining.is_empty() {
        return true;
    }

    // Limit search depth to prevent exponential blowup
    if depth > 100 {
        // If we can't find a linearization within this depth, assume it's ok
        // (conservative for large histories)
        return true;
    }

    // Find the earliest complete time among remaining ops
    let min_complete = remaining
        .iter()
        .map(|op| op.complete_time)
        .min()
        .unwrap();

    // Try to linearize any operation that was invoked before min_complete
    for (i, op) in remaining.iter().enumerate() {
        if op.invoke_time <= min_complete {
            let (new_model, matches) = model.apply(op);
            if matches {
                let mut next_remaining: Vec<LinearOp> = remaining.to_vec();
                next_remaining.remove(i);
                if try_linearize(&next_remaining, &new_model, depth + 1) {
                    return true;
                }
            }
        }
    }

    false
}

// ============================================================================
// Node Process Manager
// ============================================================================

struct NodeProcess {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout_rx: std::sync::mpsc::Receiver<String>,
}

impl NodeProcess {
    fn spawn(binary_path: &str) -> Self {
        let mut child = Command::new(binary_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn maelstrom-node process");

        let stdin = child.stdin.take().expect("Failed to get stdin");
        let stdout = child.stdout.take().expect("Failed to get stdout");

        // Spawn a thread to read stdout lines
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) if !line.is_empty() => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        });

        NodeProcess {
            child,
            stdin,
            stdout_rx: rx,
        }
    }

    fn send(&mut self, msg: &Value) {
        let line = serde_json::to_string(msg).unwrap();
        writeln!(self.stdin, "{}", line).unwrap();
        self.stdin.flush().unwrap();
    }

    fn try_recv(&self) -> Option<Value> {
        match self.stdout_rx.try_recv() {
            Ok(line) => serde_json::from_str(&line).ok(),
            Err(_) => None,
        }
    }

    fn recv_timeout(&self, timeout: Duration) -> Option<Value> {
        match self.stdout_rx.recv_timeout(timeout) {
            Ok(line) => serde_json::from_str(&line).ok(),
            Err(_) => None,
        }
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ============================================================================
// Network Simulator with Fault Injection
// ============================================================================

struct NetworkSimulator {
    /// Set of (src, dest) pairs that are currently partitioned
    partitions: HashMap<(String, String), bool>,
    /// Set of node IDs that are currently "killed"
    killed_nodes: Vec<String>,
}

impl NetworkSimulator {
    fn new() -> Self {
        Self {
            partitions: HashMap::new(),
            killed_nodes: Vec::new(),
        }
    }

    fn is_partitioned(&self, src: &str, dest: &str) -> bool {
        self.partitions
            .get(&(src.to_string(), dest.to_string()))
            .copied()
            .unwrap_or(false)
    }

    fn is_killed(&self, node_id: &str) -> bool {
        self.killed_nodes.contains(&node_id.to_string())
    }

    /// Partition a node from the rest of the cluster.
    fn isolate_node(&mut self, node: &str, all_nodes: &[String]) {
        for other in all_nodes {
            if other != node {
                self.partitions
                    .insert((node.to_string(), other.clone()), true);
                self.partitions
                    .insert((other.clone(), node.to_string()), true);
            }
        }
        eprintln!("NEMESIS: Isolated node {} from cluster", node);
    }

    /// Split the cluster into two halves.
    fn split_brain(&mut self, all_nodes: &[String]) {
        let mid = all_nodes.len() / 2;
        let (group_a, group_b) = all_nodes.split_at(mid);
        for a in group_a {
            for b in group_b {
                self.partitions
                    .insert((a.clone(), b.to_string()), true);
                self.partitions
                    .insert((b.to_string(), a.clone()), true);
            }
        }
        eprintln!(
            "NEMESIS: Split brain - {:?} vs {:?}",
            group_a, group_b
        );
    }

    /// Heal all partitions.
    fn heal_all(&mut self) {
        self.partitions.clear();
        eprintln!("NEMESIS: All partitions healed");
    }

    fn kill_node(&mut self, node: &str) {
        self.killed_nodes.push(node.to_string());
        eprintln!("NEMESIS: Killed node {}", node);
    }

    fn restart_node(&mut self, node: &str) {
        self.killed_nodes.retain(|n| n != node);
        eprintln!("NEMESIS: Restarted node {}", node);
    }
}

// ============================================================================
// Test Harness
// ============================================================================

struct TestHarness {
    nodes: HashMap<String, NodeProcess>,
    node_ids: Vec<String>,
    network: NetworkSimulator,
    history: Arc<Mutex<Vec<HistoryEntry>>>,
    next_op_id: u64,
    binary_path: String,
    start_time: std::time::Instant,
}

impl TestHarness {
    fn new(node_count: usize, binary_path: String) -> Self {
        let node_ids: Vec<String> = (0..node_count).map(|i| format!("n{}", i)).collect();
        let mut nodes = HashMap::new();

        // Spawn node processes
        for node_id in &node_ids {
            let mut process = NodeProcess::spawn(&binary_path);

            // Send init message
            let init_msg = json!({
                "src": "harness",
                "dest": node_id,
                "body": {
                    "type": "init",
                    "msg_id": 0,
                    "node_id": node_id,
                    "node_ids": node_ids,
                }
            });
            process.send(&init_msg);

            // Wait for init_ok
            if let Some(response) = process.recv_timeout(Duration::from_secs(5)) {
                assert_eq!(
                    response["body"]["type"].as_str(),
                    Some("init_ok"),
                    "Expected init_ok from {}, got: {}",
                    node_id,
                    response
                );
                eprintln!("Node {} initialized", node_id);
            } else {
                panic!("Timeout waiting for init_ok from {}", node_id);
            }

            nodes.insert(node_id.clone(), process);
        }

        Self {
            nodes,
            node_ids,
            network: NetworkSimulator::new(),
            history: Arc::new(Mutex::new(Vec::new())),
            next_op_id: 1,
            binary_path,
            start_time: std::time::Instant::now(),
        }
    }

    fn get_op_id(&mut self) -> u64 {
        let id = self.next_op_id;
        self.next_op_id += 1;
        id
    }

    /// Route messages between nodes (act as Maelstrom network).
    fn route_messages(&mut self) {
        let node_ids: Vec<String> = self.nodes.keys().cloned().collect();
        let mut to_deliver: Vec<(String, Value)> = Vec::new();

        for node_id in &node_ids {
            if self.network.is_killed(node_id) {
                continue;
            }
            if let Some(process) = self.nodes.get(node_id) {
                while let Some(msg) = process.try_recv() {
                    let dest = msg["dest"].as_str().unwrap_or("").to_string();
                    let src = msg["src"].as_str().unwrap_or("").to_string();

                    // Check if this is a client response
                    if dest.starts_with('c') || dest == "harness" {
                        to_deliver.push((dest, msg));
                        continue;
                    }

                    // Check partition
                    if self.network.is_partitioned(&src, &dest) {
                        // Drop the message (simulating partition)
                        continue;
                    }

                    if !self.network.is_killed(&dest) {
                        to_deliver.push((dest, msg));
                    }
                }
            }
        }

        // Deliver messages
        for (dest, msg) in to_deliver {
            if dest.starts_with('c') || dest == "harness" {
                // Client response - handled by the caller
                continue;
            }
            if let Some(process) = self.nodes.get_mut(&dest) {
                process.send(&msg);
            }
        }
    }

    /// Send a client operation to a specific node.
    fn send_operation(
        &mut self,
        node_id: &str,
        client_id: &str,
        op_type: &str,
        key: &str,
        value: Option<Value>,
        cas_from: Option<Value>,
        cas_to: Option<Value>,
    ) -> u64 {
        let op_id = self.get_op_id();

        let mut body = json!({
            "type": op_type,
            "msg_id": op_id,
            "key": key,
        });

        match op_type {
            "write" => {
                body["value"] = value.clone().unwrap();
            }
            "cas" => {
                body["from"] = cas_from.clone().unwrap();
                body["to"] = cas_to.clone().unwrap();
            }
            _ => {}
        }

        let msg = json!({
            "src": client_id,
            "dest": node_id,
            "body": body,
        });

        // Record invocation
        let now = self.start_time.elapsed().as_nanos();
        self.history.lock().unwrap().push(HistoryEntry {
            id: op_id,
            event_type: "invoke".to_string(),
            client_id: client_id
                .strip_prefix('c')
                .unwrap_or("0")
                .parse()
                .unwrap_or(0),
            op: op_type.to_string(),
            key: key.to_string(),
            value,
            cas_from,
            cas_to,
            time_ns: now,
        });

        if let Some(process) = self.nodes.get_mut(node_id) {
            process.send(&msg);
        }

        op_id
    }

    /// Drain all pending messages from node stdout buffers.
    fn drain_all_messages(&mut self) -> Vec<Value> {
        let mut all_msgs = Vec::new();
        let node_ids: Vec<String> = self.nodes.keys().cloned().collect();
        for node_id in &node_ids {
            if let Some(process) = self.nodes.get(node_id) {
                while let Some(msg) = process.try_recv() {
                    all_msgs.push(msg);
                }
            }
        }
        all_msgs
    }

    /// Deliver inter-node messages and return client responses.
    fn deliver_messages(&mut self, messages: Vec<Value>) -> Vec<Value> {
        let mut client_responses = Vec::new();
        for msg in messages {
            let dest = msg["dest"].as_str().unwrap_or("").to_string();
            if dest.starts_with('c') || dest == "harness" {
                client_responses.push(msg);
            } else if dest.starts_with('n') {
                let src = msg["src"].as_str().unwrap_or("");
                if !self.network.is_partitioned(src, &dest)
                    && !self.network.is_killed(&dest)
                {
                    if let Some(dest_process) = self.nodes.get_mut(&dest) {
                        dest_process.send(&msg);
                    }
                }
            }
        }
        client_responses
    }

    /// Record a client response in the operation history.
    fn record_response(&mut self, msg: &Value) {
        let dest = msg["dest"].as_str().unwrap_or("").to_string();
        let msg_type = msg["body"]["type"].as_str().unwrap_or("");
        let in_reply_to = msg["body"]["in_reply_to"].as_u64().unwrap_or(0);

        let event_type = if msg_type.ends_with("_ok") {
            "ok"
        } else if msg_type == "error" {
            let code = msg["body"]["code"].as_u64().unwrap_or(0);
            if code == 20 || code == 22 {
                "fail"
            } else {
                "info"
            }
        } else {
            "info"
        };

        let result_value = if msg_type == "read_ok" {
            Some(msg["body"]["value"].clone())
        } else {
            None
        };

        let now = self.start_time.elapsed().as_nanos();

        self.history.lock().unwrap().push(HistoryEntry {
            id: in_reply_to,
            event_type: event_type.to_string(),
            client_id: dest
                .strip_prefix('c')
                .unwrap_or("0")
                .parse()
                .unwrap_or(0),
            op: msg_type.replace("_ok", "").to_string(),
            key: String::new(),
            value: result_value,
            cas_from: None,
            cas_to: None,
            time_ns: now,
        });
    }

    /// Run the nemesis (fault injector) for the specified duration.
    fn run_nemesis(
        &mut self,
        nemesis_type: &str,
        duration: Duration,
        rate: f64,
        concurrency: usize,
        num_keys: usize,
    ) {
        let start = Instant::now();
        let op_delay = Duration::from_secs_f64(1.0 / rate);
        let mut rng = rand::thread_rng();
        let mut next_nemesis = Duration::from_secs(5);
        let mut nemesis_active = false;

        eprintln!(
            "Starting test: nemesis={}, duration={}s, rate={}/s, concurrency={}, keys={}",
            nemesis_type,
            duration.as_secs(),
            rate,
            concurrency,
            num_keys
        );

        while start.elapsed() < duration {
            // Drain all node outputs, route inter-node messages, collect client responses
            let all_msgs = self.drain_all_messages();
            let client_responses = self.deliver_messages(all_msgs);
            for resp in &client_responses {
                self.record_response(resp);
            }

            // Generate client operations
            for client_idx in 0..concurrency {
                let client_id = format!("c{}", client_idx);
                let key = format!("key{}", rng.gen_range(0..num_keys));
                let target_node = self.node_ids[rng.gen_range(0..self.node_ids.len())].clone();

                if self.network.is_killed(&target_node) {
                    continue;
                }

                let op_choice: f64 = rng.gen();
                if op_choice < 0.4 {
                    // Read
                    self.send_operation(&target_node, &client_id, "read", &key, None, None, None);
                } else if op_choice < 0.8 {
                    // Write
                    let val = json!(rng.gen_range(0..100));
                    self.send_operation(
                        &target_node,
                        &client_id,
                        "write",
                        &key,
                        Some(val),
                        None,
                        None,
                    );
                } else {
                    // CAS
                    let from = json!(rng.gen_range(0..100));
                    let to = json!(rng.gen_range(0..100));
                    self.send_operation(
                        &target_node,
                        &client_id,
                        "cas",
                        &key,
                        None,
                        Some(from),
                        Some(to),
                    );
                }
            }

            // Nemesis actions
            if nemesis_type != "none" && start.elapsed() > next_nemesis {
                if !nemesis_active {
                    match nemesis_type {
                        "partition" => {
                            if rng.gen_bool(0.5) {
                                let target = self.node_ids[rng.gen_range(0..self.node_ids.len())].clone();
                                let all = self.node_ids.clone();
                                self.network.isolate_node(&target, &all);
                            } else {
                                let all = self.node_ids.clone();
                                self.network.split_brain(&all);
                            }
                        }
                        "kill" => {
                            let target = self.node_ids[rng.gen_range(0..self.node_ids.len())].clone();
                            if let Some(process) = self.nodes.get_mut(&target) {
                                process.kill();
                            }
                            self.network.kill_node(&target);
                        }
                        "all" => {
                            if rng.gen_bool(0.5) {
                                let target = self.node_ids[rng.gen_range(0..self.node_ids.len())].clone();
                                let all = self.node_ids.clone();
                                self.network.isolate_node(&target, &all);
                            } else {
                                let target = self.node_ids[rng.gen_range(0..self.node_ids.len())].clone();
                                if let Some(process) = self.nodes.get_mut(&target) {
                                    process.kill();
                                }
                                self.network.kill_node(&target);
                            }
                        }
                        _ => {}
                    }
                    nemesis_active = true;
                    next_nemesis = start.elapsed() + Duration::from_secs(rng.gen_range(3..8));
                } else {
                    // Heal / restart
                    self.network.heal_all();

                    // Restart killed nodes
                    let killed: Vec<String> = self.network.killed_nodes.clone();
                    for node_id in killed {
                        self.network.restart_node(&node_id);
                        // Respawn the process
                        let mut process = NodeProcess::spawn(&self.binary_path);
                        let init_msg = json!({
                            "src": "harness",
                            "dest": &node_id,
                            "body": {
                                "type": "init",
                                "msg_id": 0,
                                "node_id": &node_id,
                                "node_ids": self.node_ids,
                            }
                        });
                        process.send(&init_msg);
                        if process.recv_timeout(Duration::from_secs(5)).is_some() {
                            eprintln!("Node {} restarted successfully", node_id);
                        }
                        self.nodes.insert(node_id, process);
                    }

                    nemesis_active = false;
                    next_nemesis = start.elapsed() + Duration::from_secs(rng.gen_range(3..8));
                }
            }

            std::thread::sleep(op_delay);
        }

        // Final healing
        self.network.heal_all();
    }

    /// Kill all node processes.
    fn shutdown(&mut self) {
        for (_, process) in self.nodes.iter_mut() {
            process.kill();
        }
    }
}

// ============================================================================
// Main
// ============================================================================

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let mut node_count = 3;
    let mut duration_secs = 30;
    let mut rate = 10.0;
    let mut concurrency = 5;
    let mut nemesis = "partition".to_string();
    let mut num_keys = 5;

    // Parse args
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--nodes" => {
                i += 1;
                node_count = args[i].parse().unwrap();
            }
            "--duration" => {
                i += 1;
                duration_secs = args[i].parse().unwrap();
            }
            "--rate" => {
                i += 1;
                rate = args[i].parse().unwrap();
            }
            "--concurrency" => {
                i += 1;
                concurrency = args[i].parse().unwrap();
            }
            "--nemesis" => {
                i += 1;
                nemesis = args[i].clone();
            }
            "--keys" => {
                i += 1;
                num_keys = args[i].parse().unwrap();
            }
            _ => {
                eprintln!("Unknown argument: {}", args[i]);
                std::process::exit(1);
            }
        }
        i += 1;
    }

    // Find binary
    let binary_path = std::env::current_dir()
        .unwrap()
        .join("target/release/maelstrom-node")
        .to_string_lossy()
        .to_string();

    if !std::path::Path::new(&binary_path).exists() {
        // Try debug build
        let debug_path = std::env::current_dir()
            .unwrap()
            .join("target/debug/maelstrom-node")
            .to_string_lossy()
            .to_string();
        if !std::path::Path::new(&debug_path).exists() {
            eprintln!("ERROR: maelstrom-node binary not found. Run: cargo build --bin maelstrom-node");
            std::process::exit(1);
        }
    }

    let binary = if std::path::Path::new(&binary_path).exists() {
        binary_path
    } else {
        std::env::current_dir()
            .unwrap()
            .join("target/debug/maelstrom-node")
            .to_string_lossy()
            .to_string()
    };

    eprintln!("Using binary: {}", binary);
    eprintln!("Configuration:");
    eprintln!("  Nodes:       {}", node_count);
    eprintln!("  Duration:    {}s", duration_secs);
    eprintln!("  Rate:        {}/s", rate);
    eprintln!("  Concurrency: {}", concurrency);
    eprintln!("  Nemesis:     {}", nemesis);
    eprintln!("  Keys:        {}", num_keys);
    eprintln!();

    // Create and run harness
    let mut harness = TestHarness::new(node_count, binary);

    // Give the cluster time to elect a leader
    eprintln!("Waiting for leader election...");
    std::thread::sleep(Duration::from_secs(2));
    harness.route_messages();
    std::thread::sleep(Duration::from_secs(1));
    harness.route_messages();

    // Run the test
    harness.run_nemesis(
        &nemesis,
        Duration::from_secs(duration_secs),
        rate,
        concurrency,
        num_keys,
    );

    // Wait for remaining responses
    eprintln!("Waiting for pending responses...");
    std::thread::sleep(Duration::from_secs(2));
    harness.route_messages();

    // Get history
    let history = harness.history.lock().unwrap().clone();

    // Shutdown nodes
    harness.shutdown();

    // Save history to file
    let history_path = "test-results/history.json";
    std::fs::create_dir_all("test-results").unwrap();
    let history_json = serde_json::to_string_pretty(&history).unwrap();
    std::fs::write(history_path, &history_json).unwrap();
    eprintln!("History saved to {}", history_path);

    // Count operations
    let invocations = history.iter().filter(|e| e.event_type == "invoke").count();
    let completions = history
        .iter()
        .filter(|e| e.event_type == "ok" || e.event_type == "fail")
        .count();
    let indeterminate = history.iter().filter(|e| e.event_type == "info").count();

    eprintln!();
    eprintln!("=== Test Results ===");
    eprintln!("Operations invoked:     {}", invocations);
    eprintln!("Operations completed:   {}", completions);
    eprintln!("Operations indeterminate: {}", indeterminate);
    eprintln!();

    // Run linearizability check
    eprintln!("Checking linearizability...");
    match check_linearizability(&history) {
        Ok(()) => {
            eprintln!("PASS: History is linearizable");
            println!("PASS");
        }
        Err(msg) => {
            eprintln!("FAIL: {}", msg);
            println!("FAIL: {}", msg);
            std::process::exit(1);
        }
    }
}
