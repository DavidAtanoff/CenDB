//! Simplified R-tree for spatial bounding-box queries.
//!
//! Originally a flat-list brute-force index. Now extended to optionally
//! store full OGC [`Geometry`] values per entry, enabling exact
//! predicate-based queries via the [`crate::predicates`] module:
//!
//! * [`RTree::query_intersects`] — exact intersection against a query geometry.
//! * [`RTree::query_contains`]  — entries whose stored geometry contains the query.
//! * [`RTree::query_within_distance`] — `(id, distance)` pairs within a radius.
//!
//! When an entry has no stored geometry (e.g. inserted via the legacy
//! [`RTree::insert_point`] / [`RTree::insert_bbox`] methods), the query
//! methods fall back to bounding-box-level matching.

use crate::geometry::{BoundingBox, Geometry, Point};
use crate::predicates;

// Re-export the canonical Point / BoundingBox types so callers using
// `cendb_spatial::rtree::Point` keep working.
pub use crate::geometry::{BoundingBox as GeomBBox, Point as GeomPoint};

/// An R-tree entry: a bounding box + associated data (row ID), plus an
/// optional full [`Geometry`] for exact predicate queries.
#[derive(Clone, Debug)]
pub struct RTreeEntry {
    pub bbox: BoundingBox,
    pub id: u64,
    /// Optional full geometry. Populated by [`RTree::insert_geometry`];
    /// `None` for entries inserted via the legacy point/bbox methods.
    pub geometry: Option<Geometry>,
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
            geometry: Some(Geometry::Point(p)),
        });
    }

    /// Insert a bounding box with an associated ID. No full geometry is
    /// stored, so the exact-predicate query methods will fall back to
    /// bbox-level matching for this entry.
    pub fn insert_bbox(&mut self, id: u64, bbox: BoundingBox) {
        self.entries.push(RTreeEntry { bbox, id, geometry: None });
    }

    /// Insert a full geometry with an associated ID. The entry's
    /// bounding box is computed from the geometry. Enables exact
    /// predicate queries via [`query_intersects`], [`query_contains`],
    /// and [`query_within_distance`].
    ///
    /// [`query_intersects`]: RTree::query_intersects
    /// [`query_contains`]: RTree::query_contains
    /// [`query_within_distance`]: RTree::query_within_distance
    pub fn insert_geometry(&mut self, id: u64, geom: Geometry) {
        let bbox = geom.bounding_box().unwrap_or(BoundingBox {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 0.0,
            max_y: 0.0,
        });
        self.entries.push(RTreeEntry { bbox, id, geometry: Some(geom) });
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

    /// K-nearest neighbors search (brute-force, by bbox centroid).
    pub fn knn(&self, p: Point, k: usize) -> Vec<(u64, f64)> {
        let mut dists: Vec<(f64, u64)> = self
            .entries
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

    /// Find all indexed entries that intersect the query geometry.
    ///
    /// Uses bounding-box pre-filtering followed by exact geometry tests
    /// via [`predicates::intersects`] when an entry has a stored
    /// geometry; otherwise falls back to bbox intersection.
    pub fn query_intersects(&self, geom: &Geometry) -> Vec<u64> {
        let query_bbox = match geom.bounding_box() {
            Some(b) => b,
            None => return Vec::new(),
        };
        self.entries
            .iter()
            .filter(|e| {
                if !e.bbox.intersects(&query_bbox) {
                    return false;
                }
                match &e.geometry {
                    Some(g) => predicates::intersects(g, geom),
                    None => true, // bbox-level match; assume intersects
                }
            })
            .map(|e| e.id)
            .collect()
    }

    /// Find all indexed entries whose stored geometry contains the
    /// query geometry.
    ///
    /// Entries without a stored geometry (inserted via
    /// [`insert_bbox`]) are matched at the bbox level (i.e. their bbox
    /// contains the query bbox).
    ///
    /// [`insert_bbox`]: RTree::insert_bbox
    pub fn query_contains(&self, geom: &Geometry) -> Vec<u64> {
        let query_bbox = match geom.bounding_box() {
            Some(b) => b,
            None => return Vec::new(),
        };
        self.entries
            .iter()
            .filter(|e| {
                if !e.bbox.contains_bbox(&query_bbox) {
                    return false;
                }
                match &e.geometry {
                    Some(g) => predicates::contains(g, geom),
                    None => true, // bbox-level match
                }
            })
            .map(|e| e.id)
            .collect()
    }

    /// Find all indexed entries within `dist` (Euclidean) of the query
    /// geometry. Returns `(id, distance)` pairs sorted by distance.
    ///
    /// Uses bbox pre-filtering (bbox-to-bbox distance ≤ dist) followed
    /// by exact geometry distance via [`predicates::distance`] when an
    /// entry has a stored geometry; otherwise falls back to bbox
    /// centroid distance.
    pub fn query_within_distance(&self, geom: &Geometry, dist: f64) -> Vec<(u64, f64)> {
        let query_bbox = match geom.bounding_box() {
            Some(b) => b,
            None => return Vec::new(),
        };
        let mut results: Vec<(u64, f64)> = self
            .entries
            .iter()
            .filter_map(|e| {
                // Cheap reject: bbox-to-bbox distance > dist.
                if !bbox_within_distance(&e.bbox, &query_bbox, dist) {
                    return None;
                }
                let d = match &e.geometry {
                    Some(g) => predicates::distance(g, geom),
                    None => {
                        // Fallback: distance from query bbox to entry
                        // bbox centroid.
                        let cx = (e.bbox.min_x + e.bbox.max_x) / 2.0;
                        let cy = (e.bbox.min_y + e.bbox.max_y) / 2.0;
                        predicates::distance(&Geometry::Point(Point::new(cx, cy)), geom)
                    }
                };
                if d <= dist {
                    Some((e.id, d))
                } else {
                    None
                }
            })
            .collect();
        results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        results
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

/// True if the minimum Euclidean distance between two bboxes is ≤ `dist`.
fn bbox_within_distance(a: &BoundingBox, b: &BoundingBox, dist: f64) -> bool {
    let dx = if a.max_x < b.min_x {
        b.min_x - a.max_x
    } else if b.max_x < a.min_x {
        a.min_x - b.max_x
    } else {
        0.0
    };
    let dy = if a.max_y < b.min_y {
        b.min_y - a.max_y
    } else if b.max_y < a.min_y {
        a.min_y - b.max_y
    } else {
        0.0
    };
    dx * dx + dy * dy <= dist * dist
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

    // ----------------- New predicate-based query tests -----------------

    #[test]
    fn query_intersects_point() {
        let mut tree = RTree::new();
        // Insert two small polygons as geometry.
        tree.insert_geometry(1, Geometry::polygon(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]));
        tree.insert_geometry(2, Geometry::polygon(&[(100.0, 100.0), (110.0, 100.0), (110.0, 110.0), (100.0, 110.0), (100.0, 100.0)]));

        // A point inside the first polygon.
        let q = Geometry::point(5.0, 5.0);
        let hits = tree.query_intersects(&q);
        assert_eq!(hits, vec![1]);

        // A point inside the second polygon.
        let q = Geometry::point(105.0, 105.0);
        let hits = tree.query_intersects(&q);
        assert_eq!(hits, vec![2]);

        // A point in neither.
        let q = Geometry::point(50.0, 50.0);
        let hits = tree.query_intersects(&q);
        assert!(hits.is_empty());
    }

    #[test]
    fn query_intersects_polygon() {
        let mut tree = RTree::new();
        tree.insert_geometry(1, Geometry::polygon(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]));
        tree.insert_geometry(2, Geometry::polygon(&[(100.0, 100.0), (110.0, 100.0), (110.0, 110.0), (100.0, 110.0), (100.0, 100.0)]));

        // Query polygon overlapping the first.
        let q = Geometry::polygon(&[(5.0, 5.0), (15.0, 5.0), (15.0, 15.0), (5.0, 15.0), (5.0, 5.0)]);
        let hits = tree.query_intersects(&q);
        assert_eq!(hits, vec![1]);
    }

    #[test]
    fn query_within_distance_point() {
        let mut tree = RTree::new();
        tree.insert_geometry(1, Geometry::point(0.0, 0.0));
        tree.insert_geometry(2, Geometry::point(10.0, 0.0));
        tree.insert_geometry(3, Geometry::point(100.0, 0.0));

        // Query point at (5, 0) with radius 6: should match (0,0) dist=5 and (10,0) dist=5
        let q = Geometry::point(5.0, 0.0);
        let hits = tree.query_within_distance(&q, 6.0);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|(id, d)| *id == 1 && (*d - 5.0).abs() < 1e-9));
        assert!(hits.iter().any(|(id, d)| *id == 2 && (*d - 5.0).abs() < 1e-9));

        // Sorted by distance?
        assert!(hits[0].1 <= hits[1].1);
    }

    #[test]
    fn query_within_distance_excludes_far_points() {
        let mut tree = RTree::new();
        tree.insert_geometry(1, Geometry::point(0.0, 0.0));
        tree.insert_geometry(2, Geometry::point(100.0, 100.0));

        let q = Geometry::point(0.0, 0.0);
        let hits = tree.query_within_distance(&q, 5.0);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 1);
        assert!(hits[0].1.abs() < 1e-9);
    }

    #[test]
    fn query_contains_polygon() {
        let mut tree = RTree::new();
        // Large polygon.
        tree.insert_geometry(1, Geometry::polygon(&[(0.0, 0.0), (100.0, 0.0), (100.0, 100.0), (0.0, 100.0), (0.0, 0.0)]));
        // Small polygon (won't contain the query).
        tree.insert_geometry(2, Geometry::polygon(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]));

        // Query point inside the large polygon.
        let q = Geometry::point(50.0, 50.0);
        let hits = tree.query_contains(&q);
        assert_eq!(hits, vec![1]);
    }

    #[test]
    fn query_intersects_with_bbox_only_entry() {
        // Entries inserted via insert_bbox have no stored geometry —
        // query_intersects should still find them via bbox intersection.
        let mut tree = RTree::new();
        tree.insert_bbox(1, BoundingBox::new(0.0, 10.0, 0.0, 10.0));
        tree.insert_bbox(2, BoundingBox::new(100.0, 110.0, 100.0, 110.0));

        let q = Geometry::point(5.0, 5.0);
        let hits = tree.query_intersects(&q);
        assert_eq!(hits, vec![1]);
    }

    #[test]
    fn bbox_within_distance_helper() {
        let a = BoundingBox::new(0.0, 1.0, 0.0, 1.0);
        let b = BoundingBox::new(3.0, 4.0, 0.0, 1.0);
        // x-gap of 2; distance = 2.
        assert!(bbox_within_distance(&a, &b, 2.0));
        assert!(bbox_within_distance(&a, &b, 3.0));
        assert!(!bbox_within_distance(&a, &b, 1.0));
    }
}
