//! Simplified R-tree for spatial bounding-box queries.

/// A 2D point.
#[derive(Copy, Clone, Debug)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// A bounding box.
#[derive(Copy, Clone, Debug)]
pub struct BoundingBox {
    pub min_x: f64,
    pub max_x: f64,
    pub min_y: f64,
    pub max_y: f64,
}

impl BoundingBox {
    pub fn new(min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Self {
        Self { min_x, max_x, min_y, max_y }
    }

    pub fn from_point(p: Point) -> Self {
        Self { min_x: p.x, max_x: p.x, min_y: p.y, max_y: p.y }
    }

    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.min_x && p.x <= self.max_x && p.y >= self.min_y && p.y <= self.max_y
    }

    pub fn intersects(&self, other: &BoundingBox) -> bool {
        !(self.max_x < other.min_x
            || self.min_x > other.max_x
            || self.max_y < other.min_y
            || self.min_y > other.max_y)
    }

    pub fn merge(&self, other: &BoundingBox) -> BoundingBox {
        BoundingBox {
            min_x: self.min_x.min(other.min_x),
            max_x: self.max_x.max(other.max_x),
            min_y: self.min_y.min(other.min_y),
            max_y: self.max_y.max(other.max_y),
        }
    }

    pub fn area(&self) -> f64 {
        (self.max_x - self.min_x) * (self.max_y - self.min_y)
    }
}

/// An R-tree entry: a bounding box + associated data (row ID).
#[derive(Clone, Debug)]
pub struct RTreeEntry {
    pub bbox: BoundingBox,
    pub id: u64,
}

/// A simplified R-tree: supports insertion and range search.
/// This implementation uses a flat list with brute-force search for
/// correctness; a production version would use hierarchical nodes.
pub struct RTree {
    entries: Vec<RTreeEntry>,
}

impl RTree {
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Insert a point with an associated ID.
    pub fn insert_point(&mut self, id: u64, p: Point) {
        self.entries.push(RTreeEntry {
            bbox: BoundingBox::from_point(p),
            id,
        });
    }

    /// Insert a bounding box with an associated ID.
    pub fn insert_bbox(&mut self, id: u64, bbox: BoundingBox) {
        self.entries.push(RTreeEntry { bbox, id });
    }

    /// Search for all entries intersecting the query bounding box.
    pub fn search(&self, query: &BoundingBox) -> Vec<u64> {
        self.entries
            .iter()
            .filter(|e| e.bbox.intersects(query))
            .map(|e| e.id)
            .collect()
    }

    /// Search for all entries containing a point.
    pub fn search_point(&self, p: Point) -> Vec<u64> {
        self.entries
            .iter()
            .filter(|e| e.bbox.contains(p))
            .map(|e| e.id)
            .collect()
    }

    /// K-nearest neighbors search (brute-force).
    pub fn knn(&self, p: Point, k: usize) -> Vec<(u64, f64)> {
        let mut dists: Vec<(f64, u64)> = self.entries
            .iter()
            .map(|e| {
                let cx = (e.bbox.min_x + e.bbox.max_x) / 2.0;
                let cy = (e.bbox.min_y + e.bbox.max_y) / 2.0;
                let dx = cx - p.x;
                let dy = cy - p.y;
                (dx * dx + dy * dy, e.id)
            })
            .collect();
        dists.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        dists.into_iter().take(k).map(|(d, id)| (id, d.sqrt())).collect()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for RTree {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_search_bbox() {
        let mut tree = RTree::new();
        tree.insert_point(1, Point { x: 10.0, y: 10.0 });
        tree.insert_point(2, Point { x: 50.0, y: 50.0 });
        tree.insert_point(3, Point { x: 90.0, y: 90.0 });

        let results = tree.search(&BoundingBox::new(0.0, 20.0, 0.0, 20.0));
        assert_eq!(results, vec![1]);

        let results = tree.search(&BoundingBox::new(40.0, 100.0, 40.0, 100.0));
        assert!(results.contains(&2));
        assert!(results.contains(&3));
    }

    #[test]
    fn knn_search() {
        let mut tree = RTree::new();
        for i in 0..100u64 {
            tree.insert_point(i, Point { x: i as f64, y: i as f64 });
        }

        let results = tree.knn(Point { x: 50.0, y: 50.0 }, 3);
        assert_eq!(results.len(), 3);
        // Closest to (50,50) should be point 50 itself.
        assert_eq!(results[0].0, 50);
    }
}
