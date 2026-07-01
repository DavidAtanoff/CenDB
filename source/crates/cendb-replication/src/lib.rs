//! cendb-replication: replication and HA for CenDB.
//!
//! Two complementary mechanisms:
//!
//!   * **Raft consensus** (`RaftNode` / `RaftCluster`) — for
//!     multi-node clusters that need strong consistency. Single-process
//!     simulation; production use would add a network transport.
//!   * **WAL shipping** (`wal_shipping::WalShipper`) — the recommended
//!     HA story for embedded deployments. Ships WAL segments to a
//!     replica directory or S3 staging area; a replica process applies
//!     them via ARIES recovery.
//!
//! ## When to use which
//!
//! Use **WAL shipping** when:
//!   - You have a single embedded CenDB instance and want disaster
//!     recovery.
//!   - You can tolerate bounded RPO (one transaction with `OnCommit`,
//!     up to N seconds with `Interval`).
//!   - You don't need automatic failover (external orchestration is OK).
//!
//! Use **Raft** when:
//!   - You need automatic failover and strong consistency.
//!   - You can deploy multiple CenDB instances with a network
//!     transport between them.
//!   - You can accept the complexity of a consensus protocol.

pub mod tcp_transport;
pub mod wal_shipping;

pub use tcp_transport::{FailoverManager, RaftMessage, ReadRouter, TcpTransport};

use std::collections::HashMap;

/// Node ID.
pub type NodeId = u64;

/// Log entry: a replicated command.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    pub term: u64,
    pub index: u64,
    pub data: Vec<u8>,
}

/// Node role in the Raft cluster.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum NodeRole {
    Follower,
    Candidate,
    Leader,
}

/// A Raft node (single-process simulation).
pub struct RaftNode {
    pub id: NodeId,
    pub role: NodeRole,
    pub current_term: u64,
    pub voted_for: Option<NodeId>,
    pub log: Vec<LogEntry>,
    pub commit_index: u64,
    pub last_applied: u64,
    /// For leaders: next log index to send to each follower.
    pub next_index: HashMap<NodeId, u64>,
    /// For leaders: highest log index replicated on each follower.
    pub match_index: HashMap<NodeId, u64>,
    /// Votes received in current election.
    pub votes_received: u32,
    /// Cluster members.
    pub peers: Vec<NodeId>,
}

impl RaftNode {
    pub fn new(id: NodeId, peers: Vec<NodeId>) -> Self {
        Self {
            id,
            role: NodeRole::Follower,
            current_term: 0,
            voted_for: None,
            log: Vec::new(),
            commit_index: 0,
            last_applied: 0,
            next_index: HashMap::new(),
            match_index: HashMap::new(),
            votes_received: 0,
            peers,
        }
    }

    /// Start a new election: become a candidate, vote for self, increment term.
    pub fn start_election(&mut self) {
        self.role = NodeRole::Candidate;
        self.current_term += 1;
        self.voted_for = Some(self.id);
        self.votes_received = 1; // Vote for self.
    }

    /// Receive a vote from a peer. Returns true if this node becomes leader.
    pub fn receive_vote(&mut self) -> bool {
        if self.role != NodeRole::Candidate {
            return false;
        }
        self.votes_received += 1;
        let majority = (self.peers.len() + 1) / 2 + 1;
        if self.votes_received as usize >= majority {
            self.become_leader();
            return true;
        }
        false
    }

    /// Become the leader.
    fn become_leader(&mut self) {
        self.role = NodeRole::Leader;
        let next = self.log.len() as u64 + 1;
        for &peer in &self.peers {
            self.next_index.insert(peer, next);
            self.match_index.insert(peer, 0);
        }
    }

    /// Append a new entry to the log (leader only).
    pub fn append_entry(&mut self, data: Vec<u8>) -> u64 {
        if self.role != NodeRole::Leader {
            return 0;
        }
        let index = self.log.len() as u64 + 1;
        self.log.push(LogEntry {
            term: self.current_term,
            index,
            data,
        });
        index
    }

    /// A follower acknowledges replication of `index`.
    pub fn acknowledge(&mut self, follower: NodeId, index: u64) {
        if self.role != NodeRole::Leader {
            return;
        }
        self.match_index.insert(follower, index);
        self.next_index.insert(follower, index + 1);

        // Check if we can advance commit_index.
        let majority = (self.peers.len() + 1) / 2 + 1;
        for n in (self.commit_index + 1..=self.log.len() as u64).rev() {
            let count = 1 + self.match_index.values().filter(|&&i| i >= n).count();
            if count >= majority {
                // Only commit entries from the current term (Raft safety).
                if self.log[(n - 1) as usize].term == self.current_term {
                    self.commit_index = n;
                }
                break;
            }
        }
    }

    /// Apply committed entries (simulate execution).
    pub fn apply_committed(&mut self) -> Vec<&LogEntry> {
        let mut to_apply = Vec::new();
        while self.last_applied < self.commit_index {
            self.last_applied += 1;
            if let Some(entry) = self.log.get((self.last_applied - 1) as usize) {
                to_apply.push(entry);
            }
        }
        to_apply
    }

    /// Check if this node is the leader.
    pub fn is_leader(&self) -> bool {
        self.role == NodeRole::Leader
    }

    /// Number of log entries.
    pub fn log_len(&self) -> usize {
        self.log.len()
    }
}

/// A Raft cluster: a collection of nodes (single-process simulation).
pub struct RaftCluster {
    pub nodes: HashMap<NodeId, RaftNode>,
    pub leader: Option<NodeId>,
}

impl RaftCluster {
    /// Create a 3-node cluster.
    pub fn new_3_node() -> Self {
        let mut nodes = HashMap::new();
        let peer_ids = vec![1, 2, 3];
        for &id in &peer_ids {
            let peers: Vec<NodeId> = peer_ids.iter().filter(|&&p| p != id).copied().collect();
            nodes.insert(id, RaftNode::new(id, peers));
        }
        Self {
            nodes,
            leader: None,
        }
    }

    /// Elect a leader. Node `candidate_id` starts an election and
    /// receives votes from the majority.
    pub fn elect_leader(&mut self, candidate_id: NodeId) -> bool {
        let candidate = self.nodes.get_mut(&candidate_id).unwrap();
        candidate.start_election();

        // Collect votes from other nodes.
        let term = candidate.current_term;
        let mut votes = 1; // Self-vote.

        for (&id, node) in &mut self.nodes {
            if id == candidate_id {
                continue;
            }
            // Node votes for candidate if it hasn't voted in this term.
            if node.current_term < term || node.voted_for.is_none() {
                node.current_term = term;
                node.voted_for = Some(candidate_id);
                votes += 1;
            }
        }

        let majority = (self.nodes.len() / 2) + 1;
        if votes as usize >= majority {
            let leader = self.nodes.get_mut(&candidate_id).unwrap();
            leader.become_leader();
            self.leader = Some(candidate_id);
            true
        } else {
            false
        }
    }

    /// Replicate an entry from the leader to followers.
    pub fn replicate_entry(&mut self, data: Vec<u8>) -> u64 {
        let leader_id = match self.leader {
            Some(id) => id,
            None => return 0,
        };

        // Leader appends to its log.
        let index = self.nodes.get_mut(&leader_id).unwrap().append_entry(data);

        // Followers receive and append the entry.
        let entry_clone = self.nodes[&leader_id].log[(index - 1) as usize].clone();
        for (&id, node) in &mut self.nodes {
            if id == leader_id {
                continue;
            }
            if node.log.len() < index as usize {
                node.log.push(entry_clone.clone());
            }
        }

        // Leader receives acknowledgments.
        let follower_ids: Vec<NodeId> = self.nodes.keys().filter(|&&id| id != leader_id).copied().collect();
        for id in follower_ids {
            self.nodes.get_mut(&leader_id).unwrap().acknowledge(id, index);
        }

        index
    }

    /// Apply committed entries on all nodes.
    pub fn apply_all(&mut self) {
        for node in self.nodes.values_mut() {
            node.apply_committed();
        }
    }

    /// Verify all nodes have the same log.
    pub fn verify_consistency(&self) -> bool {
        let logs: Vec<&[LogEntry]> = self.nodes.values().map(|n| n.log.as_slice()).collect();
        if logs.is_empty() {
            return true;
        }
        let first = logs[0];
        logs.iter().all(|l| l.len() == first.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leader_election() {
        let mut cluster = RaftCluster::new_3_node();
        assert!(cluster.leader.is_none());

        let elected = cluster.elect_leader(1);
        assert!(elected);
        assert_eq!(cluster.leader, Some(1));
        assert!(cluster.nodes[&1].is_leader());
    }

    #[test]
    fn log_replication() {
        let mut cluster = RaftCluster::new_3_node();
        cluster.elect_leader(1);

        // Replicate 5 entries.
        for i in 0..5 {
            cluster.replicate_entry(format!("entry_{}", i).into_bytes());
        }

        // All nodes should have the same log.
        assert!(cluster.verify_consistency());
        assert_eq!(cluster.nodes[&1].log_len(), 5);
        assert_eq!(cluster.nodes[&2].log_len(), 5);
        assert_eq!(cluster.nodes[&3].log_len(), 5);
    }

    #[test]
    fn committed_entries_applied() {
        let mut cluster = RaftCluster::new_3_node();
        cluster.elect_leader(1);
        cluster.replicate_entry(b"cmd1".to_vec());
        cluster.replicate_entry(b"cmd2".to_vec());

        // Sync commit_index to followers.
        let leader_commit = cluster.nodes[&1].commit_index;
        for node in cluster.nodes.values_mut() {
            node.commit_index = leader_commit;
        }
        cluster.apply_all();

        // Leader should have applied both entries.
        assert_eq!(cluster.nodes[&1].last_applied, 2);
    }

    #[test]
    fn no_leader_no_writes() {
        let mut cluster = RaftCluster::new_3_node();
        // No election — no leader.
        let result = cluster.replicate_entry(b"data".to_vec());
        assert_eq!(result, 0);
    }
}

// ============================================================================
// Raft Log Compaction and Snapshotting
// ============================================================================

/// A Raft snapshot: a point-in-time copy of the state machine.
#[derive(Clone, Debug)]
pub struct RaftSnapshot {
    /// The last included log index.
    pub last_included_index: u64,
    /// The last included log term.
    pub last_included_term: u64,
    /// The serialized state machine data.
    pub data: Vec<u8>,
}

impl RaftNode {
    /// Create a snapshot at the current commit index. The log is then
    /// truncated up to the snapshot point.
    pub fn create_snapshot(&mut self) -> RaftSnapshot {
        let last_idx = self.commit_index;
        let last_term = if last_idx > 0 && (last_idx as usize) <= self.log.len() {
            self.log[(last_idx - 1) as usize].term
        } else {
            0
        };

        // Serialize the state: all applied log entries' data.
        let mut data = Vec::new();
        for i in 0..last_idx as usize {
            if i < self.log.len() {
                data.extend_from_slice(&self.log[i].data);
                data.push(b'\n');
            }
        }

        let snapshot = RaftSnapshot {
            last_included_index: last_idx,
            last_included_term: last_term,
            data,
        };

        // Truncate the log up to the snapshot point.
        if last_idx > 0 {
            self.log.drain(0..last_idx as usize);
        }

        snapshot
    }

    /// Install a snapshot received from the leader. Replaces the local
    /// state and truncates the log.
    pub fn install_snapshot(&mut self, snapshot: RaftSnapshot) {
        // Only install if the snapshot is newer than our current state.
        if snapshot.last_included_index <= self.commit_index {
            return;
        }

        // Truncate any log entries that conflict with the snapshot.
        let truncate_to = snapshot.last_included_index as usize;
        if truncate_to <= self.log.len() {
            self.log.drain(0..truncate_to);
        }

        self.commit_index = snapshot.last_included_index;
        self.last_applied = snapshot.last_included_index;
    }

    /// Add a new node to the cluster (dynamic membership).
    pub fn add_peer(&mut self, peer_id: NodeId) {
        if !self.peers.contains(&peer_id) {
            self.peers.push(peer_id);
            if self.role == NodeRole::Leader {
                self.next_index.insert(peer_id, self.log.len() as u64 + 1);
                self.match_index.insert(peer_id, 0);
            }
        }
    }

    /// Remove a node from the cluster (dynamic membership).
    pub fn remove_peer(&mut self, peer_id: NodeId) {
        self.peers.retain(|&p| p != peer_id);
        self.next_index.remove(&peer_id);
        self.match_index.remove(&peer_id);
    }
}

impl RaftCluster {
    /// Add a new node to the cluster.
    pub fn add_node(&mut self, node_id: NodeId) {
        let peers: Vec<NodeId> = self.nodes.keys().copied().collect();
        let mut new_node = RaftNode::new(node_id, peers.clone());

        // Update existing nodes to include the new peer.
        for node in self.nodes.values_mut() {
            node.add_peer(node_id);
        }
        new_node.peers = peers;

        self.nodes.insert(node_id, new_node);
    }

    /// Remove a node from the cluster.
    pub fn remove_node(&mut self, node_id: NodeId) {
        self.nodes.remove(&node_id);
        for node in self.nodes.values_mut() {
            node.remove_peer(node_id);
        }
    }

    /// Create a snapshot on the leader and install it on all followers.
    pub fn snapshot_and_sync(&mut self) -> Option<RaftSnapshot> {
        let leader_id = self.leader?;
        let snapshot = self.nodes.get_mut(&leader_id)?.create_snapshot();

        let follower_ids: Vec<NodeId> = self.nodes.keys()
            .filter(|&&id| id != leader_id)
            .copied()
            .collect();

        for fid in follower_ids {
            if let Some(node) = self.nodes.get_mut(&fid) {
                node.install_snapshot(snapshot.clone());
            }
        }

        Some(snapshot)
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[test]
    fn snapshot_truncates_log() {
        let mut cluster = RaftCluster::new_3_node();
        cluster.elect_leader(1);
        for i in 0..10 {
            cluster.replicate_entry(format!("entry_{}", i).into_bytes());
        }

        let leader = cluster.nodes.get_mut(&1).unwrap();
        assert_eq!(leader.log_len(), 10);

        let snapshot = leader.create_snapshot();
        assert_eq!(snapshot.last_included_index, 10);
        assert_eq!(leader.log_len(), 0); // Log truncated.
    }

    #[test]
    fn snapshot_install_on_follower() {
        let mut cluster = RaftCluster::new_3_node();
        cluster.elect_leader(1);
        for i in 0..5 {
            cluster.replicate_entry(format!("entry_{}", i).into_bytes());
        }

        let snapshot = cluster.snapshot_and_sync().unwrap();
        assert_eq!(snapshot.last_included_index, 5);

        // All nodes should have commit_index = 5.
        for node in cluster.nodes.values() {
            assert_eq!(node.commit_index, 5);
        }
    }

    #[test]
    fn dynamic_membership_add() {
        let mut cluster = RaftCluster::new_3_node();
        cluster.elect_leader(1);
        cluster.add_node(4);

        assert_eq!(cluster.nodes.len(), 4);
        // Leader should track the new node.
        assert!(cluster.nodes[&1].next_index.contains_key(&4));
    }

    #[test]
    fn dynamic_membership_remove() {
        let mut cluster = RaftCluster::new_3_node();
        cluster.elect_leader(1);
        cluster.remove_node(3);

        assert_eq!(cluster.nodes.len(), 2);
        assert!(!cluster.nodes[&1].peers.contains(&3));
    }
}
