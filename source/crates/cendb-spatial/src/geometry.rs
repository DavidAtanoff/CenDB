//! OGC Simple Features geometry types.
//!
//! Defines the canonical geometry model used by the spatial crate:
//! `Point`, `LineString`, `Polygon`, `MultiPoint`, `MultiLineString`,
//! `MultiPolygon`, and a `Geometry` enum wrapping them all, plus a
//! `BoundingBox` type used for spatial indexing pre-filtering.
//!
//! The `Point` and `BoundingBox` types defined here are re-exported by
//! the R-tree module so that callers can use a single, consistent set of
//! spatial primitives across the crate.

use core::fmt;

// ============================================================================
// Point
// ============================================================================

/// A 2D point with `x` and `y` coordinates. `Copy` so it can be passed
/// around by value cheaply.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    #[inline]
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }

    /// Returns `true` if either coordinate is NaN or infinite.
    /// Such points are invalid and should be rejected by all spatial
    /// operations to prevent silent incorrect results.
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }

    /// Euclidean distance to another point.
    #[inline]
    pub fn distance(&self, other: &Point) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        (dx * dx + dy * dy).sqrt()
    }
}

impl fmt::Display for Point {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({} {})", self.x, self.y)
    }
}

// ============================================================================
// LineString
// ============================================================================

/// An ordered sequence of `Point`s. A `LineString` with two or more
/// points represents a polyline; if the first and last points coincide,
/// it is *closed* and can be used as a polygon ring.
#[derive(Clone, Debug, PartialEq)]
pub struct LineString {
    pub points: Vec<Point>,
}

impl LineString {
    pub fn new(points: Vec<Point>) -> Self {
        Self { points }
    }

    /// Build a `LineString` from `(x, y)` tuples.
    pub fn from_xy(coords: &[(f64, f64)]) -> Self {
        Self {
            points: coords.iter().map(|&(x, y)| Point::new(x, y)).collect(),
        }
    }

    /// Number of points.
    #[inline]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// True if there are zero points.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// True if the linestring is closed (first point == last point).
    pub fn is_closed(&self) -> bool {
        self.points.len() >= 2 && self.points.first() == self.points.last()
    }

    /// Iterator over consecutive segments `(p_i, p_{i+1})`.
    pub fn segments(&self) -> impl Iterator<Item = (Point, Point)> + '_ {
        self.points.windows(2).map(|w| (w[0], w[1]))
    }

    /// Compute the bounding box. Returns `None` if empty.
    pub fn bounding_box(&self) -> Option<BoundingBox> {
        let (min_x, max_x, min_y, max_y) =
            self.points.iter().fold(None, |acc, p| match acc {
                None => Some((p.x, p.x, p.y, p.y)),
                Some((mnx, mxx, mny, mxy)) => Some((
                    mnx.min(p.x),
                    mxx.max(p.x),
                    mny.min(p.y),
                    mxy.max(p.y),
                )),
            })?;
        Some(BoundingBox { min_x, min_y, max_x, max_y })
    }
}

// ============================================================================
// Polygon
// ============================================================================

/// A polygon: one exterior ring and zero or more interior rings (holes).
/// Rings are `LineString`s; they should be closed but the predicates
/// module tolerates unclosed rings by treating them as implicitly closed.
#[derive(Clone, Debug, PartialEq)]
pub struct Polygon {
    pub exterior: LineString,
    pub interiors: Vec<LineString>,
}

impl Polygon {
    pub fn new(exterior: LineString, interiors: Vec<LineString>) -> Self {
        Self { exterior, interiors }
    }

    /// Build a polygon from an exterior ring only.
    pub fn from_exterior(exterior: LineString) -> Self {
        Self { exterior, interiors: Vec::new() }
    }

    /// Iterator over all rings: exterior first, then interiors.
    pub fn rings(&self) -> impl Iterator<Item = &LineString> {
        core::iter::once(&self.exterior).chain(self.interiors.iter())
    }

    pub fn bounding_box(&self) -> Option<BoundingBox> {
        self.exterior.bounding_box()
    }
}

// ============================================================================
// Multi* collections
// ============================================================================

/// A collection of points.
#[derive(Clone, Debug, PartialEq)]
pub struct MultiPoint {
    pub points: Vec<Point>,
}

impl MultiPoint {
    pub fn new(points: Vec<Point>) -> Self {
        Self { points }
    }

    pub fn bounding_box(&self) -> Option<BoundingBox> {
        let (min_x, max_x, min_y, max_y) =
            self.points.iter().fold(None, |acc, p| match acc {
                None => Some((p.x, p.x, p.y, p.y)),
                Some((mnx, mxx, mny, mxy)) => Some((
                    mnx.min(p.x),
                    mxx.max(p.x),
                    mny.min(p.y),
                    mxy.max(p.y),
                )),
            })?;
        Some(BoundingBox { min_x, min_y, max_x, max_y })
    }
}

/// A collection of linestrings.
#[derive(Clone, Debug, PartialEq)]
pub struct MultiLineString {
    pub linestrings: Vec<LineString>,
}

impl MultiLineString {
    pub fn new(linestrings: Vec<LineString>) -> Self {
        Self { linestrings }
    }

    pub fn bounding_box(&self) -> Option<BoundingBox> {
        self.linestrings
            .iter()
            .fold(None, |acc, ls| match (acc, ls.bounding_box()) {
                (None, None) => None,
                (None, Some(b)) => Some(b),
                (Some(a), None) => Some(a),
                (Some(a), Some(b)) => Some(a.union(&b)),
            })
    }
}

/// A collection of polygons.
#[derive(Clone, Debug, PartialEq)]
pub struct MultiPolygon {
    pub polygons: Vec<Polygon>,
}

impl MultiPolygon {
    pub fn new(polygons: Vec<Polygon>) -> Self {
        Self { polygons }
    }

    pub fn bounding_box(&self) -> Option<BoundingBox> {
        self.polygons
            .iter()
            .fold(None, |acc, p| match (acc, p.bounding_box()) {
                (None, None) => None,
                (None, Some(b)) => Some(b),
                (Some(a), None) => Some(a),
                (Some(a), Some(b)) => Some(a.union(&b)),
            })
    }
}

// ============================================================================
// Geometry enum
// ============================================================================

/// The OGC geometry enum wrapping every concrete geometry type.
#[derive(Clone, Debug, PartialEq)]
pub enum Geometry {
    Point(Point),
    LineString(LineString),
    Polygon(Polygon),
    MultiPoint(MultiPoint),
    MultiLineString(MultiLineString),
    MultiPolygon(MultiPolygon),
    /// A heterogeneous collection of geometries. Supported for
    /// serialization round-tripping; most predicates treat it as the
    /// union of its parts.
    GeometryCollection(Vec<Geometry>),
}

impl Geometry {
    /// Convenience constructor for a `Point`.
    pub fn point(x: f64, y: f64) -> Self {
        Self::Point(Point::new(x, y))
    }

    /// Convenience constructor for a `LineString` from `(x, y)` tuples.
    pub fn line_string(coords: &[(f64, f64)]) -> Self {
        Self::LineString(LineString::from_xy(coords))
    }

    /// Convenience constructor for a `Polygon` from an exterior ring of
    /// `(x, y)` tuples.
    pub fn polygon(exterior: &[(f64, f64)]) -> Self {
        Self::Polygon(Polygon::from_exterior(LineString::from_xy(exterior)))
    }

    /// Topological dimension of the geometry (0 for points, 1 for
    /// curves, 2 for surfaces).
    pub fn dimension(&self) -> u8 {
        match self {
            Geometry::Point(_) | Geometry::MultiPoint(_) => 0,
            Geometry::LineString(_) | Geometry::MultiLineString(_) => 1,
            Geometry::Polygon(_) | Geometry::MultiPolygon(_) => 2,
            Geometry::GeometryCollection(gs) => {
                gs.iter().map(|g| g.dimension()).max().unwrap_or(0)
            }
        }
    }

    /// True if the geometry contains no points at all (e.g. an empty
    /// linestring or polygon with an empty exterior).
    pub fn is_empty(&self) -> bool {
        match self {
            Geometry::Point(_) => false,
            Geometry::LineString(ls) => ls.is_empty(),
            Geometry::Polygon(p) => p.exterior.is_empty(),
            Geometry::MultiPoint(mp) => mp.points.is_empty(),
            Geometry::MultiLineString(mls) => mls.linestrings.is_empty(),
            Geometry::MultiPolygon(mp) => mp.polygons.is_empty(),
            Geometry::GeometryCollection(gs) => gs.is_empty(),
        }
    }

    /// Axis-aligned bounding box of the geometry. Returns `None` for
    /// empty geometries.
    pub fn bounding_box(&self) -> Option<BoundingBox> {
        match self {
            Geometry::Point(p) => Some(BoundingBox::from_point(*p)),
            Geometry::LineString(ls) => ls.bounding_box(),
            Geometry::Polygon(p) => p.bounding_box(),
            Geometry::MultiPoint(mp) => mp.bounding_box(),
            Geometry::MultiLineString(mls) => mls.bounding_box(),
            Geometry::MultiPolygon(mp) => mp.bounding_box(),
            Geometry::GeometryCollection(gs) => gs.iter().fold(None, |acc, g| {
                match (acc, g.bounding_box()) {
                    (None, None) => None,
                    (None, Some(b)) => Some(b),
                    (Some(a), None) => Some(a),
                    (Some(a), Some(b)) => Some(a.union(&b)),
                }
            }),
        }
    }

    /// Iterate over the components as `&Geometry`. For single
    /// geometries yields a single reference to self.
    pub fn components(&self) -> Vec<&Geometry> {
        match self {
            Geometry::GeometryCollection(gs) => gs.iter().collect(),
            other => vec![other],
        }
    }
}

impl From<Point> for Geometry {
    fn from(p: Point) -> Self {
        Geometry::Point(p)
    }
}
impl From<LineString> for Geometry {
    fn from(ls: LineString) -> Self {
        Geometry::LineString(ls)
    }
}
impl From<Polygon> for Geometry {
    fn from(p: Polygon) -> Self {
        Geometry::Polygon(p)
    }
}

// ============================================================================
// BoundingBox
// ============================================================================

/// An axis-aligned bounding box. Field order follows the convention
/// `min_x, min_y, max_x, max_y` used throughout the OGC literature.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct BoundingBox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl BoundingBox {
    /// Construct a new bbox. The argument order `(min_x, max_x, min_y,
    /// max_y)` is preserved from the original R-tree API for backward
    /// compatibility — internally we store the fields as
    /// `{min_x, min_y, max_x, max_y}`.
    #[inline]
    pub const fn new(min_x: f64, max_x: f64, min_y: f64, max_y: f64) -> Self {
        Self { min_x, min_y, max_x, max_y }
    }

    /// Construct from explicit min/max pairs.
    #[inline]
    pub const fn from_corners(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self { min_x, min_y, max_x, max_y }
    }

    /// A degenerate bbox containing a single point.
    #[inline]
    pub fn from_point(p: Point) -> Self {
        Self { min_x: p.x, min_y: p.y, max_x: p.x, max_y: p.y }
    }

    /// True if `p` lies inside (or on the boundary of) the bbox.
    pub fn contains(&self, p: Point) -> bool {
        p.x >= self.min_x && p.x <= self.max_x && p.y >= self.min_y && p.y <= self.max_y
    }

    /// True if the bbox fully contains `other` (boundaries inclusive).
    pub fn contains_bbox(&self, other: &BoundingBox) -> bool {
        self.min_x <= other.min_x
            && self.max_x >= other.max_x
            && self.min_y <= other.min_y
            && self.max_y >= other.max_y
    }

    /// True if the two bboxes share any point.
    pub fn intersects(&self, other: &BoundingBox) -> bool {
        !(self.max_x < other.min_x
            || self.min_x > other.max_x
            || self.max_y < other.min_y
            || self.min_y > other.max_y)
    }

    /// Smallest bbox containing both `self` and `other`.
    pub fn union(&self, other: &BoundingBox) -> BoundingBox {
        BoundingBox {
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }

    /// Alias for [`union`] retained for backward compatibility with the
    /// original R-tree API.
    pub fn merge(&self, other: &BoundingBox) -> BoundingBox {
        self.union(other)
    }

    /// Area of the bbox. Zero for degenerate (point/line) boxes.
    pub fn area(&self) -> f64 {
        (self.max_x - self.min_x) * (self.max_y - self.min_y)
    }

    /// Minimum squared Euclidean distance from `p` to this bbox. Zero
    /// if `p` is inside.
    pub fn distance_sq_to_point(&self, p: Point) -> f64 {
        let dx = if p.x < self.min_x {
            self.min_x - p.x
        } else if p.x > self.max_x {
            p.x - self.max_x
        } else {
            0.0
        };
        let dy = if p.y < self.min_y {
            self.min_y - p.y
        } else if p.y > self.max_y {
            p.y - self.max_y
        } else {
            0.0
        };
        dx * dx + dy * dy
    }

    /// Minimum Euclidean distance from `p` to this bbox.
    pub fn distance_to_point(&self, p: Point) -> f64 {
        self.distance_sq_to_point(p).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_creation_and_distance() {
        let p = Point::new(0.0, 0.0);
        let q = Point::new(3.0, 4.0);
        assert_eq!(p.distance(&q), 5.0);
        assert_eq!(p, Point { x: 0.0, y: 0.0 });
    }

    #[test]
    fn linestring_closed_and_bbox() {
        let ls = LineString::from_xy(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]);
        assert!(ls.is_closed());
        assert_eq!(ls.len(), 4);
        let bb = ls.bounding_box().unwrap();
        assert_eq!(bb, BoundingBox::from_corners(0.0, 0.0, 1.0, 1.0));
    }

    #[test]
    fn polygon_with_hole() {
        let exterior = LineString::from_xy(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]);
        let hole = LineString::from_xy(&[(2.0, 2.0), (4.0, 2.0), (4.0, 4.0), (2.0, 4.0), (2.0, 2.0)]);
        let poly = Polygon::new(exterior.clone(), vec![hole.clone()]);
        assert_eq!(poly.exterior, exterior);
        assert_eq!(poly.interiors.len(), 1);
        assert_eq!(poly.interiors[0], hole);
        let bb = poly.bounding_box().unwrap();
        assert_eq!(bb, BoundingBox::from_corners(0.0, 0.0, 10.0, 10.0));
    }

    #[test]
    fn bbox_union_intersects_contains() {
        let a = BoundingBox::from_corners(0.0, 0.0, 2.0, 2.0);
        let b = BoundingBox::from_corners(1.0, 1.0, 3.0, 3.0);
        let c = BoundingBox::from_corners(10.0, 10.0, 12.0, 12.0);

        assert!(a.intersects(&b));
        assert!(!a.intersects(&c));

        let u = a.union(&b);
        assert_eq!(u, BoundingBox::from_corners(0.0, 0.0, 3.0, 3.0));

        // contains(point)
        assert!(a.contains(Point::new(1.0, 1.0)));
        assert!(a.contains(Point::new(0.0, 0.0))); // boundary
        assert!(!a.contains(Point::new(2.5, 1.0)));

        // contains_bbox
        let inner = BoundingBox::from_corners(0.5, 0.5, 1.5, 1.5);
        assert!(a.contains_bbox(&inner));
        assert!(!a.contains_bbox(&b)); // b sticks out
    }

    #[test]
    fn geometry_dimension_and_bbox() {
        let pt = Geometry::point(1.0, 2.0);
        let ls = Geometry::line_string(&[(0.0, 0.0), (1.0, 1.0)]);
        let poly = Geometry::polygon(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]);
        assert_eq!(pt.dimension(), 0);
        assert_eq!(ls.dimension(), 1);
        assert_eq!(poly.dimension(), 2);
        assert!(pt.bounding_box().is_some());
    }

    #[test]
    fn bbox_distance_to_point() {
        let bb = BoundingBox::from_corners(0.0, 0.0, 2.0, 2.0);
        // Point inside.
        assert_eq!(bb.distance_to_point(Point::new(1.0, 1.0)), 0.0);
        // Point outside.
        let d = bb.distance_to_point(Point::new(3.0, 0.0));
        assert!((d - 1.0).abs() < 1e-9);
    }
}
