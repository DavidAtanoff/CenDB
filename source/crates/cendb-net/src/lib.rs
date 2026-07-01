//! cendb-net: network transport layer for P2P replication.
//!
//! ## Design
//!
//! Provides a message-passing abstraction over TCP for Raft-based
//! replication. Each node runs a listener that accepts connections from
//! peers and dispatches messages to the replication engine.
//!
//! ## Protocol
//!
//! Messages are length-prefixed binary frames:
//! ```text
//! [magic 4B][msg_type 1B][payload_len 4B][payload]
//! ```
//!
//! Message types:
//!   * `VoteRequest` — leader election.
//!   * `VoteResponse` — vote grant/deny.
//!   * `AppendEntries` — log replication.
//!   * `AppendEntriesResponse` — acknowledgment.
//!   * `Snapshot` — Raft snapshot transfer.
//!   * `JoinCluster` — dynamic membership add.
//!   * `LeaveCluster` — dynamic membership remove.
//!
//! For the prototype, the transport is simulated in-process; production
//! would use `tokio` + `tcp` for real network I/O.

use std::collections::HashMap;
use cendb_replication::{NodeId, LogEntry};

/// Network message types.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum MsgType {
    VoteRequest = 1,
    VoteResponse = 2,
    AppendEntries = 3,
    AppendEntriesResponse = 4,
    Snapshot = 5,
    JoinCluster = 6,
    LeaveCluster = 7,
}

impl MsgType {
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::VoteRequest),
            2 => Some(Self::VoteResponse),
            3 => Some(Self::AppendEntries),
            4 => Some(Self::AppendEntriesResponse),
            5 => Some(Self::Snapshot),
            6 => Some(Self::JoinCluster),
            7 => Some(Self::LeaveCluster),
            _ => None,
        }
    }
}

/// A network message.
#[derive(Clone, Debug)]
pub struct Message {
    pub msg_type: MsgType,
    pub from: NodeId,
    pub to: NodeId,
    pub term: u64,
    pub payload: Vec<u8>,
}

/// Vote request payload.
#[derive(Clone, Debug)]
pub struct VoteRequest {
    pub candidate_id: NodeId,
    pub term: u64,
    pub last_log_index: u64,
    pub last_log_term: u64,
}

/// Vote response payload.
#[derive(Clone, Debug)]
pub struct VoteResponse {
    pub term: u64,
    pub vote_granted: bool,
}

/// Append entries payload (log replication).
#[derive(Clone, Debug)]
pub struct AppendEntries {
    pub leader_id: NodeId,
    pub term: u64,
    pub prev_log_index: u64,
    pub prev_log_term: u64,
    pub entries: Vec<LogEntry>,
    pub leader_commit: u64,
}

/// Append entries response.
#[derive(Clone, Debug)]
pub struct AppendEntriesResponse {
    pub term: u64,
    pub success: bool,
    pub match_index: u64,
}

/// Serialize a message to bytes.
pub fn serialize_message(msg: &Message) -> Vec<u8> {
    let mut out = Vec::with_capacity(13 + msg.payload.len());
    out.extend_from_slice(b"CNR1"); // magic
    out.push(msg.msg_type as u8);
    out.extend_from_slice(&(msg.payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&msg.from.to_le_bytes());
    out.extend_from_slice(&msg.to.to_le_bytes());
    out.extend_from_slice(&msg.term.to_le_bytes());
    out.extend_from_slice(&msg.payload);
    out
}

/// Deserialize a message from bytes.
pub fn deserialize_message(bytes: &[u8]) -> Option<Message> {
    // Format: [magic 4B][msg_type 1B][payload_len 4B][from 8B][to 8B][term 8B][payload]
    let min_len = 4 + 1 + 4 + 8 + 8 + 8;
    if bytes.len() < min_len {
        return None;
    }
    if &bytes[..4] != b"CNR1" {
        return None;
    }
    let msg_type = MsgType::from_u8(bytes[4])?;
    let payload_len = u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) as usize;
    let total_needed = min_len + payload_len;
    if bytes.len() < total_needed {
        return None;
    }
    let from = u64::from_le_bytes([
        bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15], bytes[16],
    ]);
    let to = u64::from_le_bytes([
        bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23], bytes[24],
    ]);
    let term = u64::from_le_bytes([
        bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31], bytes[32],
    ]);
    let payload = bytes[33..33 + payload_len].to_vec();
    Some(Message {
        msg_type,
        from,
        to,
        term,
        payload,
    })
}

/// In-process network simulator: routes messages between nodes without
/// real network I/O. Useful for testing the replication protocol.
pub struct InProcessNetwork {
    /// Mailbox for each node: messages waiting to be delivered.
    mailboxes: HashMap<NodeId, Vec<Message>>,
    /// Whether the network is partitioned between specific node pairs.
    partitions: Vec<(NodeId, NodeId)>,
}

impl InProcessNetwork {
    pub fn new() -> Self {
        Self {
            mailboxes: HashMap::new(),
            partitions: Vec::new(),
        }
    }

    /// Send a message (non-blocking; delivered to the recipient's mailbox).
    pub fn send(&mut self, msg: Message) {
        // Check for network partition.
        if self.is_partitioned(msg.from, msg.to) {
            return; // Message dropped (simulated network partition).
        }
        self.mailboxes.entry(msg.to).or_default().push(msg);
    }

    /// Receive a message for a node (non-blocking).
    pub fn recv(&mut self, node: NodeId) -> Option<Message> {
        self.mailboxes.get_mut(&node).and_then(|m| {
            if m.is_empty() {
                None
            } else {
                Some(m.remove(0))
            }
        })
    }

    /// Simulate a network partition between two nodes.
    pub fn add_partition(&mut self, a: NodeId, b: NodeId) {
        self.partitions.push((a, b));
        self.partitions.push((b, a));
    }

    /// Heal a network partition.
    pub fn heal_partition(&mut self, a: NodeId, b: NodeId) {
        self.partitions.retain(|&(x, y)| (x, y) != (a, b) && (x, y) != (b, a));
    }

    /// Check if two nodes are partitioned.
    fn is_partitioned(&self, a: NodeId, b: NodeId) -> bool {
        self.partitions.contains(&(a, b))
    }

    /// Number of pending messages for a node.
    pub fn pending_count(&self, node: NodeId) -> usize {
        self.mailboxes.get(&node).map(|m| m.len()).unwrap_or(0)
    }
}

impl Default for InProcessNetwork {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_serialize_deserialize() {
        let msg = Message {
            msg_type: MsgType::VoteRequest,
            from: 1,
            to: 2,
            term: 5,
            payload: vec![1, 2, 3],
        };
        let bytes = serialize_message(&msg);
        let msg2 = deserialize_message(&bytes).unwrap();
        assert_eq!(msg.msg_type, msg2.msg_type);
        assert_eq!(msg.from, msg2.from);
        assert_eq!(msg.to, msg2.to);
        assert_eq!(msg.term, msg2.term);
        assert_eq!(msg.payload, msg2.payload);
    }

    #[test]
    fn in_process_network_basic() {
        let mut net = InProcessNetwork::new();
        net.send(Message {
            msg_type: MsgType::VoteRequest,
            from: 1,
            to: 2,
            term: 1,
            payload: vec![],
        });
        assert_eq!(net.pending_count(2), 1);
        let msg = net.recv(2).unwrap();
        assert_eq!(msg.msg_type, MsgType::VoteRequest);
        assert_eq!(net.pending_count(2), 0);
    }

    #[test]
    fn network_partition_drops_messages() {
        let mut net = InProcessNetwork::new();
        net.add_partition(1, 2);
        net.send(Message {
            msg_type: MsgType::AppendEntries,
            from: 1,
            to: 2,
            term: 1,
            payload: vec![],
        });
        assert_eq!(net.pending_count(2), 0); // Message dropped.
    }

    #[test]
    fn heal_partition_restores_delivery() {
        let mut net = InProcessNetwork::new();
        net.add_partition(1, 2);
        net.heal_partition(1, 2);
        net.send(Message {
            msg_type: MsgType::AppendEntries,
            from: 1,
            to: 2,
            term: 1,
            payload: vec![],
        });
        assert_eq!(net.pending_count(2), 1); // Message delivered.
    }
}
