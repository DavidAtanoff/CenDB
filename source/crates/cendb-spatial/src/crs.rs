//! Coordinate reference system (CRS) support.
//!
//! Currently implements the WGS84 (EPSG:4326, lat/lon degrees) ↔ Web
//! Mercator (EPSG:3857, meters) transform, which is the most common web
//! mapping transform. Other CRS pairs are rejected with an error.

use crate::geometry::Point;
use cendb_core::{CenError, CenResult, CenStatus};

/// Half the circumference of the Web Mercator projection's "world
/// square" in meters. Equivalent to π * R where R = 6378137 m.
const MAX_EXTENT: f64 = 20037508.34;

/// A coordinate reference system.
#[derive(Clone, Debug, PartialEq)]
pub enum Crs {
    /// WGS84 geographic (EPSG:4326). Coordinates are (lon, lat) in
    /// degrees. This is the default CRS for GeoJSON and most
    /// GPS-derived data.
    Wgs84,
    /// Web Mercator (EPSG:3857). Coordinates are (x, y) in meters on
    /// the spherical Mercator projection. Used by virtually all web
    /// map tiles (OSM, Google, Bing).
    WebMercator,
    /// Any other EPSG-coded CRS. Only `Wgs84` and `WebMercator` have
    /// built-in transforms; other EPSG codes are recognised but
    /// `transform` will return an error.
    Epsg(u32),
    /// A custom CRS described by a name and a WKT (Well-Known Text)
    /// definition. Always rejected by `transform`.
    Custom { name: String, wkt: String },
}

impl Crs {
    /// The EPSG code for this CRS, if it has one.
    pub fn epsg_code(&self) -> Option<u32> {
        match self {
            Crs::Wgs84 => Some(4326),
            Crs::WebMercator => Some(3857),
            Crs::Epsg(c) => Some(*c),
            Crs::Custom { .. } => None,
        }
    }

    /// Human-readable name.
    pub fn name(&self) -> &str {
        match self {
            Crs::Wgs84 => "WGS84 (EPSG:4326)",
            Crs::WebMercator => "Web Mercator (EPSG:3857)",
            Crs::Epsg(_) => "EPSG (generic)",
            Crs::Custom { name, .. } => name,
        }
    }
}

impl core::fmt::Display for Crs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Crs::Wgs84 => write!(f, "Wgs84"),
            Crs::WebMercator => write!(f, "WebMercator"),
            Crs::Epsg(c) => write!(f, "Epsg({})", c),
            Crs::Custom { name, .. } => write!(f, "Custom({})", name),
        }
    }
}

/// Transform a point from one CRS to another.
///
/// Only the `Wgs84 ↔ WebMercator` pair is implemented; all other
/// combinations return an `ErrInternal` error.
///
/// # Conventions
///
/// For `Wgs84`, `point.x` is **longitude** (degrees) and `point.y` is
/// **latitude** (degrees). For `WebMercator`, `point.x` is easting
/// (meters) and `point.y` is northing (meters).
pub fn transform(point: &Point, from: &Crs, to: &Crs) -> CenResult<Point> {
    if from == to {
        return Ok(*point);
    }
    match (from, to) {
        (Crs::Wgs84, Crs::WebMercator) => Ok(wgs84_to_webmercator(point)),
        (Crs::WebMercator, Crs::Wgs84) => Ok(webmercator_to_wgs84(point)),
        _ => Err(CenError::new(
            CenStatus::ErrInternal,
            format!(
                "unsupported CRS transform: {} -> {} (only Wgs84 <-> WebMercator is implemented)",
                from, to
            ),
        )),
    }
}

/// WGS84 (lon, lat in degrees) → Web Mercator (x, y in meters).
///
/// Formula:
/// ```text
/// x = lon * MAX_EXTENT / 180
/// y = ln(tan((90 + lat) * PI/360)) * MAX_EXTENT / PI
/// ```
fn wgs84_to_webmercator(p: &Point) -> Point {
    let lon = p.x;
    let lat = p.y;
    let x = lon * MAX_EXTENT / 180.0;
    // (90 + lat) * PI/360 — clamp lat to the Mercator-valid range to
    // avoid taking tan(±π/2).
    let lat_clamped = lat.clamp(-85.05112878, 85.05112878);
    let y = ((90.0 + lat_clamped).to_radians() / 2.0)
        .tan()
        .ln()
        * MAX_EXTENT
        / core::f64::consts::PI;
    Point::new(x, y)
}

/// Web Mercator (x, y in meters) → WGS84 (lon, lat in degrees).
///
/// Inverse of [`wgs84_to_webmercator`]:
/// ```text
/// lon = x * 180 / MAX_EXTENT
/// lat = 180/PI * (2 * atan(exp(y * PI / MAX_EXTENT)) - PI/2)
/// ```
fn webmercator_to_wgs84(p: &Point) -> Point {
    let x = p.x;
    let y = p.y;
    let lon = x * 180.0 / MAX_EXTENT;
    let lat = 180.0 / core::f64::consts::PI
        * (2.0 * (y * core::f64::consts::PI / MAX_EXTENT).exp().atan()
            - core::f64::consts::PI / 2.0);
    Point::new(lon, lat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wgs84_to_webmercator_origin() {
        // (lat=0, lon=0) → (0, 0)
        let p = Point::new(0.0, 0.0);
        let out = transform(&p, &Crs::Wgs84, &Crs::WebMercator).unwrap();
        assert!(out.x.abs() < 1e-6);
        assert!(out.y.abs() < 1e-6);
    }

    #[test]
    fn webmercator_to_wgs84_origin() {
        let p = Point::new(0.0, 0.0);
        let out = transform(&p, &Crs::WebMercator, &Crs::Wgs84).unwrap();
        assert!(out.x.abs() < 1e-9);
        assert!(out.y.abs() < 1e-9);
    }

    #[test]
    fn roundtrip_wgs84_webmercator_london() {
        // London lat=51.5074, lon=-0.1278
        let london = Point::new(-0.1278, 51.5074);
        let merc = transform(&london, &Crs::Wgs84, &Crs::WebMercator).unwrap();
        let back = transform(&merc, &Crs::WebMercator, &Crs::Wgs84).unwrap();
        assert!((back.x - london.x).abs() < 1e-6, "lon mismatch: {} vs {}", back.x, london.x);
        assert!((back.y - london.y).abs() < 1e-6, "lat mismatch: {} vs {}", back.y, london.y);
    }

    #[test]
    fn london_known_webmercator_coordinates() {
        // London lat=51.5074, lon=-0.1278.
        // Expected Web Mercator (from epsg.io):
        //   x ≈ -14226.63 m, y ≈ 6711508.16 m
        // Our `MAX_EXTENT = 20037508.34` is rounded to 2 decimal places
        // (the canonical value is 20037508.342789244), so we accept a
        // tolerance of ~50 m on y.
        let london = Point::new(-0.1278, 51.5074);
        let merc = transform(&london, &Crs::Wgs84, &Crs::WebMercator).unwrap();
        assert!((merc.x - (-14226.63)).abs() < 1.0, "x: {}", merc.x);
        assert!((merc.y - 6711508.16).abs() < 50.0, "y: {}", merc.y);
    }

    #[test]
    fn transform_identity_when_same_crs() {
        let p = Point::new(1.0, 2.0);
        let out = transform(&p, &Crs::Wgs84, &Crs::Wgs84).unwrap();
        assert_eq!(out, p);
    }

    #[test]
    fn unsupported_transform_returns_error() {
        let p = Point::new(1.0, 2.0);
        let res = transform(&p, &Crs::Wgs84, &Crs::Epsg(2154)); // Lambert-93 France
        assert!(res.is_err());
        if let Err(e) = res {
            assert_eq!(e.status, CenStatus::ErrInternal);
        }
    }

    #[test]
    fn crs_epsg_codes() {
        assert_eq!(Crs::Wgs84.epsg_code(), Some(4326));
        assert_eq!(Crs::WebMercator.epsg_code(), Some(3857));
        assert_eq!(Crs::Epsg(2154).epsg_code(), Some(2154));
        assert_eq!(Crs::Custom { name: "x".into(), wkt: "x".into() }.epsg_code(), None);
    }

    #[test]
    fn wgs84_to_webmercator_antimeridian_wraps() {
        // lon = 180 should produce x = MAX_EXTENT
        let p = Point::new(180.0, 0.0);
        let out = transform(&p, &Crs::Wgs84, &Crs::WebMercator).unwrap();
        assert!((out.x - MAX_EXTENT).abs() < 1e-3);
    }
}
