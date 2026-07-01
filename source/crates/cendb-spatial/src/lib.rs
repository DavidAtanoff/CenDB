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
//! range search (find all rectangles intersecting a query rectangle).

pub mod geohash;
pub mod rtree;

pub use geohash::{encode_geohash, decode_geohash, geohash_bounds, GeohashPrecision};
pub use rtree::{RTree, RTreeEntry, BoundingBox, Point};
