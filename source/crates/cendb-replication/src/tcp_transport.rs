//! Raft TCP transport — real network communication between Raft nodes.
//!
//! Enables multi-process Raft clusters where nodes communicate over TCP.
//! Each node listens on a port and connects to peers.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::{NodeId, LogEntry, NodeRole};

/// Message types exchanged between Raft nodes.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum RaftMessage {
    /// Request vote from candidate.
    VoteRequest { term: u64, candidate_id: NodeId, last_log_index: u64, last_log_term: u64 },
    /// Response to vote request.
    VoteResponse { term: u64, vote_granted: bool },
    /// Append entries (heartbeat or log replication).
    AppendEntriesRequest {
        term: u64,
        leader_id: NodeId,
        prev_log_index: u64,
        prev_log_term: u64,
        entries: Vec<LogEntry>,
        leader_commit: u64,
    },
    /// Response to append entries.
    AppendEntriesResponse { term: u64, success: bool, match_index: u64 },
    /// Leader promotion notification.
    LeaderPromotion { node_id: NodeId, term: u64 },
}

impl RaftMessage {
    /// Serialize to a simple length-prefixed binary format.
    fn serialize(&self) -> Vec<u8> {
        let json = serde_json::to_string(self).unwrap_or_default();
        let bytes = json.into_bytes();
        let len = bytes.len() as u32;
        let mut out = len.to_le_bytes().to_vec();
        out.extend_from_slice(&bytes);
        out
    }

    /// Deserialize from bytes.
    fn deserialize(data: &[u8]) -> Option<Self> {
        serde_json::from_slice(data).ok()
    }
}

/// Address of a Raft peer.
#[derive(Clone, Debug)]
pub struct PeerAddr {
    pub node_id: NodeId,
    pub addr: String, // e.g. "127.0.0.1:8080"
}

/// TCP transport for Raft. Manages connections to peers and a listener
/// for incoming messages.
pub struct TcpTransport {
    node_id: NodeId,
    listen_addr: String,
    peers: Arc<Mutex<HashMap<NodeId, String>>>,
    listener: Option<thread::JoinHandle<()>>,
    incoming: Arc<Mutex<Vec<(NodeId, RaftMessage)>>>,
}

impl TcpTransport {
    /// Create a new TCP transport. Does NOT start listening yet.
    pub fn new(node_id: NodeId, listen_addr: String) -> Self {
        Self {
            node_id,
            listen_addr,
            peers: Arc::new(Mutex::new(HashMap::new())),
            listener: None,
            incoming: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Add a peer address.
    pub fn add_peer(&self, node_id: NodeId, addr: String) {
        self.peers.lock().unwrap().insert(node_id, addr);
    }

    /// Remove a peer.
    pub fn remove_peer(&self, node_id: NodeId) {
        self.peers.lock().unwrap().remove(&node_id);
    }

    /// Start listening for incoming connections. Spawns a background thread.
    pub fn start(&mut self) -> std::io::Result<()> {
        let addr = self.listen_addr.clone();
        let incoming = Arc::clone(&self.incoming);
        let node_id = self.node_id;

        let listener = TcpListener::bind(&addr)?;
        listener.set_nonblocking(false).ok();

        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(mut s) => {
                        if let Ok(msg) = read_message(&mut s) {
                            incoming.lock().unwrap().push((node_id, msg));
                        }
                    }
                    Err(e) => {
                        eprintln!("Raft listener error: {}", e);
                    }
                }
            }
        });

        self.listener = Some(handle);
        Ok(())
    }

    /// Send a message to a peer. Returns Ok(()) if the message was sent.
    pub fn send(&self, to: NodeId, msg: RaftMessage) -> Result<(), String> {
        let addr = {
            let peers = self.peers.lock().unwrap();
            peers.get(&to).cloned().ok_or_else(|| format!("unknown peer {}", to))?
        };

        let mut stream = TcpStream::connect_timeout(
            &addr.parse().map_err(|e| format!("bad addr {}: {}", addr, e))?,
            Duration::from_secs(5),
        ).map_err(|e| format!("connect to {} failed: {}", addr, e))?;

        let data = msg.serialize();
        stream.write_all(&data).map_err(|e| format!("write failed: {}", e))?;
        stream.flush().map_err(|e| format!("flush failed: {}", e))?;
        Ok(())
    }

    /// Poll for incoming messages. Returns all messages received since
    /// the last poll.
    pub fn poll_incoming(&self) -> Vec<(NodeId, RaftMessage)> {
        let mut incoming = self.incoming.lock().unwrap();
        let msgs = incoming.drain(..).collect();
        msgs
    }

    /// Stop the transport.
    pub fn stop(&mut self) {
        // Dropping the listener handle will end the thread on the next
        // connection attempt (which won't come since we're stopping).
        if let Some(handle) = self.listener.take() {
            drop(handle);
        }
    }

    /// This node's ID.
    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    /// This node's listen address.
    pub fn listen_addr(&self) -> &str {
        &self.listen_addr
    }

    /// List of peer node IDs.
    pub fn peer_ids(&self) -> Vec<NodeId> {
        self.peers.lock().unwrap().keys().copied().collect()
    }
}

impl Drop for TcpTransport {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Read a length-prefixed message from a TCP stream.
fn read_message(stream: &mut TcpStream) -> std::io::Result<RaftMessage> {
    // Read 4-byte length prefix.
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;

    // Read the message body.
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body)?;

    // Deserialize.
    RaftMessage::deserialize(&body)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "deserialize failed"))
}

/// Automatic failover manager. Monitors the leader and triggers
/// promotion of a replica when the leader is unreachable.
pub struct FailoverManager {
    transport: Arc<TcpTransport>,
    leader_id: Arc<Mutex<Option<NodeId>>>,
    heartbeat_timeout: Duration,
    last_heartbeat: Arc<Mutex<std::time::Instant>>,
}

impl FailoverManager {
    pub fn new(transport: Arc<TcpTransport>, heartbeat_timeout: Duration) -> Self {
        Self {
            transport,
            leader_id: Arc::new(Mutex::new(None)),
            heartbeat_timeout,
            last_heartbeat: Arc::new(Mutex::new(std::time::Instant::now())),
        }
    }

    /// Set the current leader.
    pub fn set_leader(&self, leader_id: NodeId) {
        *self.leader_id.lock().unwrap() = Some(leader_id);
        *self.last_heartbeat.lock().unwrap() = std::time::Instant::now();
    }

    /// Record a heartbeat from the leader.
    pub fn record_heartbeat(&self) {
        *self.last_heartbeat.lock().unwrap() = std::time::Instant::now();
    }

    /// Check if the leader has timed out and failover is needed.
    pub fn check_failover(&self) -> bool {
        let elapsed = self.last_heartbeat.lock().unwrap().elapsed();
        elapsed > self.heartbeat_timeout
    }

    /// Get the current leader (if any).
    pub fn leader(&self) -> Option<NodeId> {
        *self.leader_id.lock().unwrap()
    }
}

/// Read router: distributes read traffic across replicas.
pub struct ReadRouter {
    replicas: Arc<Mutex<Vec<NodeId>>>,
    next: Arc<Mutex<usize>>,
}

impl ReadRouter {
    pub fn new(replicas: Vec<NodeId>) -> Self {
        Self {
            replicas: Arc::new(Mutex::new(replicas)),
            next: Arc::new(Mutex::new(0)),
        }
    }

    /// Add a replica to the pool.
    pub fn add_replica(&self, node_id: NodeId) {
        self.replicas.lock().unwrap().push(node_id);
    }

    /// Remove a replica from the pool.
    pub fn remove_replica(&self, node_id: NodeId) {
        self.replicas.lock().unwrap().retain(|&r| r != node_id);
    }

    /// Get the next replica (round-robin).
    pub fn next_replica(&self) -> Option<NodeId> {
        let replicas = self.replicas.lock().unwrap();
        if replicas.is_empty() {
            return None;
        }
        let mut next = self.next.lock().unwrap();
        let replica = replicas[*next % replicas.len()];
        *next += 1;
        Some(replica)
    }

    /// Number of available replicas.
    pub fn replica_count(&self) -> usize {
        self.replicas.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_router_round_robin() {
        let router = ReadRouter::new(vec![1, 2, 3]);
        assert_eq!(router.next_replica(), Some(1));
        assert_eq!(router.next_replica(), Some(2));
        assert_eq!(router.next_replica(), Some(3));
        assert_eq!(router.next_replica(), Some(1)); // wraps around
    }

    #[test]
    fn read_router_add_remove() {
        let router = ReadRouter::new(vec![1, 2]);
        assert_eq!(router.replica_count(), 2);
        router.add_replica(3);
        assert_eq!(router.replica_count(), 3);
        router.remove_replica(2);
        assert_eq!(router.replica_count(), 2);
    }

    #[test]
    fn read_router_empty() {
        let router = ReadRouter::new(vec![]);
        assert_eq!(router.next_replica(), None);
    }

    #[test]
    fn failover_manager_timeout() {
        let transport = Arc::new(TcpTransport::new(1, "127.0.0.1:0".to_string()));
        let mgr = FailoverManager::new(transport, Duration::from_millis(10));
        mgr.set_leader(2);
        assert!(!mgr.check_failover());
        thread::sleep(Duration::from_millis(20));
        assert!(mgr.check_failover());
    }

    #[test]
    fn failover_manager_heartbeat_resets() {
        let transport = Arc::new(TcpTransport::new(1, "127.0.0.1:0".to_string()));
        let mgr = FailoverManager::new(transport, Duration::from_millis(50));
        mgr.set_leader(2);
        thread::sleep(Duration::from_millis(30));
        mgr.record_heartbeat(); // reset
        thread::sleep(Duration::from_millis(30));
        assert!(!mgr.check_failover()); // still within timeout
    }

    #[test]
    fn tcp_transport_peer_management() {
        let transport = TcpTransport::new(1, "127.0.0.1:0".to_string());
        transport.add_peer(2, "127.0.0.1:8081".to_string());
        transport.add_peer(3, "127.0.0.1:8082".to_string());
        assert_eq!(transport.peer_ids().len(), 2);
        transport.remove_peer(2);
        assert_eq!(transport.peer_ids().len(), 1);
    }
}
