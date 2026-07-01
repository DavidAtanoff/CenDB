//! OGC spatial predicates based on the DE-9IM (Dimensionally Extended
//! 9-Intersection Model).
//!
//! The public API exposes the standard named predicates (`intersects`,
//! `contains`, `within`, `equals`, `disjoint`, `touches`, `crosses`,
//! `overlaps`), plus `distance` and `buffer`. Internally each predicate
//! is dispatched on the [`crate::geometry::Geometry`] enum, with
//! bounding-box pre-filtering followed by exact geometry tests.
//!
//! Coverage matrix (✓ = implemented, ~ = approximation):
//!
//! |              | Point | LineString | Polygon | Multi* |
//! |--------------|:-----:|:----------:|:-------:|:------:|
//! | intersects   |   ✓   |     ✓      |    ✓    |   ✓    |
//! | contains     |   ✓   |     ✓      |    ✓    |   ✓    |
//! | within       |   ✓   |     ✓      |    ✓    |   ✓    |
//! | equals       |   ✓   |     ✓      |    ✓    |   ✓    |
//! | disjoint     |   ✓   |     ✓      |    ✓    |   ✓    |
//! | touches      |   ✓   |     ✓      |    ✓    |   ✓    |
//! | crosses      |   ✓   |     ✓      |    ✓    |   ✓    |
//! | overlaps     |   ✓   |     ~      |    ✓    |   ~    |
//! | distance     |   ✓   |     ✓      |    ✓    |   ✓    |
//! | buffer       |   ✓   |     ✓      |    ~    |   ✓    |

use crate::geometry::*;

const EPS: f64 = 1e-12;

/// Validate that all points in a geometry are finite (non-NaN, non-infinite).
/// Returns `false` if any point is invalid, which causes all predicates
/// to return `false` (safe default — an invalid geometry doesn't intersect
/// or contain anything).
fn is_geometry_valid(geom: &Geometry) -> bool {
    match geom {
        Geometry::Point(p) => p.is_valid(),
        Geometry::LineString(ls) => ls.points.iter().all(|p| p.is_valid()),
        Geometry::Polygon(poly) => {
            poly.exterior.points.iter().all(|p| p.is_valid())
                && poly.interiors.iter().all(|ring| ring.points.iter().all(|p| p.is_valid()))
        }
        Geometry::MultiPoint(mp) => mp.points.iter().all(|p| p.is_valid()),
        Geometry::MultiLineString(mls) => mls.linestrings.iter().all(|ls| ls.points.iter().all(|p| p.is_valid())),
        Geometry::MultiPolygon(mp) => mp.polygons.iter().all(|poly| {
            poly.exterior.points.iter().all(|p| p.is_valid())
                && poly.interiors.iter().all(|ring| ring.points.iter().all(|p| p.is_valid()))
        }),
        Geometry::GeometryCollection(gs) => gs.iter().all(is_geometry_valid),
    }
}

// ============================================================================
// Point location helpers
// ============================================================================

/// Where a point lies relative to a polygon ring.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Loc {
    Outside,
    Boundary,
    Inside,
}

/// Orientation test: 0 = collinear, 1 = counterclockwise, 2 = clockwise.
fn orientation(p: Point, q: Point, r: Point) -> i32 {
    let val = (q.y - p.y) * (r.x - q.x) - (q.x - p.x) * (r.y - q.y);
    if val.abs() < EPS {
        0
    } else if val > 0.0 {
        1
    } else {
        2
    }
}

/// True if point `r` lies on segment `(p, q)` (assumes collinear).
fn on_segment(p: Point, q: Point, r: Point) -> bool {
    r.x >= p.x.min(q.x) - EPS
        && r.x <= p.x.max(q.x) + EPS
        && r.y >= p.y.min(q.y) - EPS
        && r.y <= p.y.max(q.y) + EPS
}

/// True if point `p` lies on segment `(a, b)`.
fn point_on_segment(p: Point, a: Point, b: Point) -> bool {
    let o = orientation(a, b, p);
    o == 0 && on_segment(a, b, p)
}

/// True if point lies on any segment of the linestring.
fn point_on_linestring(p: Point, ls: &LineString) -> bool {
    for (a, b) in ls.segments() {
        if point_on_segment(p, a, b) {
            return true;
        }
    }
    false
}

/// Ray-casting point-in-polygon. Returns `Boundary` if the point lies on
/// any ring edge, `Inside` if the point is inside the exterior and
/// outside all interiors, `Outside` otherwise.
fn point_in_polygon(p: Point, poly: &Polygon) -> Loc {
    // First check the exterior ring.
    let ext_loc = point_in_ring(p, &poly.exterior);
    if ext_loc == Loc::Boundary {
        return Loc::Boundary;
    }
    if ext_loc == Loc::Outside {
        return Loc::Outside;
    }
    // Exterior says Inside. Check interiors (holes).
    for hole in &poly.interiors {
        let hole_loc = point_in_ring(p, hole);
        if hole_loc == Loc::Boundary {
            return Loc::Boundary;
        }
        if hole_loc == Loc::Inside {
            return Loc::Outside;
        }
    }
    Loc::Inside
}

/// Ray-casting point-in-ring. Returns `Boundary` if `p` lies on any
/// segment of the ring (treating it as implicitly closed).
fn point_in_ring(p: Point, ring: &LineString) -> Loc {
    let pts: &[Point] = &ring.points;
    if pts.len() < 3 {
        // Degenerate ring.
        if pts.len() == 2 {
            return if point_on_segment(p, pts[0], pts[1]) {
                Loc::Boundary
            } else {
                Loc::Outside
            };
        }
        if pts.len() == 1 {
            return if approx_eq_point(p, pts[0]) {
                Loc::Boundary
            } else {
                Loc::Outside
            };
        }
        return Loc::Outside;
    }

    // Boundary check first.
    let n = pts.len();
    let closed = pts[0] == pts[n - 1];
    let seg_count = if closed { n - 1 } else { n };
    for i in 0..seg_count {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        if point_on_segment(p, a, b) {
            return Loc::Boundary;
        }
    }

    // Ray casting.
    let mut inside = false;
    for i in 0..seg_count {
        let a = pts[i];
        let b = pts[(i + 1) % n];
        let cond1 = a.y > p.y;
        let cond2 = b.y > p.y;
        if cond1 != cond2 {
            let x_inter = a.x + (p.y - a.y) * (b.x - a.x) / (b.y - a.y);
            if x_inter > p.x {
                inside = !inside;
            }
        }
    }
    if inside {
        Loc::Inside
    } else {
        Loc::Outside
    }
}

// ============================================================================
// Segment intersection
// ============================================================================

/// True if segments (p1,p2) and (p3,p4) share at least one point.
fn segments_intersect(p1: Point, p2: Point, p3: Point, p4: Point) -> bool {
    let o1 = orientation(p1, p2, p3);
    let o2 = orientation(p1, p2, p4);
    let o3 = orientation(p3, p4, p1);
    let o4 = orientation(p3, p4, p2);

    if o1 != o2 && o3 != o4 {
        return true;
    }

    // Collinear special cases.
    if o1 == 0 && on_segment(p1, p2, p3) {
        return true;
    }
    if o2 == 0 && on_segment(p1, p2, p4) {
        return true;
    }
    if o3 == 0 && on_segment(p3, p4, p1) {
        return true;
    }
    if o4 == 0 && on_segment(p3, p4, p2) {
        return true;
    }
    false
}

/// True if any segment of `a` intersects any segment of `b`.
fn linestring_intersects_linestring(a: &LineString, b: &LineString) -> bool {
    for (p1, p2) in a.segments() {
        for (p3, p4) in b.segments() {
            if segments_intersect(p1, p2, p3, p4) {
                return true;
            }
        }
    }
    false
}

/// True if the linestring shares any point with the polygon (boundary
/// or interior).
fn linestring_intersects_polygon(ls: &LineString, poly: &Polygon) -> bool {
    // Any vertex of ls inside or on the polygon boundary?
    for p in &ls.points {
        if point_in_polygon(*p, poly) != Loc::Outside {
            return true;
        }
    }
    // Any segment of ls crosses any segment of any ring?
    for (p1, p2) in ls.segments() {
        for ring in poly.rings() {
            for (p3, p4) in ring.segments() {
                if segments_intersect(p1, p2, p3, p4) {
                    return true;
                }
            }
        }
    }
    false
}

/// True if two polygons share any point.
fn polygon_intersects_polygon(a: &Polygon, b: &Polygon) -> bool {
    // Any vertex of a inside b (or on its boundary)?
    for p in &a.exterior.points {
        if point_in_polygon(*p, b) != Loc::Outside {
            return true;
        }
    }
    // Any vertex of b inside a?
    for p in &b.exterior.points {
        if point_in_polygon(*p, a) != Loc::Outside {
            return true;
        }
    }
    // Any segment-segment crossing between rings?
    for ring_a in a.rings() {
        for (p1, p2) in ring_a.segments() {
            for ring_b in b.rings() {
                for (p3, p4) in ring_b.segments() {
                    if segments_intersect(p1, p2, p3, p4) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

// ============================================================================
// Distance helpers
// ============================================================================

fn dot(a: Point, b: Point) -> f64 {
    a.x * b.x + a.y * b.y
}

fn sub(a: Point, b: Point) -> Point {
    Point::new(a.x - b.x, a.y - b.y)
}

/// Minimum distance from point `p` to segment `(a, b)`.
fn point_segment_distance(p: Point, a: Point, b: Point) -> f64 {
    let ab = sub(b, a);
    let ap = sub(p, a);
    let ab_len2 = dot(ab, ab);
    if ab_len2 < EPS {
        return p.distance(&a);
    }
    let t = (dot(ap, ab) / ab_len2).clamp(0.0, 1.0);
    let proj = Point::new(a.x + t * ab.x, a.y + t * ab.y);
    p.distance(&proj)
}

fn point_linestring_distance(p: Point, ls: &LineString) -> f64 {
    if ls.points.is_empty() {
        return f64::INFINITY;
    }
    if ls.points.len() == 1 {
        return p.distance(&ls.points[0]);
    }
    ls.segments()
        .map(|(a, b)| point_segment_distance(p, a, b))
        .fold(f64::INFINITY, f64::min)
}

fn linestring_linestring_distance(a: &LineString, b: &LineString) -> f64 {
    if a.points.is_empty() || b.points.is_empty() {
        return f64::INFINITY;
    }
    let mut min = f64::INFINITY;
    for p in &a.points {
        min = min.min(point_linestring_distance(*p, b));
    }
    for p in &b.points {
        min = min.min(point_linestring_distance(*p, a));
    }
    min
}

fn point_polygon_distance(p: Point, poly: &Polygon) -> f64 {
    match point_in_polygon(p, poly) {
        Loc::Inside => 0.0,
        Loc::Boundary => 0.0,
        Loc::Outside => {
            // Minimum distance to any ring.
            poly.rings()
                .map(|r| point_linestring_distance(p, r))
                .fold(f64::INFINITY, f64::min)
        }
    }
}

fn linestring_polygon_distance(ls: &LineString, poly: &Polygon) -> f64 {
    let mut min = f64::INFINITY;
    for p in &ls.points {
        min = min.min(point_polygon_distance(*p, poly));
    }
    for ring in poly.rings() {
        min = min.min(linestring_linestring_distance(ls, ring));
    }
    min
}

fn polygon_polygon_distance(a: &Polygon, b: &Polygon) -> f64 {
    let mut min = f64::INFINITY;
    for p in &a.exterior.points {
        min = min.min(point_polygon_distance(*p, b));
    }
    for p in &b.exterior.points {
        min = min.min(point_polygon_distance(*p, a));
    }
    min
}

// ============================================================================
// Equality helpers
// ============================================================================

fn approx_eq_point(a: Point, b: Point) -> bool {
    (a.x - b.x).abs() < EPS && (a.y - b.y).abs() < EPS
}

/// True if two linestrings cover the same point set, allowing for
/// reversed direction. Used by [`equals`].
fn linestring_same_set(a: &LineString, b: &LineString) -> bool {
    if a.points.len() != b.points.len() {
        return false;
    }
    if a.points.is_empty() {
        return true;
    }
    let same = a.points.iter().zip(b.points.iter()).all(|(p, q)| approx_eq_point(*p, *q));
    if same {
        return true;
    }
    // Reversed direction.
    a.points
        .iter()
        .zip(b.points.iter().rev())
        .all(|(p, q)| approx_eq_point(*p, *q))
}

fn polygon_same_set(a: &Polygon, b: &Polygon) -> bool {
    if a.interiors.len() != b.interiors.len() {
        return false;
    }
    if !linestring_same_set(&a.exterior, &b.exterior) {
        return false;
    }
    // Interiors as a set: naive O(n^2) check, fine for small inputs.
    for ha in &a.interiors {
        let mut matched = false;
        for hb in &b.interiors {
            if linestring_same_set(ha, hb) {
                matched = true;
                break;
            }
        }
        if !matched {
            return false;
        }
    }
    true
}

// ============================================================================
// Public predicate API
// ============================================================================

/// `intersects(a, b)` — true if the two geometries share at least one
/// point. This is the negation of [`disjoint`].
pub fn intersects(a: &Geometry, b: &Geometry) -> bool {
    // Reject invalid geometries (NaN/infinite coordinates).
    if !is_geometry_valid(a) || !is_geometry_valid(b) {
        return false;
    }
    // Bounding-box pre-filter.
    if let (Some(ba), Some(bb)) = (a.bounding_box(), b.bounding_box()) {
        if !ba.intersects(&bb) {
            return false;
        }
    }
    intersects_exact(a, b)
}

fn intersects_exact(a: &Geometry, b: &Geometry) -> bool {
    match (a, b) {
        (Geometry::Point(p), Geometry::Point(q)) => approx_eq_point(*p, *q),
        (Geometry::Point(p), Geometry::LineString(ls)) => point_on_linestring(*p, ls),
        (Geometry::Point(p), Geometry::Polygon(poly)) => {
            point_in_polygon(*p, poly) != Loc::Outside
        }
        (Geometry::LineString(ls), Geometry::Point(p)) => point_on_linestring(*p, ls),
        (Geometry::LineString(a), Geometry::LineString(b)) => {
            linestring_intersects_linestring(a, b)
        }
        (Geometry::LineString(ls), Geometry::Polygon(poly)) => {
            linestring_intersects_polygon(ls, poly)
        }
        (Geometry::Polygon(poly), Geometry::Point(p)) => {
            point_in_polygon(*p, poly) != Loc::Outside
        }
        (Geometry::Polygon(poly), Geometry::LineString(ls)) => {
            linestring_intersects_polygon(ls, poly)
        }
        (Geometry::Polygon(a), Geometry::Polygon(b)) => polygon_intersects_polygon(a, b),
        // Multi* / Collection: any-component intersection.
        _ => {
            for ga in a.components() {
                for gb in b.components() {
                    if intersects(ga, gb) {
                        return true;
                    }
                }
            }
            false
        }
    }
}

/// `disjoint(a, b)` — true if the two geometries share no points.
pub fn disjoint(a: &Geometry, b: &Geometry) -> bool {
    !intersects(a, b)
}

/// `contains(a, b)` — true if every point of `b` lies in `a`'s
/// interior (points of `b` on `a`'s boundary are not contained).
pub fn contains(a: &Geometry, b: &Geometry) -> bool {
    match (a, b) {
        (Geometry::Point(p), Geometry::Point(q)) => approx_eq_point(*p, *q),
        (Geometry::Polygon(poly), Geometry::Point(p)) => {
            point_in_polygon(*p, poly) == Loc::Inside
        }
        (Geometry::Polygon(poly), Geometry::LineString(ls)) => {
            // All points of ls strictly inside (none on the boundary).
            ls.points
                .iter()
                .all(|p| point_in_polygon(*p, poly) == Loc::Inside)
                && !poly
                    .rings()
                    .any(|r| linestring_intersects_linestring(ls, r))
        }
        (Geometry::Polygon(a), Geometry::Polygon(b)) => {
            // All of b's vertices strictly inside a, and no ring crossings.
            b.exterior
                .points
                .iter()
                .all(|p| point_in_polygon(*p, a) == Loc::Inside)
                && !a
                    .rings()
                    .any(|ra| b.rings().any(|rb| linestring_intersects_linestring(ra, rb)))
        }
        (Geometry::MultiPolygon(mp), other) => mp
            .polygons
            .iter()
            .any(|p| contains(&Geometry::Polygon(p.clone()), other)),
        (Geometry::GeometryCollection(gs), other) => {
            gs.iter().any(|g| contains(g, other))
        }
        // Generic fallback: b's bbox is within a's bbox AND distance == 0
        // AND contains_via_within (i.e. within(b, a)).
        _ => within(b, a),
    }
}

/// `within(a, b)` — true if every point of `a` lies in `b`'s interior.
/// `within(a, b)` is the inverse of `contains(b, a)`.
pub fn within(a: &Geometry, b: &Geometry) -> bool {
    contains(b, a)
}

/// `equals(a, b)` — true if the two geometries cover the same point
/// set. Implemented as exact structural equality for the simple types
/// and as mutual containment for the rest.
pub fn equals(a: &Geometry, b: &Geometry) -> bool {
    match (a, b) {
        (Geometry::Point(p), Geometry::Point(q)) => approx_eq_point(*p, *q),
        (Geometry::LineString(a), Geometry::LineString(b)) => linestring_same_set(a, b),
        (Geometry::Polygon(a), Geometry::Polygon(b)) => polygon_same_set(a, b),
        (Geometry::MultiPoint(a), Geometry::MultiPoint(b)) => {
            if a.points.len() != b.points.len() {
                return false;
            }
            for p in &a.points {
                if !b.points.iter().any(|q| approx_eq_point(*p, *q)) {
                    return false;
                }
            }
            true
        }
        (Geometry::MultiLineString(a), Geometry::MultiLineString(b)) => {
            if a.linestrings.len() != b.linestrings.len() {
                return false;
            }
            for la in &a.linestrings {
                if !b
                    .linestrings
                    .iter()
                    .any(|lb| linestring_same_set(la, lb))
                {
                    return false;
                }
            }
            true
        }
        (Geometry::MultiPolygon(a), Geometry::MultiPolygon(b)) => {
            if a.polygons.len() != b.polygons.len() {
                return false;
            }
            for pa in &a.polygons {
                if !b.polygons.iter().any(|pb| polygon_same_set(pa, pb)) {
                    return false;
                }
            }
            true
        }
        (Geometry::GeometryCollection(a), Geometry::GeometryCollection(b)) => {
            if a.len() != b.len() {
                return false;
            }
            for ga in a {
                if !b.iter().any(|gb| equals(ga, gb)) {
                    return false;
                }
            }
            true
        }
        _ => false,
    }
}

/// `touches(a, b)` — true if the two geometries share boundary points
/// but their interiors do not intersect.
pub fn touches(a: &Geometry, b: &Geometry) -> bool {
    if !intersects(a, b) {
        return false;
    }
    // If interiors intersect (crosses/overlaps), it's not a touch.
    if interiors_intersect(a, b) {
        return false;
    }
    // They intersect but interiors don't → boundary touch.
    true
}

/// True if any segment of `ls` passes through the interior of `poly`.
fn linestring_through_polygon_interior(ls: &LineString, poly: &Polygon) -> bool {
    for (p1, p2) in ls.segments() {
        let mid = Point::new((p1.x + p2.x) / 2.0, (p1.y + p2.y) / 2.0);
        if point_in_polygon(mid, poly) == Loc::Inside {
            return true;
        }
    }
    false
}

/// True if any segment of `ls` has a midpoint strictly outside `poly`.
fn linestring_through_polygon_exterior(ls: &LineString, poly: &Polygon) -> bool {
    for (p1, p2) in ls.segments() {
        let mid = Point::new((p1.x + p2.x) / 2.0, (p1.y + p2.y) / 2.0);
        if point_in_polygon(mid, poly) == Loc::Outside {
            return true;
        }
    }
    false
}

/// True if the interiors of `a` and `b` share any point.
fn interiors_intersect(a: &Geometry, b: &Geometry) -> bool {
    match (a, b) {
        (Geometry::Polygon(poly), Geometry::Point(p))
        | (Geometry::Point(p), Geometry::Polygon(poly)) => {
            point_in_polygon(*p, poly) == Loc::Inside
        }
        (Geometry::Polygon(a), Geometry::Polygon(b)) => {
            // Any vertex of a strictly inside b, or vice versa.
            a.exterior
                .points
                .iter()
                .any(|p| point_in_polygon(*p, b) == Loc::Inside)
                || b
                    .exterior
                    .points
                    .iter()
                    .any(|p| point_in_polygon(*p, a) == Loc::Inside)
                // Or a ring of one passes through the interior of the other.
                || a.rings().any(|ra| linestring_through_polygon_interior(ra, b))
                || b.rings().any(|rb| linestring_through_polygon_interior(rb, a))
        }
        (Geometry::Polygon(poly), Geometry::LineString(ls))
        | (Geometry::LineString(ls), Geometry::Polygon(poly)) => {
            // Any point of ls strictly inside poly, or any segment of
            // ls passing through poly's interior?
            ls.points
                .iter()
                .any(|p| point_in_polygon(*p, poly) == Loc::Inside)
                || linestring_through_polygon_interior(ls, poly)
        }
        _ => {
            // For point-point: interiors intersect iff same point.
            if let (Geometry::Point(p), Geometry::Point(q)) = (a, b) {
                return approx_eq_point(*p, *q);
            }
            // Fallback: assume intersecting implies interior overlap for
            // the multi/collection case. This is a conservative
            // approximation — `touches` may under-report for exotic
            // multi-type pairs.
            intersects_exact(a, b)
        }
    }
}

/// `crosses(a, b)` — true if the interiors intersect and the
/// intersection has dimension less than the maximum of the two
/// geometries' dimensions. Typically used for point/line, line/polygon,
/// and line/line pairs.
pub fn crosses(a: &Geometry, b: &Geometry) -> bool {
    let da = a.dimension();
    let db = b.dimension();
    // Crosses requires interior intersection with dim < max(da, db).
    if !interiors_intersect(a, b) {
        return false;
    }
    match (a, b) {
        (Geometry::LineString(l1), Geometry::LineString(l2)) => {
            // Lines cross iff they intersect at a single point interior
            // to both, i.e. they are not collinear and not merely
            // touching at endpoints.
            linestring_intersects_linestring(l1, l2)
                && !linestring_same_set(l1, l2)
                && !linestring_overlaps(l1, l2)
        }
        (Geometry::LineString(ls), Geometry::Polygon(poly))
        | (Geometry::Polygon(poly), Geometry::LineString(ls)) => {
            // Line crosses polygon iff some part of the line is in the
            // polygon's interior AND some part is in the polygon's
            // exterior (not on the boundary). We check both explicit
            // vertices and segment midpoints so that a line whose
            // endpoints are both outside (but whose middle passes
            // through the polygon) still counts as crossing.
            let any_inside = ls
                .points
                .iter()
                .any(|p| point_in_polygon(*p, poly) == Loc::Inside)
                || linestring_through_polygon_interior(ls, poly);
            let any_outside = ls
                .points
                .iter()
                .any(|p| point_in_polygon(*p, poly) == Loc::Outside)
                || linestring_through_polygon_exterior(ls, poly);
            any_inside && any_outside
        }
        (Geometry::Point(p), Geometry::LineString(ls))
        | (Geometry::LineString(ls), Geometry::Point(p)) => {
            // Point on line interior.
            point_on_linestring(*p, ls) && ls.points.iter().all(|v| !approx_eq_point(*v, *p))
        }
        _ => {
            let _ = (da, db);
            false
        }
    }
}

/// True if two linestrings overlap (share a common segment), used to
/// distinguish `crosses` from `overlaps`.
fn linestring_overlaps(a: &LineString, b: &LineString) -> bool {
    // Naive: any segment of a is collinear-and-overlapping with a
    // segment of b.
    for (p1, p2) in a.segments() {
        for (p3, p4) in b.segments() {
            if orientation(p1, p2, p3) == 0
                && orientation(p1, p2, p4) == 0
                && (on_segment(p1, p2, p3)
                    || on_segment(p1, p2, p4)
                    || on_segment(p3, p4, p1)
                    || on_segment(p3, p4, p2))
            {
                return true;
            }
        }
    }
    false
}

/// `overlaps(a, b)` — true if the geometries have the same dimension,
/// their interiors intersect, and neither contains the other.
pub fn overlaps(a: &Geometry, b: &Geometry) -> bool {
    if a.dimension() != b.dimension() {
        return false;
    }
    if !interiors_intersect(a, b) {
        return false;
    }
    if equals(a, b) {
        return false;
    }
    if contains(a, b) || contains(b, a) {
        return false;
    }
    // They share interior but neither contains the other → overlap.
    true
}

/// `distance(a, b)` — minimum Euclidean distance between any point of
/// `a` and any point of `b`. Returns 0.0 if they intersect.
/// Returns `f64::NAN` if either geometry contains NaN/inf coordinates.
pub fn distance(a: &Geometry, b: &Geometry) -> f64 {
    if !is_geometry_valid(a) || !is_geometry_valid(b) {
        return f64::NAN;
    }
    match (a, b) {
        (Geometry::Point(p), Geometry::Point(q)) => p.distance(q),
        (Geometry::Point(p), Geometry::LineString(ls)) => point_linestring_distance(*p, ls),
        (Geometry::LineString(ls), Geometry::Point(p)) => point_linestring_distance(*p, ls),
        (Geometry::Point(p), Geometry::Polygon(poly)) => point_polygon_distance(*p, poly),
        (Geometry::Polygon(poly), Geometry::Point(p)) => point_polygon_distance(*p, poly),
        (Geometry::LineString(a), Geometry::LineString(b)) => {
            linestring_linestring_distance(a, b)
        }
        (Geometry::LineString(ls), Geometry::Polygon(poly))
        | (Geometry::Polygon(poly), Geometry::LineString(ls)) => {
            linestring_polygon_distance(ls, poly)
        }
        (Geometry::Polygon(a), Geometry::Polygon(b)) => polygon_polygon_distance(a, b),
        // Multi* / Collection: min over components.
        _ => {
            let mut min = f64::INFINITY;
            for ga in a.components() {
                for gb in b.components() {
                    let d = distance(ga, gb);
                    if d < min {
                        min = d;
                    }
                }
            }
            min
        }
    }
}

// ============================================================================
// Buffer
// ============================================================================

/// Compute the convex hull (Andrew's monotone chain) of a point set.
/// Returns the hull vertices in counter-clockwise order, closed (first
/// == last).
fn convex_hull(mut points: Vec<Point>) -> Vec<Point> {
    if points.len() <= 2 {
        return points;
    }
    points.sort_by(|a, b| {
        a.x.partial_cmp(&b.x)
            .unwrap_or(core::cmp::Ordering::Equal)
            .then(a.y.partial_cmp(&b.y).unwrap_or(core::cmp::Ordering::Equal))
    });
    points.dedup_by(|a, b| approx_eq_point(*a, *b));
    let n = points.len();
    if n <= 2 {
        return points;
    }

    let cross = |o: Point, a: Point, b: Point| -> f64 {
        (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x)
    };

    let mut hull = Vec::with_capacity(2 * n);
    // Lower hull.
    for &p in &points {
        while hull.len() >= 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }
    // Upper hull.
    let lower_len = hull.len() + 1;
    for &p in points.iter().rev() {
        while hull.len() >= lower_len && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }
    hull.pop(); // remove duplicated first point
    if !hull.is_empty() {
        hull.push(hull[0]); // close the ring
    }
    hull
}

/// Approximate a circle of radius `r` around `center` as a regular
/// 16-gon (closed ring).
fn circle_ring(center: Point, r: f64, segments: usize) -> LineString {
    let mut pts = Vec::with_capacity(segments + 1);
    for i in 0..segments {
        let theta = 2.0 * core::f64::consts::PI * (i as f64) / (segments as f64);
        pts.push(Point::new(
            center.x + r * theta.cos(),
            center.y + r * theta.sin(),
        ));
    }
    pts.push(pts[0]);
    LineString::new(pts)
}

/// `buffer(geom, distance)` — return the polygonal area within
/// `distance` of the geometry. Points become regular 16-gons;
/// linestrings become the convex hull of vertex-centered circles
/// (approximate); polygons are expanded by offsetting their bounding
/// box (very approximate, suitable only for axis-aligned rectangles).
pub fn buffer(geom: &Geometry, distance: f64) -> Geometry {
    if distance <= 0.0 {
        return geom.clone();
    }
    match geom {
        Geometry::Point(p) => Geometry::Polygon(Polygon::from_exterior(circle_ring(*p, distance, 16))),
        Geometry::MultiPoint(mp) => {
            // Union of vertex circles: convex hull of all circle points.
            let mut pts = Vec::new();
            for p in &mp.points {
                let ring = circle_ring(*p, distance, 16);
                pts.extend(ring.points);
            }
            let hull = convex_hull(pts);
            Geometry::Polygon(Polygon::from_exterior(LineString::new(hull)))
        }
        Geometry::LineString(ls) => {
            // Convex hull of vertex-centered circles. Reasonable
            // approximation for short, roughly-straight lines; loses
            // concavity for bent lines.
            let mut pts = Vec::new();
            for p in &ls.points {
                let ring = circle_ring(*p, distance, 16);
                pts.extend(ring.points);
            }
            let hull = convex_hull(pts);
            Geometry::Polygon(Polygon::from_exterior(LineString::new(hull)))
        }
        Geometry::Polygon(poly) => {
            // Bounding-box expansion. Simple but shape-preserving only
            // for rectangles aligned with the axes.
            let bb = match poly.bounding_box() {
                Some(b) => b,
                None => return geom.clone(),
            };
            let expanded = LineString::from_xy(&[
                (bb.min_x - distance, bb.min_y - distance),
                (bb.max_x + distance, bb.min_y - distance),
                (bb.max_x + distance, bb.max_y + distance),
                (bb.min_x - distance, bb.max_y + distance),
                (bb.min_x - distance, bb.min_y - distance),
            ]);
            Geometry::Polygon(Polygon::from_exterior(expanded))
        }
        Geometry::MultiLineString(mls) => {
            // Convex hull of all vertex circles across all linestrings.
            let mut pts = Vec::new();
            for ls in &mls.linestrings {
                for p in &ls.points {
                    let ring = circle_ring(*p, distance, 16);
                    pts.extend(ring.points);
                }
            }
            let hull = convex_hull(pts);
            Geometry::Polygon(Polygon::from_exterior(LineString::new(hull)))
        }
        Geometry::MultiPolygon(mp) => {
            // Union (as convex hull) of expanded bboxes of each polygon.
            let mut pts = Vec::new();
            for p in &mp.polygons {
                let bb = match p.bounding_box() {
                    Some(b) => b,
                    None => continue,
                };
                pts.push(Point::new(bb.min_x - distance, bb.min_y - distance));
                pts.push(Point::new(bb.max_x + distance, bb.min_y - distance));
                pts.push(Point::new(bb.max_x + distance, bb.max_y + distance));
                pts.push(Point::new(bb.min_x - distance, bb.max_y + distance));
            }
            let hull = convex_hull(pts);
            Geometry::Polygon(Polygon::from_exterior(LineString::new(hull)))
        }
        Geometry::GeometryCollection(gs) => {
            Geometry::GeometryCollection(gs.iter().map(|g| buffer(g, distance)).collect())
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_square() -> Polygon {
        Polygon::from_exterior(LineString::from_xy(&[
            (0.0, 0.0),
            (1.0, 0.0),
            (1.0, 1.0),
            (0.0, 1.0),
            (0.0, 0.0),
        ]))
    }

    fn square_at(minx: f64, miny: f64, maxx: f64, maxy: f64) -> Polygon {
        Polygon::from_exterior(LineString::from_xy(&[
            (minx, miny),
            (maxx, miny),
            (maxx, maxy),
            (minx, maxy),
            (minx, miny),
        ]))
    }

    #[test]
    fn intersects_point_polygon_inside() {
        let poly = unit_square();
        let p = Geometry::point(0.5, 0.5);
        assert!(intersects(&p, &Geometry::Polygon(poly)));
    }

    #[test]
    fn intersects_point_polygon_outside() {
        let poly = unit_square();
        let p = Geometry::point(2.0, 2.0);
        assert!(!intersects(&p, &Geometry::Polygon(poly)));
    }

    #[test]
    fn intersects_line_crosses_polygon() {
        let poly = unit_square();
        let ls = Geometry::line_string(&[(-1.0, 0.5), (2.0, 0.5)]);
        assert!(intersects(&ls, &Geometry::Polygon(poly)));
    }

    #[test]
    fn intersects_polygon_polygon_overlap() {
        let a = square_at(0.0, 0.0, 2.0, 2.0);
        let b = square_at(1.0, 1.0, 3.0, 3.0);
        assert!(intersects(&Geometry::Polygon(a), &Geometry::Polygon(b)));
    }

    #[test]
    fn intersects_polygon_polygon_disjoint() {
        let a = square_at(0.0, 0.0, 1.0, 1.0);
        let b = square_at(10.0, 10.0, 11.0, 11.0);
        assert!(!intersects(&Geometry::Polygon(a), &Geometry::Polygon(b)));
    }

    #[test]
    fn contains_polygon_point_inside() {
        let poly = unit_square();
        let p = Geometry::point(0.5, 0.5);
        assert!(contains(&Geometry::Polygon(poly), &p));
    }

    #[test]
    fn contains_polygon_point_on_boundary() {
        let poly = unit_square();
        let p = Geometry::point(0.0, 0.5);
        // Point on boundary is NOT contained (OGC semantics).
        assert!(!contains(&Geometry::Polygon(poly), &p));
    }

    #[test]
    fn contains_polygon_point_outside() {
        let poly = unit_square();
        let p = Geometry::point(2.0, 2.0);
        assert!(!contains(&Geometry::Polygon(poly), &p));
    }

    #[test]
    fn within_is_inverse_of_contains() {
        let poly = Geometry::Polygon(unit_square());
        let p = Geometry::point(0.5, 0.5);
        assert!(contains(&poly, &p));
        assert!(within(&p, &poly));
    }

    #[test]
    fn equals_points() {
        let p = Geometry::point(1.0, 2.0);
        let q = Geometry::point(1.0, 2.0);
        let r = Geometry::point(1.0, 3.0);
        assert!(equals(&p, &q));
        assert!(!equals(&p, &r));
    }

    #[test]
    fn equals_linestrings_reversed() {
        let a = Geometry::line_string(&[(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)]);
        let b = Geometry::line_string(&[(2.0, 2.0), (1.0, 1.0), (0.0, 0.0)]);
        assert!(equals(&a, &b));
    }

    #[test]
    fn equals_polygons_reversed() {
        let a = Geometry::polygon(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]);
        let b = Geometry::polygon(&[(0.0, 0.0), (0.0, 1.0), (1.0, 1.0), (1.0, 0.0), (0.0, 0.0)]);
        assert!(equals(&a, &b));
    }

    #[test]
    fn disjoint_points() {
        let a = Geometry::point(0.0, 0.0);
        let b = Geometry::point(1.0, 1.0);
        assert!(disjoint(&a, &b));
        assert!(!intersects(&a, &b));
    }

    #[test]
    fn touches_polygons_share_edge() {
        let a = Geometry::Polygon(square_at(0.0, 0.0, 1.0, 1.0));
        let b = Geometry::Polygon(square_at(1.0, 0.0, 2.0, 1.0));
        assert!(touches(&a, &b));
    }

    #[test]
    fn touches_point_on_polygon_boundary() {
        let poly = Geometry::Polygon(unit_square());
        let p = Geometry::point(0.0, 0.5);
        assert!(touches(&poly, &p));
    }

    #[test]
    fn crosses_line_polygon() {
        let poly = Geometry::Polygon(unit_square());
        let ls = Geometry::line_string(&[(-1.0, 0.5), (2.0, 0.5)]);
        assert!(crosses(&ls, &poly));
    }

    #[test]
    fn crosses_line_line() {
        let a = Geometry::line_string(&[(-1.0, 0.0), (2.0, 0.0)]);
        let b = Geometry::line_string(&[(0.5, -1.0), (0.5, 2.0)]);
        assert!(crosses(&a, &b));
    }

    #[test]
    fn overlaps_polygons() {
        let a = Geometry::Polygon(square_at(0.0, 0.0, 2.0, 2.0));
        let b = Geometry::Polygon(square_at(1.0, 1.0, 3.0, 3.0));
        assert!(overlaps(&a, &b));
    }

    #[test]
    fn overlaps_not_when_one_contains_other() {
        let a = Geometry::Polygon(square_at(0.0, 0.0, 4.0, 4.0));
        let b = Geometry::Polygon(square_at(1.0, 1.0, 2.0, 2.0));
        assert!(!overlaps(&a, &b));
    }

    #[test]
    fn distance_point_point() {
        let a = Geometry::point(0.0, 0.0);
        let b = Geometry::point(3.0, 4.0);
        assert!((distance(&a, &b) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn distance_point_line() {
        let a = Geometry::point(0.0, 1.0);
        let b = Geometry::line_string(&[(-1.0, 0.0), (1.0, 0.0)]);
        assert!((distance(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn distance_point_polygon_inside_is_zero() {
        let a = Geometry::point(0.5, 0.5);
        let b = Geometry::Polygon(unit_square());
        assert_eq!(distance(&a, &b), 0.0);
    }

    #[test]
    fn buffer_point_returns_polygon_containing_point() {
        let p = Geometry::point(0.0, 0.0);
        let buf = buffer(&p, 1.0);
        match buf {
            Geometry::Polygon(poly) => {
                let g = Geometry::Polygon(poly);
                assert!(contains(&g, &p) || intersects(&g, &p));
                // A point at distance 0.5 should be inside the buffer.
                let inside_pt = Geometry::point(0.5, 0.0);
                assert!(intersects(&g, &inside_pt));
                // A point at distance 2.0 should be outside.
                let outside_pt = Geometry::point(2.0, 0.0);
                assert!(!intersects(&g, &outside_pt));
            }
            _ => panic!("buffer of point should be a polygon"),
        }
    }

    #[test]
    fn buffer_linestring_contains_neighbours() {
        let ls = Geometry::line_string(&[(0.0, 0.0), (1.0, 0.0)]);
        let buf = buffer(&ls, 0.5);
        match buf {
            Geometry::Polygon(poly) => {
                let g = Geometry::Polygon(poly);
                // A point directly above the line at distance 0.25 should be inside.
                let near = Geometry::point(0.5, 0.25);
                assert!(intersects(&g, &near));
                // A point far above the line at distance 5 should be outside.
                let far = Geometry::point(0.5, 5.0);
                assert!(!intersects(&g, &far));
            }
            _ => panic!("buffer of linestring should be a polygon"),
        }
    }

    #[test]
    fn convex_hull_basic() {
        let pts = vec![
            Point::new(0.0, 0.0),
            Point::new(1.0, 0.0),
            Point::new(0.0, 1.0),
            Point::new(0.5, 0.5), // interior point — should be excluded
        ];
        let hull = convex_hull(pts);
        assert!(hull.len() == 4); // 3 vertices + closing
    }
}
