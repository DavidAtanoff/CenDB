//! Graph projection: nodes + edges stored as PAX blocks, plus an in-memory
//! Compressed Sparse Row (CSR) overlay for O(1) neighbor enumeration.
//!
//! ## Layout
//!
//! ```text
//! ┌─────────────────────────────────────────────────┐
//! │ Nodes PAX block(s): (node_id, label, props)     │
//! ├─────────────────────────────────────────────────┤
//! │ Edges PAX block(s): (src, dst, type, props)     │
//! │  - sorted by (src, type)                        │
//! ├─────────────────────────────────────────────────┤
//! │ CSR overlay (in memory):                        │
//! │   offsets:   Vec<u64>     len = N+1             │
//! │   adjacency: Vec<NodeId>  len = E               │
//! │   edge_refs: Vec<EdgeRef> len = E (parallel)    │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! The CSR overlay is the spec's "index-free adjacency" mechanism: given
//! a node `u`, `adjacency[offsets[u]..offsets[u+1]]` is the list of `u`'s
//! out-neighbors — a contiguous slice, no index lookup needed. This gives
//! BFS/DFS the pointer-chasing locality of a native graph DB while edge
//! *properties* still live in the columnar substrate (so
//! `WHERE edge.weight > 5` is a columnar predicate).

use std::collections::HashMap;

use cendb_core::{BlockId, CenError, CenResult, NodeId, SegmentId, Value, ValueKind};
use cendb_storage::header::ColumnSpec;
use cendb_storage::pax::{PaxBlock, PaxBlockBuilder};

/// CSR overlay: `offsets[i]..offsets[i+1]` indexes `adjacency` to give
/// node `i`'s out-neighbors. `edge_refs` is parallel to `adjacency` and
/// points back to the originating edge record.
#[derive(Clone, Debug)]
pub struct CsrOverlay {
    /// Length = node_count + 1. `offsets[0] = 0`, `offsets[node_count] = edge_count`.
    pub offsets: Vec<u64>,
    /// Length = edge_count. The destination node id of each edge, sorted by src.
    pub adjacency: Vec<NodeId>,
    /// Length = edge_count. The (block_id, slot) of the originating edge
    /// record in the edges PAX block.
    pub edge_refs: Vec<(BlockId, u32)>,
}

impl CsrOverlay {
    /// Build a CSR overlay from a list of `(src, dst, edge_ref)` triples.
    /// The triples do not need to be sorted; we sort them by src internally.
    pub fn build(mut edges: Vec<(NodeId, NodeId, (BlockId, u32))>) -> Self {
        // Sort by src.
        edges.sort_by_key(|&(src, _, _)| src);

        // Determine node count (max node id + 1).
        let max_node = edges
            .iter()
            .map(|&(s, d, _)| s.0.max(d.0))
            .max()
            .unwrap_or(0);
        let node_count = max_node as usize + 1;

        // Build offsets: for each node, count its edges.
        let mut offsets = vec![0u64; node_count + 1];
        for &(src, _, _) in &edges {
            offsets[src.0 as usize + 1] += 1;
        }
        // Prefix sum.
        for i in 1..=node_count {
            offsets[i] += offsets[i - 1];
        }

        // Fill adjacency and edge_refs in CSR order.
        let mut adjacency = vec![NodeId(0); edges.len()];
        let mut edge_refs = vec![(BlockId(0), 0u32); edges.len()];
        let mut cursor = vec![0u64; node_count];
        for (src, dst, edge_ref) in &edges {
            let s = src.0 as usize;
            let pos = (offsets[s] + cursor[s]) as usize;
            adjacency[pos] = *dst;
            edge_refs[pos] = *edge_ref;
            cursor[s] += 1;
        }

        Self { offsets, adjacency, edge_refs }
    }

    /// Return the out-neighbors of `node`. O(1) per neighbor.
    pub fn neighbors(&self, node: NodeId) -> &[NodeId] {
        let s = node.0 as usize;
        if s + 1 >= self.offsets.len() {
            return &[];
        }
        let lo = self.offsets[s] as usize;
        let hi = self.offsets[s + 1] as usize;
        &self.adjacency[lo..hi]
    }

    /// Number of nodes the overlay knows about.
    pub fn node_count(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    /// Number of edges.
    pub fn edge_count(&self) -> usize {
        self.adjacency.len()
    }
}

/// Graph projection: nodes + edges + CSR overlay.
pub struct GraphProjection {
    /// Reserved for future use (will be used when the projection spills to
    /// a real segment file on disk).
    #[allow(dead_code)]
    segment_id: SegmentId,
    block_size: u32,
    nodes_block: Option<PaxBlock>,
    edges_block: Option<PaxBlock>,
    /// CSR overlay, built on demand via [`build_csr`].
    csr: Option<CsrOverlay>,
    /// Pending edges not yet flushed.
    pending_edges: Vec<(NodeId, NodeId, String)>,
    /// Pending nodes not yet flushed.
    pending_nodes: Vec<(NodeId, String)>,
}

impl GraphProjection {
    pub fn new(segment_id: SegmentId, block_size: u32) -> Self {
        Self {
            segment_id,
            block_size,
            nodes_block: None,
            edges_block: None,
            csr: None,
            pending_edges: Vec::new(),
            pending_nodes: Vec::new(),
        }
    }

    /// Add a node with a label.
    pub fn add_node(&mut self, id: NodeId, label: impl Into<String>) {
        self.pending_nodes.push((id, label.into()));
        self.csr = None; // invalidate
    }

    /// Add a directed edge `src -> dst` with a type label.
    pub fn add_edge(&mut self, src: NodeId, dst: NodeId, edge_type: impl Into<String>) {
        self.pending_edges.push((src, dst, edge_type.into()));
        self.csr = None; // invalidate
    }

    /// Flush pending nodes and edges into PAX blocks.
    pub fn flush(&mut self) -> CenResult<()> {
        if !self.pending_nodes.is_empty() {
            let specs = node_specs();
            let mut builder = PaxBlockBuilder::new(self.block_size, specs)?;
            for (id, label) in self.pending_nodes.drain(..) {
                builder.append_row(&[
                    Value::U64(id.0),
                    Value::Bytes(label.into_bytes()),
                ])?;
            }
            self.nodes_block = Some(builder.finalize()?);
        }
        if !self.pending_edges.is_empty() {
            // Sort by (src, edge_type) so the CSR overlay can be built
            // directly from the sorted order.
            self.pending_edges.sort_by(|a, b| {
                a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2))
            });
            let specs = edge_specs();
            let mut builder = PaxBlockBuilder::new(self.block_size, specs)?;
            for (src, dst, etype) in self.pending_edges.drain(..) {
                builder.append_row(&[
                    Value::U64(src.0),
                    Value::U64(dst.0),
                    Value::Bytes(etype.into_bytes()),
                ])?;
            }
            self.edges_block = Some(builder.finalize()?);
        }
        Ok(())
    }

    /// Build (or rebuild) the CSR overlay from the edges block.
    pub fn build_csr(&mut self) -> CenResult<&CsrOverlay> {
        if self.csr.is_some() {
            return Ok(self.csr.as_ref().unwrap());
        }
        let block = self
            .edges_block
            .as_ref()
            .ok_or_else(|| CenError::constraint("build_csr: no edges block (call flush first)"))?;
        let src_vals = block.decode_i64_column(0)?;
        let dst_vals = block.decode_i64_column(1)?;
        let mut edges: Vec<(NodeId, NodeId, (BlockId, u32))> = Vec::with_capacity(src_vals.len());
        for (i, (&src, &dst)) in src_vals.iter().zip(dst_vals.iter()).enumerate() {
            edges.push((NodeId(src as u64), NodeId(dst as u64), (BlockId(0), i as u32)));
        }
        self.csr = Some(CsrOverlay::build(edges));
        Ok(self.csr.as_ref().unwrap())
    }

    /// 1-hop traversal: return the out-neighbors of `node`.
    pub fn neighbors(&self, node: NodeId) -> CenResult<Vec<NodeId>> {
        let csr = self
            .csr
            .as_ref()
            .ok_or_else(|| CenError::constraint("neighbors: CSR not built (call build_csr first)"))?;
        Ok(csr.neighbors(node).to_vec())
    }

    /// 2-hop traversal: return all nodes reachable from `start` in exactly
    /// 2 hops. Used by the verification suite.
    pub fn two_hop(&self, start: NodeId) -> CenResult<Vec<NodeId>> {
        let csr = self
            .csr
            .as_ref()
            .ok_or_else(|| CenError::constraint("two_hop: CSR not built"))?;
        let mut out = Vec::new();
        let first_hop = csr.neighbors(start).to_vec();
        for &n1 in &first_hop {
            for &n2 in csr.neighbors(n1) {
                if n2 != start {
                    out.push(n2);
                }
            }
        }
        // Deduplicate (sorted).
        out.sort_by_key(|n| n.0);
        out.dedup();
        Ok(out)
    }

    /// BFS up to `max_depth` hops from `start`. Returns (depth, node_id)
    /// pairs.
    pub fn bfs(&self, start: NodeId, max_depth: usize) -> CenResult<Vec<(usize, NodeId)>> {
        let csr = self
            .csr
            .as_ref()
            .ok_or_else(|| CenError::constraint("bfs: CSR not built"))?;
        let mut visited: HashMap<u64, usize> = HashMap::new();
        visited.insert(start.0, 0);
        let mut frontier: Vec<NodeId> = vec![start];
        let mut out: Vec<(usize, NodeId)> = vec![(0, start)];
        for depth in 1..=max_depth {
            let mut next_frontier: Vec<NodeId> = Vec::new();
            for &node in &frontier {
                for &neighbor in csr.neighbors(node) {
                    if !visited.contains_key(&neighbor.0) {
                        visited.insert(neighbor.0, depth);
                        next_frontier.push(neighbor);
                        out.push((depth, neighbor));
                    }
                }
            }
            if next_frontier.is_empty() {
                break;
            }
            frontier = next_frontier;
        }
        Ok(out)
    }

    pub fn node_count(&self) -> usize {
        if let Some(b) = &self.nodes_block {
            b.header().row_count as usize
        } else {
            self.pending_nodes.len()
        }
    }

    pub fn edge_count(&self) -> usize {
        if let Some(b) = &self.edges_block {
            b.header().row_count as usize
        } else {
            self.pending_edges.len()
        }
    }

    pub fn csr_node_count(&self) -> usize {
        self.csr.as_ref().map(|c| c.node_count()).unwrap_or(0)
    }
}

/// Schema for the nodes PAX block: (node_id u64, label bytes).
fn node_specs() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new(0, ValueKind::U64).pk(),
        ColumnSpec::new(1, ValueKind::Bytes),
    ]
}

/// Schema for the edges PAX block: (src u64, dst u64, type bytes).
fn edge_specs() -> Vec<ColumnSpec> {
    vec![
        ColumnSpec::new(0, ValueKind::U64).pk(),
        ColumnSpec::new(1, ValueKind::U64),
        ColumnSpec::new(2, ValueKind::Bytes),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csr_neighbors_basic() {
        let edges = vec![
            (NodeId(0), NodeId(1), (BlockId(0), 0)),
            (NodeId(0), NodeId(2), (BlockId(0), 1)),
            (NodeId(1), NodeId(3), (BlockId(0), 2)),
            (NodeId(2), NodeId(3), (BlockId(0), 3)),
            (NodeId(3), NodeId(4), (BlockId(0), 4)),
        ];
        let csr = CsrOverlay::build(edges);
        assert_eq!(csr.node_count(), 5);
        assert_eq!(csr.edge_count(), 5);

        let n0 = csr.neighbors(NodeId(0));
        assert_eq!(n0, &[NodeId(1), NodeId(2)]);

        let n1 = csr.neighbors(NodeId(1));
        assert_eq!(n1, &[NodeId(3)]);

        let n4 = csr.neighbors(NodeId(4));
        assert!(n4.is_empty());
    }

    #[test]
    fn two_hop_traversal() {
        let mut g = GraphProjection::new(SegmentId(1), 16 * 1024);
        // 0 -> 1 -> 3
        // 0 -> 2 -> 3
        // 3 -> 4
        g.add_edge(NodeId(0), NodeId(1), "follows");
        g.add_edge(NodeId(0), NodeId(2), "follows");
        g.add_edge(NodeId(1), NodeId(3), "follows");
        g.add_edge(NodeId(2), NodeId(3), "follows");
        g.add_edge(NodeId(3), NodeId(4), "follows");
        g.flush().unwrap();
        g.build_csr().unwrap();

        let two_hop = g.two_hop(NodeId(0)).unwrap();
        // From 0: 1-hop = {1, 2}, 2-hop = {3} (from both 1 and 2).
        assert!(two_hop.contains(&NodeId(3)));
        // 4 is 3 hops away, not 2.
        assert!(!two_hop.contains(&NodeId(4)));
    }

    #[test]
    fn bfs_visits_all_reachable() {
        let mut g = GraphProjection::new(SegmentId(1), 16 * 1024);
        // Linear chain: 0 -> 1 -> 2 -> 3 -> 4
        for i in 0..4u64 {
            g.add_edge(NodeId(i), NodeId(i + 1), "next");
        }
        g.flush().unwrap();
        g.build_csr().unwrap();

        let bfs = g.bfs(NodeId(0), 10).unwrap();
        assert_eq!(bfs.len(), 5); // 5 nodes visited
        assert_eq!(bfs[0], (0, NodeId(0)));
        assert_eq!(bfs[4], (4, NodeId(4)));
    }

    #[test]
    fn add_nodes_and_flush() {
        let mut g = GraphProjection::new(SegmentId(1), 16 * 1024);
        g.add_node(NodeId(0), "Person");
        g.add_node(NodeId(1), "Person");
        g.add_node(NodeId(2), "Product");
        g.flush().unwrap();
        assert_eq!(g.node_count(), 3);
    }
}
