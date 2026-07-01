//! cendb-spatial: geographic and spatial indexing.
//!
//! ## Geohash-based Spatial Index
//!
//! Uses geohash encoding to linearize 2D coordinates (lat, lon) into a
//! 1D string that preserves spatial locality. This allows using the
//! existing ART index for spatial range queries.
//!
//! ## R-Tree
//!
//! A simplified R-tree for bounding-box queries. Supports insertion and
//! range search (find all rectangles intersecting a query rectangle),
//! plus exact-predicate queries against stored [`geometry::Geometry`]
//! values.
//!
//! ## OGC Simple Features geometry model
//!
//! The [`geometry`] module defines the canonical geometry types
//! (`Point`, `LineString`, `Polygon`, `MultiPoint`, `MultiLineString`,
//! `MultiPolygon`, `Geometry` enum, `BoundingBox`).
//!
//! ## OGC spatial predicates
//!
//! The [`predicates`] module implements the DE-9IM-based predicates
//! (`intersects`, `contains`, `within`, `equals`, `disjoint`,
//! `touches`, `crosses`, `overlaps`), plus `distance` and `buffer`.
//!
//! ## Coordinate reference systems
//!
//! The [`crs`] module implements `Wgs84 ↔ WebMercator` transforms.
//!
//! ## Serialization
//!
//! The [`serialize`] module provides WKT, WKB, and GeoJSON
//! serialization for all geometry types.

pub mod crs;
pub mod geohash;
pub mod geometry;
pub mod predicates;
pub mod rtree;
pub mod serialize;

pub use crs::{transform, Crs};
pub use geohash::{decode_geohash, encode_geohash, geohash_bounds, GeohashPrecision};
pub use geometry::{
    BoundingBox, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};
pub use predicates::{
    buffer, contains, crosses, distance, disjoint, equals, intersects, overlaps, touches, within,
};
pub use rtree::{RTree, RTreeEntry};
pub use serialize::{from_geojson, from_wkb, from_wkt, to_geojson, to_wkb, to_wkt};
