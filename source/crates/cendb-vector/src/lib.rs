//! HNSW vector search index.
//!
//! We implement a proper multi-layer hierarchical graph structure.
//! For a simple but highly correct implementation, we can maintain the standard
//! HNSW hierarchy.
//! Note: Let's make sure the distance metric is correct (distance = 1.0 - similarity)
//! where similarity = cosine_similarity. Lower distance means closer neighbors.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::cmp::Ordering;

pub type VectorId = u64;

#[derive(Clone, Debug)]
pub struct HnswConfig {
    pub m: usize,
    pub ef_search: usize,
    pub ef_construction: usize,
}

impl Default for HnswConfig {
    fn default() -> Self {
        Self { m: 16, ef_search: 50, ef_construction: 64 }
    }
}

#[derive(Clone, Debug)]
struct Node {
    vector: Vec<f32>,
    /// Neighbors at each layer. layers[0] is ground level.
    layers: Vec<Vec<VectorId>>,
}

#[derive(PartialEq)]
struct DistNode {
    dist: f64,
    id: VectorId,
}

impl Eq for DistNode {}

// BinaryHeap is a Max-Heap. We want to pop the node with the LARGEST distance first
// (farthest node) to prune the heap, so we define Ord based on dist ascending.
impl Ord for DistNode {
    fn cmp(&self, other: &Self) -> Ordering {
        self.dist.partial_cmp(&other.dist).unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for DistNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub struct HnswIndex {
    config: HnswConfig,
    nodes: HashMap<VectorId, Node>,
    entry_point: Option<VectorId>,
    max_level: usize,
}

impl HnswIndex {
    pub fn new(config: HnswConfig) -> Self {
        Self {
            config,
            nodes: HashMap::new(),
            entry_point: None,
            max_level: 0,
        }
    }

    fn random_level(&self) -> usize {
        let ml = 1.0 / (self.config.m as f64).ln();
        let mut level = 0usize;
        let mut rng = self.nodes.len() as u64 ^ 0x517cc1b727220a95;
        while level < 16 {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let f = (rng as f64) / (u64::MAX as f64);
            if f > ml { break; }
            level += 1;
        }
        level
    }

    fn distance(&self, q: &[f32], v: &[f32]) -> f64 {
        1.0 - cosine_similarity(q, v)
    }

    /// Search for neighbors in a single layer using greedy/priority queue search.
    fn search_layer(&self, q: &[f32], enter_points: &[VectorId], ef: usize, level: usize) -> Vec<DistNode> {
        let mut visited = HashSet::new();
        // min-heap by distance (so we pop closest first)
        let mut candidates = BinaryHeap::new();
        // max-heap by distance (so we pop farthest first)
        let mut found = BinaryHeap::new();

        for &ep in enter_points {
            if visited.insert(ep) {
                let dist = self.distance(q, &self.nodes[&ep].vector);
                candidates.push(std::cmp::Reverse(DistNode { dist, id: ep }));
                found.push(DistNode { dist, id: ep });
            }
        }

        while let Some(std::cmp::Reverse(curr)) = candidates.pop() {
            let worst_found = found.peek().unwrap();
            if curr.dist > worst_found.dist {
                break;
            }

            if let Some(node) = self.nodes.get(&curr.id) {
                if level < node.layers.len() {
                    for &nb in &node.layers[level] {
                        if visited.insert(nb) {
                            let dist = self.distance(q, &self.nodes[&nb].vector);
                            let worst_found = found.peek().unwrap();
                            if dist < worst_found.dist || found.len() < ef {
                                candidates.push(std::cmp::Reverse(DistNode { dist, id: nb }));
                                found.push(DistNode { dist, id: nb });
                                if found.len() > ef {
                                    found.pop();
                                }
                            }
                        }
                    }
                }
            }
        }

        found.into_sorted_vec()
    }

    pub fn insert(&mut self, id: VectorId, vector: Vec<f32>) {
        if self.nodes.is_empty() {
            let level = self.random_level();
            let node = Node {
                vector,
                layers: vec![Vec::new(); level + 1],
            };
            self.nodes.insert(id, node);
            self.entry_point = Some(id);
            self.max_level = level;
            return;
        }

        let insert_level = self.random_level();
        let mut curr_eps = vec![self.entry_point.unwrap()];

        // Greedy traversal down to insert_level
        for lvl in (insert_level + 1..=self.max_level).rev() {
            let res = self.search_layer(&vector, &curr_eps, 1, lvl);
            if let Some(closest) = res.first() {
                curr_eps = vec![closest.id];
            }
        }

        // 1. Calculate neighbors for each layer first
        let mut new_node = Node {
            vector: vector.clone(),
            layers: vec![Vec::new(); insert_level + 1],
        };

        let mut connections = vec![Vec::new(); insert_level + 1];

        for lvl in (0..=insert_level.min(self.max_level)).rev() {
            let candidates = self.search_layer(&vector, &curr_eps, self.config.ef_construction, lvl);
            let m_conn = if lvl == 0 { self.config.m * 2 } else { self.config.m };

            let selected: Vec<VectorId> = candidates.iter().take(m_conn).map(|dn| dn.id).collect();
            new_node.layers[lvl] = selected.clone();
            connections[lvl] = selected.clone();
            curr_eps = selected;
        }

        // 2. Insert the new node so it can be queried by its neighbors during their pruning
        self.nodes.insert(id, new_node);

        // 3. Connect back from neighbors
        for lvl in 0..=insert_level.min(self.max_level) {
            let m_conn = if lvl == 0 { self.config.m * 2 } else { self.config.m };
            let selected = &connections[lvl];
            for &nb_id in selected {
                if let Some(nb) = self.nodes.get_mut(&nb_id) {
                    nb.layers[lvl].push(id);
                    if nb.layers[lvl].len() > m_conn * 2 {
                        let nb_vec = nb.vector.clone();
                        let neighbors = std::mem::take(&mut nb.layers[lvl]);
                        let mut dists: Vec<(f64, VectorId)> = neighbors
                            .iter()
                            .map(|&a_id| {
                                let d = cosine_similarity(&nb_vec, &self.nodes[&a_id].vector);
                                (1.0 - d, a_id)
                            })
                            .collect();
                        dists.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
                        let mut sorted: Vec<VectorId> = dists.into_iter().map(|(_, a_id)| a_id).collect();
                        sorted.truncate(m_conn);
                        if let Some(nb_mut) = self.nodes.get_mut(&nb_id) {
                            nb_mut.layers[lvl] = sorted;
                        }
                    }
                }
            }
        }

        if insert_level > self.max_level {
            self.max_level = insert_level;
            self.entry_point = Some(id);
        }
    }

    pub fn search(&self, query: &[f32], k: usize) -> Vec<(VectorId, f32)> {
        if self.nodes.is_empty() {
            return Vec::new();
        }
        let ep = self.entry_point.unwrap();
        let mut curr_eps = vec![ep];

        for lvl in (1..=self.max_level).rev() {
            let res = self.search_layer(query, &curr_eps, 1, lvl);
            if let Some(closest) = res.first() {
                curr_eps = vec![closest.id];
            }
        }

        let ef = self.config.ef_search.max(k);
        let candidates = self.search_layer(query, &curr_eps, ef, 0);

        candidates
            .into_iter()
            .take(k)
            .map(|dn| {
                let sim = 1.0 - dn.dist;
                (dn.id, sim as f32)
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    let len = a.len().min(b.len());
    for i in 0..len {
        dot += (a[i] as f64) * (b[i] as f64);
        norm_a += (a[i] as f64) * (a[i] as f64);
        norm_b += (b[i] as f64) * (b[i] as f64);
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_search() {
        let mut index = HnswIndex::new(HnswConfig::default());
        for i in 0..100u64 {
            let vec: Vec<f32> = (0..8).map(|j| ((i + j) as f32) * 0.1).collect();
            index.insert(i, vec);
        }
        assert_eq!(index.len(), 100);

        let query: Vec<f32> = (0..8).map(|j| (j as f32) * 0.1).collect();
        let results = index.search(&query, 5);
        assert!(!results.is_empty());
        assert_eq!(results[0].0, 0); // Closest to itself.
        assert!(results[0].1 > 0.99);
    }

    #[test]
    fn search_finds_similar_vectors() {
        let mut index = HnswIndex::new(HnswConfig::default());
        for i in 0..30u64 {
            let cluster = i % 3;
            let vec: Vec<f32> = match cluster {
                0 => vec![1.0, 0.0, 0.0, 0.0],
                1 => vec![0.0, 1.0, 0.0, 0.0],
                2 => vec![0.0, 0.0, 1.0, 0.0],
                _ => vec![0.0; 4],
            };
            index.insert(i, vec);
        }
        let query = vec![0.99, 0.01, 0.0, 0.0];
        let results = index.search(&query, 5);
        let cluster_0_count = results.iter().filter(|(id, _)| id % 3 == 0).count();
        assert!(cluster_0_count >= 3, "expected mostly cluster-0 results, got {:?}", results);
    }

    #[test]
    fn empty_index_search() {
        let index = HnswIndex::new(HnswConfig::default());
        let results = index.search(&[1.0, 2.0, 3.0], 5);
        assert!(results.is_empty());
    }

    #[test]
    fn cosine_similarity_basic() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-9);

        let c = vec![0.0, 1.0, 0.0];
        assert!((cosine_similarity(&a, &c) - 0.0).abs() < 1e-9);

        let d = vec![-1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &d) - (-1.0)).abs() < 1e-9);
    }

    #[test]
    fn large_index_returns_k_results() {
        let mut index = HnswIndex::new(HnswConfig { m: 8, ef_search: 20, ef_construction: 32 });
        for i in 0..500u64 {
            let vec: Vec<f32> = (0..16).map(|j| (i as f32 + j as f32) / 500.0).collect();
            index.insert(i, vec);
        }
        let query: Vec<f32> = (0..16).map(|j| j as f32 / 500.0).collect();
        let results = index.search(&query, 10);
        assert_eq!(results.len(), 10);
        assert!(results[0].1 > 0.95, "Top similarity should be high, got {}", results[0].1);
    }
}
