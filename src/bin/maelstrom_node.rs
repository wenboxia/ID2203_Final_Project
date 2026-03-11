//! Maelstrom-compatible OmniPaxos node for Jepsen linearizability testing.
//!
//! This binary implements the Maelstrom lin-kv workload protocol, using OmniPaxos
//! for distributed consensus. It communicates via stdin/stdout JSON messages per
//! the Maelstrom protocol specification.
//!
//! Key design decisions for linearizability:
//! - ALL operations (read, write, CAS) go through the OmniPaxos log
//! - Reads are linearizable because they are ordered in the consensus log
//! - CAS is checked at apply-time (when decided), not at propose-time
//! - Only the coordinator node (that received the client request) responds

use std::collections::HashMap;
use std::io::{self, BufRead, Write as IoWrite};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use omnipaxos::macros::Entry;
use omnipaxos::messages::Message as OPMessage;
use omnipaxos::storage::Snapshot;
use omnipaxos::util::LogEntry;
use omnipaxos::{ClusterConfig, OmniPaxos, OmniPaxosConfig, ServerConfig};
use omnipaxos_storage::memory_storage::MemoryStorage;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

// ============================================================================
// OmniPaxos Entry types
// ============================================================================

type NodeId = omnipaxos::util::NodeId;

/// A KV operation that goes through OmniPaxos consensus.
#[derive(Clone, Debug, Entry, Serialize, Deserialize)]
#[snapshot(MaelstromKVSnapshot)]
pub struct MaelstromCommand {
    /// Maelstrom node ID of the node that received the client request
    pub coordinator: String,
    /// Maelstrom client ID (e.g., "c1")
    pub client_src: String,
    /// Client's msg_id for in_reply_to
    pub msg_id: u64,
    /// The KV operation
    pub op: KVOp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum KVOp {
    Read(String),
    Write(String, Value),
    Cas {
        key: String,
        from: Value,
        to: Value,
    },
}

/// Snapshot type for OmniPaxos (required by Entry derive macro).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MaelstromKVSnapshot {
    pub data: HashMap<String, Value>,
}

impl Snapshot<MaelstromCommand> for MaelstromKVSnapshot {
    fn create(entries: &[MaelstromCommand]) -> Self {
        let mut data = HashMap::new();
        for cmd in entries {
            match &cmd.op {
                KVOp::Write(key, value) => {
                    data.insert(key.clone(), value.clone());
                }
                KVOp::Cas { key, from, to } => {
                    let current = data.get(key);
                    if current == Some(from) {
                        data.insert(key.clone(), to.clone());
                    }
                    // If CAS condition fails, state unchanged
                }
                KVOp::Read(_) => {}
            }
        }
        Self { data }
    }

    fn merge(&mut self, delta: Self) {
        for (k, v) in delta.data {
            self.data.insert(k, v);
        }
    }

    fn use_snapshots() -> bool {
        false
    }
}

// ============================================================================
// Maelstrom protocol helpers
// ============================================================================

/// Convert Maelstrom node ID (e.g., "n0") to OmniPaxos NodeId (1-based).
fn maelstrom_to_op_id(s: &str) -> NodeId {
    let n: u64 = s
        .strip_prefix('n')
        .expect("Maelstrom node IDs should start with 'n'")
        .parse()
        .expect("Invalid node ID number");
    n + 1 // OmniPaxos uses 1-based IDs
}

/// Convert OmniPaxos NodeId back to Maelstrom node ID string.
fn op_to_maelstrom_id(id: NodeId) -> String {
    format!("n{}", id - 1)
}

/// Thread-safe stdout writer. All Maelstrom output must go to stdout as JSON lines.
fn send_msg(stdout: &Arc<Mutex<io::Stdout>>, msg: &Value) {
    let mut out = stdout.lock().unwrap();
    serde_json::to_writer(&mut *out, msg).unwrap();
    out.write_all(b"\n").unwrap();
    out.flush().unwrap();
}

/// Send an error response to a Maelstrom client.
fn send_error(stdout: &Arc<Mutex<io::Stdout>>, src: &str, dest: &str, in_reply_to: u64, code: u64, text: &str) {
    send_msg(stdout, &json!({
        "src": src,
        "dest": dest,
        "body": {
            "type": "error",
            "in_reply_to": in_reply_to,
            "code": code,
            "text": text,
        }
    }));
}

// ============================================================================
// Node state
// ============================================================================

struct MaelstromNode {
    /// This node's Maelstrom ID (e.g., "n0")
    node_id: String,
    /// OmniPaxos instance
    omnipaxos: OmniPaxos<MaelstromCommand, MemoryStorage<MaelstromCommand>>,
    /// Local KV store (applied from decided log entries)
    kv_store: HashMap<String, Value>,
    /// Current decided index in the OmniPaxos log
    decided_idx: usize,
    /// Buffer for outgoing OmniPaxos messages
    msg_buffer: Vec<OPMessage<MaelstromCommand>>,
    /// Stdout handle for sending Maelstrom messages
    stdout: Arc<Mutex<io::Stdout>>,
    /// All node IDs in the cluster (Maelstrom format)
    node_ids: Vec<String>,
    /// Pending forwarded requests waiting to be appended (for non-leader nodes)
    /// Maps from (client_src, msg_id) to the original request for retry
    pending_forwards: Vec<MaelstromCommand>,
}

impl MaelstromNode {
    fn new(
        node_id: String,
        node_ids: Vec<String>,
        stdout: Arc<Mutex<io::Stdout>>,
    ) -> Self {
        let my_op_id = maelstrom_to_op_id(&node_id);
        let all_op_ids: Vec<NodeId> = node_ids.iter().map(|s| maelstrom_to_op_id(s)).collect();

        let server_config = ServerConfig {
            pid: my_op_id,
            ..Default::default()
        };
        let cluster_config = ClusterConfig {
            configuration_id: 1,
            nodes: all_op_ids,
            ..Default::default()
        };
        let op_config = OmniPaxosConfig {
            server_config,
            cluster_config,
        };
        let storage = MemoryStorage::default();
        let buffer_size = op_config.server_config.buffer_size;
        let omnipaxos = op_config.build(storage).expect("Failed to build OmniPaxos");

        Self {
            node_id,
            omnipaxos,
            kv_store: HashMap::new(),
            decided_idx: 0,
            msg_buffer: Vec::with_capacity(buffer_size),
            stdout,
            node_ids,
            pending_forwards: Vec::new(),
        }
    }

    /// Handle a client request (read/write/cas) from Maelstrom.
    fn handle_client_request(&mut self, msg: &Value) {
        let msg_type = msg["body"]["type"].as_str().unwrap_or("");
        let client_src = msg["src"].as_str().unwrap().to_string();
        let msg_id = msg["body"]["msg_id"].as_u64().unwrap();
        let key = msg["body"]["key"].as_str().unwrap_or("").to_string();

        let op = match msg_type {
            "read" => KVOp::Read(key),
            "write" => {
                let value = msg["body"]["value"].clone();
                KVOp::Write(key, value)
            }
            "cas" => {
                let from = msg["body"]["from"].clone();
                let to = msg["body"]["to"].clone();
                KVOp::Cas { key, from, to }
            }
            _ => {
                send_error(&self.stdout, &self.node_id, &client_src, msg_id, 10, "unsupported operation");
                return;
            }
        };

        let command = MaelstromCommand {
            coordinator: self.node_id.clone(),
            client_src,
            msg_id,
            op,
        };

        self.try_append(command);
    }

    /// Try to append a command to OmniPaxos. If not leader, forward to leader.
    fn try_append(&mut self, command: MaelstromCommand) {
        match self.omnipaxos.append(command.clone()) {
            Ok(()) => {
                // Successfully appended, will be decided eventually
            }
            Err(_) => {
                // Not the leader or other error. Try to forward to the leader.
                if let Some((leader_id, _)) = self.omnipaxos.get_current_leader() {
                    let leader_str = op_to_maelstrom_id(leader_id);
                    if leader_str != self.node_id {
                        // Forward to leader via Maelstrom message
                        let forward_data = serde_json::to_string(&command).unwrap();
                        send_msg(&self.stdout, &json!({
                            "src": self.node_id,
                            "dest": leader_str,
                            "body": {
                                "type": "forward",
                                "data": forward_data,
                            }
                        }));
                        return;
                    }
                }
                // No leader known or we are the leader but still failed.
                // Buffer for retry on next tick.
                self.pending_forwards.push(command);
            }
        }
    }

    /// Handle a forwarded request from another node.
    fn handle_forwarded_request(&mut self, msg: &Value) {
        let data = msg["body"]["data"].as_str().unwrap();
        let command: MaelstromCommand = serde_json::from_str(data).unwrap();
        self.try_append(command);
    }

    /// Handle an incoming OmniPaxos protocol message from another node.
    fn handle_omnipaxos_message(&mut self, msg: &Value) {
        let data = msg["body"]["data"].as_str().unwrap();
        let op_msg: OPMessage<MaelstromCommand> = serde_json::from_str(data).unwrap();
        self.omnipaxos.handle_incoming(op_msg);
    }

    /// Send all pending outgoing OmniPaxos messages via Maelstrom.
    fn send_outgoing_messages(&mut self) {
        self.omnipaxos.take_outgoing_messages(&mut self.msg_buffer);
        for msg in self.msg_buffer.drain(..) {
            let receiver = msg.get_receiver();
            let dest = op_to_maelstrom_id(receiver);
            let data = serde_json::to_string(&msg).unwrap();
            send_msg(&self.stdout, &json!({
                "src": self.node_id,
                "dest": dest,
                "body": {
                    "type": "omnipaxos",
                    "data": data,
                }
            }));
        }
    }

    /// Process newly decided entries: apply to KV store and respond to clients.
    fn process_decided_entries(&mut self) {
        let new_decided_idx = self.omnipaxos.get_decided_idx();
        if self.decided_idx >= new_decided_idx {
            return;
        }

        let decided_entries = self
            .omnipaxos
            .read_decided_suffix(self.decided_idx)
            .unwrap();
        self.decided_idx = new_decided_idx;

        for entry in decided_entries {
            match entry {
                LogEntry::Decided(cmd) => {
                    self.apply_and_respond(cmd);
                }
                LogEntry::Snapshotted(snapshot) => {
                    // Apply snapshot state
                    self.kv_store = snapshot.snapshot.data.clone();
                }
                _ => {}
            }
        }
    }

    /// Apply a decided command to the KV store and send response if coordinator.
    fn apply_and_respond(&mut self, cmd: MaelstromCommand) {
        let is_coordinator = cmd.coordinator == self.node_id;

        match &cmd.op {
            KVOp::Read(key) => {
                // Linearizable read: executed after consensus ordering
                if is_coordinator {
                    match self.kv_store.get(key) {
                        Some(value) => {
                            send_msg(&self.stdout, &json!({
                                "src": self.node_id,
                                "dest": cmd.client_src,
                                "body": {
                                    "type": "read_ok",
                                    "in_reply_to": cmd.msg_id,
                                    "value": value,
                                }
                            }));
                        }
                        None => {
                            send_error(
                                &self.stdout,
                                &self.node_id,
                                &cmd.client_src,
                                cmd.msg_id,
                                20,
                                "key does not exist",
                            );
                        }
                    }
                }
            }
            KVOp::Write(key, value) => {
                self.kv_store.insert(key.clone(), value.clone());
                if is_coordinator {
                    send_msg(&self.stdout, &json!({
                        "src": self.node_id,
                        "dest": cmd.client_src,
                        "body": {
                            "type": "write_ok",
                            "in_reply_to": cmd.msg_id,
                        }
                    }));
                }
            }
            KVOp::Cas { key, from, to } => {
                let result = match self.kv_store.get(key) {
                    None => Err((20, "key does not exist")),
                    Some(current) if current != from => Err((22, "precondition failed")),
                    Some(_) => {
                        self.kv_store.insert(key.clone(), to.clone());
                        Ok(())
                    }
                };

                if is_coordinator {
                    match result {
                        Ok(()) => {
                            send_msg(&self.stdout, &json!({
                                "src": self.node_id,
                                "dest": cmd.client_src,
                                "body": {
                                    "type": "cas_ok",
                                    "in_reply_to": cmd.msg_id,
                                }
                            }));
                        }
                        Err((code, text)) => {
                            send_error(
                                &self.stdout,
                                &self.node_id,
                                &cmd.client_src,
                                cmd.msg_id,
                                code,
                                text,
                            );
                        }
                    }
                }
            }
        }
    }

    /// Tick OmniPaxos and retry any pending forwarded requests.
    fn tick(&mut self) {
        self.omnipaxos.tick();

        // Retry pending forwards
        let pending = std::mem::take(&mut self.pending_forwards);
        for cmd in pending {
            self.try_append(cmd);
        }
    }
}

// ============================================================================
// Main entry point
// ============================================================================

#[tokio::main]
async fn main() {
    // Set up stderr logging (Maelstrom captures stderr for debug)
    eprintln!("Maelstrom OmniPaxos node starting...");

    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();

    // Spawn blocking stdin reader thread
    std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(line) if !line.is_empty() => {
                    if stdin_tx.send(line).is_err() {
                        break;
                    }
                }
                Ok(_) => continue,
                Err(e) => {
                    eprintln!("stdin read error: {}", e);
                    break;
                }
            }
        }
    });

    let stdout = Arc::new(Mutex::new(io::stdout()));

    // ---- Wait for init message ----
    let init_line = stdin_rx
        .recv()
        .await
        .expect("Expected init message on stdin");
    let init_msg: Value = serde_json::from_str(&init_line).expect("Invalid JSON for init");

    let my_node_id = init_msg["body"]["node_id"]
        .as_str()
        .expect("Missing node_id in init")
        .to_string();
    let node_ids: Vec<String> = init_msg["body"]["node_ids"]
        .as_array()
        .expect("Missing node_ids in init")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();

    eprintln!("Node {} initialized with cluster {:?}", my_node_id, node_ids);

    // Send init_ok
    send_msg(
        &stdout,
        &json!({
            "src": my_node_id,
            "dest": init_msg["src"].as_str().unwrap(),
            "body": {
                "type": "init_ok",
                "in_reply_to": init_msg["body"]["msg_id"],
            }
        }),
    );

    // ---- Create the node ----
    let mut node = MaelstromNode::new(my_node_id.clone(), node_ids.clone(), stdout.clone());

    // Node n0 (OmniPaxos ID 1) tries to become leader initially
    if my_node_id == "n0" {
        let _ = node.omnipaxos.try_become_leader();
        node.send_outgoing_messages();
    }

    // ---- Main event loop ----
    let mut tick_interval = tokio::time::interval(Duration::from_millis(100));

    loop {
        tokio::select! {
            Some(line) = stdin_rx.recv() => {
                let msg: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("Failed to parse message: {} - {}", e, line);
                        continue;
                    }
                };

                let msg_type = msg["body"]["type"].as_str().unwrap_or("");

                match msg_type {
                    "read" | "write" | "cas" => {
                        node.handle_client_request(&msg);
                    }
                    "omnipaxos" => {
                        node.handle_omnipaxos_message(&msg);
                    }
                    "forward" => {
                        node.handle_forwarded_request(&msg);
                    }
                    other => {
                        eprintln!("Unknown message type: {}", other);
                    }
                }

                // After handling any message, check for decided entries and send outgoing
                node.send_outgoing_messages();
                node.process_decided_entries();
            }
            _ = tick_interval.tick() => {
                node.tick();
                node.send_outgoing_messages();
                node.process_decided_entries();
            }
        }
    }
}
