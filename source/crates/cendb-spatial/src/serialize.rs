//! Serialization for OGC geometries: WKT (Well-Known Text), WKB
//! (Well-Known Binary), and GeoJSON.
//!
//! All three formats support round-tripping for the basic geometry
//! types: `Point`, `LineString`, `Polygon`, `MultiPoint`,
//! `MultiLineString`, `MultiPolygon`, and `GeometryCollection`.

use crate::geometry::*;
use cendb_core::{CenError, CenResult, CenStatus};

// ============================================================================
// Number formatting / parsing helpers
// ============================================================================

/// Format an f64 in a way that matches common WKT output (no trailing
/// `.0` for integer-valued floats).
fn fmt_num(f: f64) -> String {
    if f == f.trunc() && f.abs() < 1e16 {
        format!("{}", f as i64)
    } else {
        format!("{}", f)
    }
}

// ============================================================================
// WKT
// ============================================================================

/// Serialize a geometry to WKT (Well-Known Text).
pub fn to_wkt(geom: &Geometry) -> String {
    let mut s = String::new();
    write_wkt(geom, &mut s);
    s
}

fn write_wkt(geom: &Geometry, out: &mut String) {
    match geom {
        Geometry::Point(p) => {
            out.push_str("POINT (");
            out.push_str(&fmt_num(p.x));
            out.push(' ');
            out.push_str(&fmt_num(p.y));
            out.push(')');
        }
        Geometry::LineString(ls) => {
            out.push_str("LINESTRING (");
            write_coord_list(&ls.points, out);
            out.push(')');
        }
        Geometry::Polygon(poly) => {
            out.push_str("POLYGON (");
            write_ring_list(poly.rings().collect::<Vec<_>>(), out);
            out.push(')');
        }
        Geometry::MultiPoint(mp) => {
            out.push_str("MULTIPOINT (");
            for (i, p) in mp.points.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push('(');
                out.push_str(&fmt_num(p.x));
                out.push(' ');
                out.push_str(&fmt_num(p.y));
                out.push(')');
            }
            out.push(')');
        }
        Geometry::MultiLineString(mls) => {
            out.push_str("MULTILINESTRING (");
            write_ring_list(mls.linestrings.iter().collect::<Vec<_>>(), out);
            out.push(')');
        }
        Geometry::MultiPolygon(mp) => {
            out.push_str("MULTIPOLYGON (");
            for (i, poly) in mp.polygons.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                // Each polygon is wrapped in its own '(' ... ')'.
                out.push('(');
                write_ring_list(poly.rings().collect::<Vec<_>>(), out);
                out.push(')');
            }
            out.push(')');
        }
        Geometry::GeometryCollection(gs) => {
            out.push_str("GEOMETRYCOLLECTION (");
            for (i, g) in gs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_wkt(g, out);
            }
            out.push(')');
        }
    }
}

fn write_coord_list(pts: &[Point], out: &mut String) {
    for (i, p) in pts.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&fmt_num(p.x));
        out.push(' ');
        out.push_str(&fmt_num(p.y));
    }
}

fn write_ring_list(rings: Vec<&LineString>, out: &mut String) {
    for (i, r) in rings.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('(');
        write_coord_list(&r.points, out);
        out.push(')');
    }
}

/// Parse a WKT string into a geometry.
pub fn from_wkt(input: &str) -> CenResult<Geometry> {
    let mut p = WktParser::new(input);
    p.skip_ws();
    let g = p.parse_geometry()?;
    p.skip_ws();
    if p.pos < p.input.len() {
        return Err(CenError::new(
            CenStatus::ErrSyntax,
            format!("trailing characters in WKT at offset {}", p.pos),
        ));
    }
    Ok(g)
}

struct WktParser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> WktParser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    fn eat(&mut self, c: char) -> CenResult<()> {
        self.skip_ws();
        if self.peek() == Some(c) {
            self.pos += c.len_utf8();
            Ok(())
        } else {
            Err(CenError::new(
                CenStatus::ErrSyntax,
                format!("expected '{}' at offset {}, got {:?}", c, self.pos, self.peek()),
            ))
        }
    }

    fn eat_ident(&mut self) -> CenResult<String> {
        self.skip_ws();
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_alphabetic() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(CenError::new(
                CenStatus::ErrSyntax,
                format!("expected identifier at offset {}", self.pos),
            ));
        }
        Ok(self.input[start..self.pos].to_string())
    }

    fn eat_number(&mut self) -> CenResult<f64> {
        self.skip_ws();
        let start = self.pos;
        let mut seen_dot = false;
        let mut seen_e = false;
        let mut seen_digit = false;
        if self.peek() == Some('-') || self.peek() == Some('+') {
            self.pos += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                seen_digit = true;
                self.pos += 1;
            } else if c == '.' && !seen_dot && !seen_e {
                seen_dot = true;
                self.pos += 1;
            } else if (c == 'e' || c == 'E') && !seen_e && seen_digit {
                seen_e = true;
                self.pos += 1;
                if self.peek() == Some('-') || self.peek() == Some('+') {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
        if !seen_digit {
            return Err(CenError::new(
                CenStatus::ErrSyntax,
                format!("expected number at offset {}", start),
            ));
        }
        let s = &self.input[start..self.pos];
        s.parse::<f64>()
            .map_err(|e| CenError::new(CenStatus::ErrSyntax, format!("bad number '{}': {}", s, e)))
    }

    fn eat_point(&mut self) -> CenResult<Point> {
        let x = self.eat_number()?;
        let y = self.eat_number()?;
        Ok(Point::new(x, y))
    }

    fn eat_coord_list(&mut self) -> CenResult<Vec<Point>> {
        // Assumes the leading '(' has NOT been consumed.
        self.eat('(')?;
        let mut pts = Vec::new();
        self.skip_ws();
        if self.peek() == Some(')') {
            self.pos += 1;
            return Ok(pts);
        }
        loop {
            pts.push(self.eat_point()?);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some(')') => {
                    self.pos += 1;
                    break;
                }
                _ => {
                    return Err(CenError::new(
                        CenStatus::ErrSyntax,
                        format!("expected ',' or ')' at offset {}", self.pos),
                    ));
                }
            }
        }
        Ok(pts)
    }

    /// Eat a single point as either `(x y)` or `x y`.
    fn eat_point_token(&mut self) -> CenResult<Point> {
        self.skip_ws();
        if self.peek() == Some('(') {
            self.eat('(')?;
            let p = self.eat_point()?;
            self.eat(')')?;
            Ok(p)
        } else {
            self.eat_point()
        }
    }

    fn eat_ring_list(&mut self) -> CenResult<Vec<LineString>> {
        self.eat('(')?;
        let mut rings = Vec::new();
        self.skip_ws();
        if self.peek() == Some(')') {
            self.pos += 1;
            return Ok(rings);
        }
        loop {
            let pts = self.eat_coord_list()?;
            rings.push(LineString::new(pts));
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some(')') => {
                    self.pos += 1;
                    break;
                }
                _ => {
                    return Err(CenError::new(
                        CenStatus::ErrSyntax,
                        format!("expected ',' or ')' at offset {}", self.pos),
                    ));
                }
            }
        }
        Ok(rings)
    }

    fn eat_polygon_list(&mut self) -> CenResult<Vec<Polygon>> {
        self.eat('(')?;
        let mut polys = Vec::new();
        self.skip_ws();
        if self.peek() == Some(')') {
            self.pos += 1;
            return Ok(polys);
        }
        loop {
            let rings = self.eat_ring_list()?;
            if rings.is_empty() {
                return Err(CenError::new(CenStatus::ErrSyntax, "polygon has no rings"));
            }
            polys.push(Polygon::new(rings[0].clone(), rings[1..].to_vec()));
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some(')') => {
                    self.pos += 1;
                    break;
                }
                _ => {
                    return Err(CenError::new(
                        CenStatus::ErrSyntax,
                        format!("expected ',' or ')' at offset {}", self.pos),
                    ));
                }
            }
        }
        Ok(polys)
    }

    fn parse_geometry(&mut self) -> CenResult<Geometry> {
        let ty = self.eat_ident()?.to_ascii_uppercase();
        // Optional 'Z', 'M', 'ZM' dimension markers — ignored.
        self.skip_ws();
        if self.peek().map(|c| c == 'Z' || c == 'M').unwrap_or(false) {
            let _ = self.eat_ident()?;
        }
        self.skip_ws();
        // Optional 'EMPTY'.
        if self.peek().map(|c| c == 'E').unwrap_or(false) {
            let save = self.pos;
            let ident = self.eat_ident().unwrap_or_default().to_ascii_uppercase();
            if ident == "EMPTY" {
                return Ok(empty_geometry(&ty));
            }
            self.pos = save;
        }

        match ty.as_str() {
            "POINT" => {
                self.eat('(')?;
                let p = self.eat_point()?;
                self.eat(')')?;
                Ok(Geometry::Point(p))
            }
            "LINESTRING" => {
                let pts = self.eat_coord_list()?;
                Ok(Geometry::LineString(LineString::new(pts)))
            }
            "POLYGON" => {
                let rings = self.eat_ring_list()?;
                if rings.is_empty() {
                    return Err(CenError::new(CenStatus::ErrSyntax, "polygon has no rings"));
                }
                Ok(Geometry::Polygon(Polygon::new(rings[0].clone(), rings[1..].to_vec())))
            }
            "MULTIPOINT" => {
                self.eat('(')?;
                let mut pts = Vec::new();
                self.skip_ws();
                if self.peek() == Some(')') {
                    self.pos += 1;
                    return Ok(Geometry::MultiPoint(MultiPoint::new(pts)));
                }
                loop {
                    pts.push(self.eat_point_token()?);
                    self.skip_ws();
                    match self.peek() {
                        Some(',') => {
                            self.pos += 1;
                        }
                        Some(')') => {
                            self.pos += 1;
                            break;
                        }
                        _ => {
                            return Err(CenError::new(
                                CenStatus::ErrSyntax,
                                format!("expected ',' or ')' at offset {}", self.pos),
                            ));
                        }
                    }
                }
                Ok(Geometry::MultiPoint(MultiPoint::new(pts)))
            }
            "MULTILINESTRING" => {
                let rings = self.eat_ring_list()?;
                Ok(Geometry::MultiLineString(MultiLineString::new(rings)))
            }
            "MULTIPOLYGON" => {
                let polys = self.eat_polygon_list()?;
                Ok(Geometry::MultiPolygon(MultiPolygon::new(polys)))
            }
            "GEOMETRYCOLLECTION" => {
                self.eat('(')?;
                let mut gs = Vec::new();
                self.skip_ws();
                if self.peek() == Some(')') {
                    self.pos += 1;
                    return Ok(Geometry::GeometryCollection(gs));
                }
                loop {
                    gs.push(self.parse_geometry()?);
                    self.skip_ws();
                    match self.peek() {
                        Some(',') => {
                            self.pos += 1;
                        }
                        Some(')') => {
                            self.pos += 1;
                            break;
                        }
                        _ => {
                            return Err(CenError::new(
                                CenStatus::ErrSyntax,
                                format!("expected ',' or ')' at offset {}", self.pos),
                            ));
                        }
                    }
                }
                Ok(Geometry::GeometryCollection(gs))
            }
            other => Err(CenError::new(
                CenStatus::ErrSyntax,
                format!("unknown WKT geometry type: {}", other),
            )),
        }
    }
}

fn empty_geometry(ty: &str) -> Geometry {
    match ty {
        "POINT" => Geometry::Point(Point::new(f64::NAN, f64::NAN)),
        "LINESTRING" => Geometry::LineString(LineString::new(Vec::new())),
        "POLYGON" => Geometry::Polygon(Polygon::new(LineString::new(Vec::new()), Vec::new())),
        "MULTIPOINT" => Geometry::MultiPoint(MultiPoint::new(Vec::new())),
        "MULTILINESTRING" => Geometry::MultiLineString(MultiLineString::new(Vec::new())),
        "MULTIPOLYGON" => Geometry::MultiPolygon(MultiPolygon::new(Vec::new())),
        _ => Geometry::GeometryCollection(Vec::new()),
    }
}

// ============================================================================
// WKB
// ============================================================================

// WKB geometry type codes.
const WKB_POINT: u32 = 1;
const WKB_LINESTRING: u32 = 2;
const WKB_POLYGON: u32 = 3;
const WKB_MULTIPOINT: u32 = 4;
const WKB_MULTILINESTRING: u32 = 5;
const WKB_MULTIPOLYGON: u32 = 6;
const WKB_GEOMETRYCOLLECTION: u32 = 7;

const BYTE_ORDER_LE: u8 = 1;

/// Serialize a geometry to WKB (Well-Known Binary), little-endian.
pub fn to_wkb(geom: &Geometry) -> Vec<u8> {
    let mut buf = Vec::new();
    write_wkb(geom, &mut buf);
    buf
}

fn write_wkb(geom: &Geometry, out: &mut Vec<u8>) {
    out.push(BYTE_ORDER_LE);
    let ty: u32 = match geom {
        Geometry::Point(_) => WKB_POINT,
        Geometry::LineString(_) => WKB_LINESTRING,
        Geometry::Polygon(_) => WKB_POLYGON,
        Geometry::MultiPoint(_) => WKB_MULTIPOINT,
        Geometry::MultiLineString(_) => WKB_MULTILINESTRING,
        Geometry::MultiPolygon(_) => WKB_MULTIPOLYGON,
        Geometry::GeometryCollection(_) => WKB_GEOMETRYCOLLECTION,
    };
    out.extend_from_slice(&ty.to_le_bytes());
    match geom {
        Geometry::Point(p) => write_point(p, out),
        Geometry::LineString(ls) => {
            out.extend_from_slice(&(ls.points.len() as u32).to_le_bytes());
            for p in &ls.points {
                write_point(p, out);
            }
        }
        Geometry::Polygon(poly) => {
            let rings: Vec<&LineString> = poly.rings().collect();
            out.extend_from_slice(&(rings.len() as u32).to_le_bytes());
            for r in rings {
                out.extend_from_slice(&(r.points.len() as u32).to_le_bytes());
                for p in &r.points {
                    write_point(p, out);
                }
            }
        }
        Geometry::MultiPoint(mp) => {
            out.extend_from_slice(&(mp.points.len() as u32).to_le_bytes());
            for p in &mp.points {
                write_wkb(&Geometry::Point(*p), out);
            }
        }
        Geometry::MultiLineString(mls) => {
            out.extend_from_slice(&(mls.linestrings.len() as u32).to_le_bytes());
            for ls in &mls.linestrings {
                write_wkb(&Geometry::LineString(ls.clone()), out);
            }
        }
        Geometry::MultiPolygon(mp) => {
            out.extend_from_slice(&(mp.polygons.len() as u32).to_le_bytes());
            for p in &mp.polygons {
                write_wkb(&Geometry::Polygon(p.clone()), out);
            }
        }
        Geometry::GeometryCollection(gs) => {
            out.extend_from_slice(&(gs.len() as u32).to_le_bytes());
            for g in gs {
                write_wkb(g, out);
            }
        }
    }
}

fn write_point(p: &Point, out: &mut Vec<u8>) {
    out.extend_from_slice(&p.x.to_le_bytes());
    out.extend_from_slice(&p.y.to_le_bytes());
}

/// Parse WKB bytes into a geometry.
pub fn from_wkb(bytes: &[u8]) -> CenResult<Geometry> {
    let mut r = WkbReader { bytes, pos: 0 };
    r.read_geometry()
}

struct WkbReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> WkbReader<'a> {
    fn read_u8(&mut self) -> CenResult<u8> {
        if self.pos >= self.bytes.len() {
            return Err(CenError::new(CenStatus::ErrSyntax, "WKB EOF reading u8"));
        }
        let v = self.bytes[self.pos];
        self.pos += 1;
        Ok(v)
    }

    fn read_u32_le(&mut self) -> CenResult<u32> {
        if self.pos + 4 > self.bytes.len() {
            return Err(CenError::new(CenStatus::ErrSyntax, "WKB EOF reading u32"));
        }
        let mut b = [0u8; 4];
        b.copy_from_slice(&self.bytes[self.pos..self.pos + 4]);
        self.pos += 4;
        Ok(u32::from_le_bytes(b))
    }

    fn read_f64_le(&mut self) -> CenResult<f64> {
        if self.pos + 8 > self.bytes.len() {
            return Err(CenError::new(CenStatus::ErrSyntax, "WKB EOF reading f64"));
        }
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.bytes[self.pos..self.pos + 8]);
        self.pos += 8;
        Ok(f64::from_le_bytes(b))
    }

    fn read_point(&mut self) -> CenResult<Point> {
        let x = self.read_f64_le()?;
        let y = self.read_f64_le()?;
        Ok(Point::new(x, y))
    }

    fn read_geometry(&mut self) -> CenResult<Geometry> {
        let byte_order = self.read_u8()?;
        if byte_order != BYTE_ORDER_LE {
            return Err(CenError::new(
                CenStatus::ErrSyntax,
                format!("unsupported WKB byte order: {} (only little-endian=1 supported)", byte_order),
            ));
        }
        let ty = self.read_u32_le()?;
        match ty {
            WKB_POINT => Ok(Geometry::Point(self.read_point()?)),
            WKB_LINESTRING => {
                let n = self.read_u32_le()? as usize;
                let mut pts = Vec::with_capacity(n);
                for _ in 0..n {
                    pts.push(self.read_point()?);
                }
                Ok(Geometry::LineString(LineString::new(pts)))
            }
            WKB_POLYGON => {
                let n_rings = self.read_u32_le()? as usize;
                let mut rings = Vec::with_capacity(n_rings);
                for _ in 0..n_rings {
                    let n_pts = self.read_u32_le()? as usize;
                    let mut pts = Vec::with_capacity(n_pts);
                    for _ in 0..n_pts {
                        pts.push(self.read_point()?);
                    }
                    rings.push(LineString::new(pts));
                }
                if rings.is_empty() {
                    return Err(CenError::new(CenStatus::ErrSyntax, "WKB polygon has no rings"));
                }
                Ok(Geometry::Polygon(Polygon::new(rings[0].clone(), rings[1..].to_vec())))
            }
            WKB_MULTIPOINT => {
                let n = self.read_u32_le()? as usize;
                let mut pts = Vec::with_capacity(n);
                for _ in 0..n {
                    let g = self.read_geometry()?;
                    if let Geometry::Point(p) = g {
                        pts.push(p);
                    } else {
                        return Err(CenError::new(CenStatus::ErrSyntax, "MULTIPOINT child not a point"));
                    }
                }
                Ok(Geometry::MultiPoint(MultiPoint::new(pts)))
            }
            WKB_MULTILINESTRING => {
                let n = self.read_u32_le()? as usize;
                let mut lss = Vec::with_capacity(n);
                for _ in 0..n {
                    let g = self.read_geometry()?;
                    if let Geometry::LineString(ls) = g {
                        lss.push(ls);
                    } else {
                        return Err(CenError::new(CenStatus::ErrSyntax, "MULTILINESTRING child not a linestring"));
                    }
                }
                Ok(Geometry::MultiLineString(MultiLineString::new(lss)))
            }
            WKB_MULTIPOLYGON => {
                let n = self.read_u32_le()? as usize;
                let mut polys = Vec::with_capacity(n);
                for _ in 0..n {
                    let g = self.read_geometry()?;
                    if let Geometry::Polygon(p) = g {
                        polys.push(p);
                    } else {
                        return Err(CenError::new(CenStatus::ErrSyntax, "MULTIPOLYGON child not a polygon"));
                    }
                }
                Ok(Geometry::MultiPolygon(MultiPolygon::new(polys)))
            }
            WKB_GEOMETRYCOLLECTION => {
                let n = self.read_u32_le()? as usize;
                let mut gs = Vec::with_capacity(n);
                for _ in 0..n {
                    gs.push(self.read_geometry()?);
                }
                Ok(Geometry::GeometryCollection(gs))
            }
            other => Err(CenError::new(
                CenStatus::ErrSyntax,
                format!("unknown WKB geometry type code: {}", other),
            )),
        }
    }
}

// ============================================================================
// GeoJSON
// ============================================================================

/// Serialize a geometry to a GeoJSON geometry JSON string.
pub fn to_geojson(geom: &Geometry) -> String {
    let mut s = String::new();
    write_geojson(geom, &mut s);
    s
}

fn write_geojson(geom: &Geometry, out: &mut String) {
    match geom {
        Geometry::Point(p) => {
            out.push_str("{\"type\":\"Point\",\"coordinates\":[");
            out.push_str(&fmt_num(p.x));
            out.push(',');
            out.push_str(&fmt_num(p.y));
            out.push_str("]}");
        }
        Geometry::LineString(ls) => {
            out.push_str("{\"type\":\"LineString\",\"coordinates\":[");
            write_json_pts(&ls.points, out);
            out.push_str("]}");
        }
        Geometry::Polygon(poly) => {
            out.push_str("{\"type\":\"Polygon\",\"coordinates\":[");
            let rings: Vec<&LineString> = poly.rings().collect();
            for (i, r) in rings.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('[');
                write_json_pts(&r.points, out);
                out.push(']');
            }
            out.push_str("]}");
        }
        Geometry::MultiPoint(mp) => {
            out.push_str("{\"type\":\"MultiPoint\",\"coordinates\":[");
            write_json_pts(&mp.points, out);
            out.push_str("]}");
        }
        Geometry::MultiLineString(mls) => {
            out.push_str("{\"type\":\"MultiLineString\",\"coordinates\":[");
            for (i, ls) in mls.linestrings.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('[');
                write_json_pts(&ls.points, out);
                out.push(']');
            }
            out.push_str("]}");
        }
        Geometry::MultiPolygon(mp) => {
            out.push_str("{\"type\":\"MultiPolygon\",\"coordinates\":[");
            for (i, poly) in mp.polygons.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('[');
                let rings: Vec<&LineString> = poly.rings().collect();
                for (j, r) in rings.iter().enumerate() {
                    if j > 0 {
                        out.push(',');
                    }
                    out.push('[');
                    write_json_pts(&r.points, out);
                    out.push(']');
                }
                out.push(']');
            }
            out.push_str("]}");
        }
        Geometry::GeometryCollection(gs) => {
            out.push_str("{\"type\":\"GeometryCollection\",\"geometries\":[");
            for (i, g) in gs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_geojson(g, out);
            }
            out.push_str("]}");
        }
    }
}

fn write_json_pts(pts: &[Point], out: &mut String) {
    for (i, p) in pts.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        out.push_str(&fmt_num(p.x));
        out.push(',');
        out.push_str(&fmt_num(p.y));
        out.push(']');
    }
}

/// Parse a GeoJSON geometry JSON string into a geometry.
pub fn from_geojson(input: &str) -> CenResult<Geometry> {
    let mut p = JsonParser::new(input);
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos < p.input.len() {
        return Err(CenError::new(
            CenStatus::ErrSyntax,
            format!("trailing characters in GeoJSON at offset {}", p.pos),
        ));
    }
    json_to_geometry(&v)
}

// A minimal JSON value type.
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    fn get(&self, key: &str) -> Option<&JsonValue> {
        if let JsonValue::Object(entries) = self {
            entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
        } else {
            None
        }
    }
}

struct JsonParser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self) -> CenResult<JsonValue> {
        self.skip_ws();
        match self.peek() {
            Some('{') => self.parse_object(),
            Some('[') => self.parse_array(),
            Some('"') => Ok(JsonValue::String(self.parse_string()?)),
            Some('t') | Some('f') => self.parse_bool(),
            Some('n') => self.parse_null(),
            Some(c) if c == '-' || c == '+' || c.is_ascii_digit() => {
                Ok(JsonValue::Number(self.parse_number()?))
            }
            other => Err(CenError::new(
                CenStatus::ErrSyntax,
                format!("unexpected character {:?} at offset {}", other, self.pos),
            )),
        }
    }

    fn parse_object(&mut self) -> CenResult<JsonValue> {
        self.pos += 1; // {
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(JsonValue::Object(entries));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(':') {
                return Err(CenError::new(
                    CenStatus::ErrSyntax,
                    format!("expected ':' at offset {}", self.pos),
                ));
            }
            self.pos += 1;
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                _ => {
                    return Err(CenError::new(
                        CenStatus::ErrSyntax,
                        format!("expected ',' or '}}' at offset {}", self.pos),
                    ));
                }
            }
        }
        Ok(JsonValue::Object(entries))
    }

    fn parse_array(&mut self) -> CenResult<JsonValue> {
        self.pos += 1; // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.pos += 1;
            return Ok(JsonValue::Array(items));
        }
        loop {
            let v = self.parse_value()?;
            items.push(v);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some(']') => {
                    self.pos += 1;
                    break;
                }
                _ => {
                    return Err(CenError::new(
                        CenStatus::ErrSyntax,
                        format!("expected ',' or ']' at offset {}", self.pos),
                    ));
                }
            }
        }
        Ok(JsonValue::Array(items))
    }

    fn parse_string(&mut self) -> CenResult<String> {
        self.skip_ws();
        if self.peek() != Some('"') {
            return Err(CenError::new(
                CenStatus::ErrSyntax,
                format!("expected '\"' at offset {}", self.pos),
            ));
        }
        self.pos += 1;
        let mut s = String::new();
        while let Some(c) = self.peek() {
            self.pos += c.len_utf8();
            match c {
                '"' => return Ok(s),
                '\\' => {
                    let esc = self.peek().ok_or_else(|| {
                        CenError::new(CenStatus::ErrSyntax, "trailing backslash in string")
                    })?;
                    self.pos += esc.len_utf8();
                    match esc {
                        '"' => s.push('"'),
                        '\\' => s.push('\\'),
                        '/' => s.push('/'),
                        'b' => s.push('\u{0008}'),
                        'f' => s.push('\u{000C}'),
                        'n' => s.push('\n'),
                        'r' => s.push('\r'),
                        't' => s.push('\t'),
                        'u' => {
                            if self.pos + 4 > self.input.len() {
                                return Err(CenError::new(
                                    CenStatus::ErrSyntax,
                                    "bad \\u escape",
                                ));
                            }
                            let hex = &self.input[self.pos..self.pos + 4];
                            self.pos += 4;
                            let code = u32::from_str_radix(hex, 16).map_err(|e| {
                                CenError::new(
                                    CenStatus::ErrSyntax,
                                    format!("bad \\u escape '{}': {}", hex, e),
                                )
                            })?;
                            if let Some(ch) = char::from_u32(code) {
                                s.push(ch);
                            }
                        }
                        other => {
                            return Err(CenError::new(
                                CenStatus::ErrSyntax,
                                format!("bad escape \\{}", other),
                            ));
                        }
                    }
                }
                c => s.push(c),
            }
        }
        Err(CenError::new(CenStatus::ErrSyntax, "unterminated string"))
    }

    fn parse_number(&mut self) -> CenResult<f64> {
        let start = self.pos;
        if self.peek() == Some('-') || self.peek() == Some('+') {
            self.pos += 1;
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.peek() == Some('.') {
            self.pos += 1;
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        if self.peek() == Some('e') || self.peek() == Some('E') {
            self.pos += 1;
            if self.peek() == Some('-') || self.peek() == Some('+') {
                self.pos += 1;
            }
            while let Some(c) = self.peek() {
                if c.is_ascii_digit() {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        let s = &self.input[start..self.pos];
        s.parse::<f64>()
            .map_err(|e| CenError::new(CenStatus::ErrSyntax, format!("bad number '{}': {}", s, e)))
    }

    fn parse_bool(&mut self) -> CenResult<JsonValue> {
        if self.input[self.pos..].starts_with("true") {
            self.pos += 4;
            Ok(JsonValue::Bool(true))
        } else if self.input[self.pos..].starts_with("false") {
            self.pos += 5;
            Ok(JsonValue::Bool(false))
        } else {
            Err(CenError::new(
                CenStatus::ErrSyntax,
                format!("bad literal at offset {}", self.pos),
            ))
        }
    }

    fn parse_null(&mut self) -> CenResult<JsonValue> {
        if self.input[self.pos..].starts_with("null") {
            self.pos += 4;
            Ok(JsonValue::Null)
        } else {
            Err(CenError::new(
                CenStatus::ErrSyntax,
                format!("bad literal at offset {}", self.pos),
            ))
        }
    }
}

fn json_to_geometry(v: &JsonValue) -> CenResult<Geometry> {
    let ty = v
        .get("type")
        .and_then(|t| if let JsonValue::String(s) = t { Some(s.clone()) } else { None })
        .ok_or_else(|| CenError::new(CenStatus::ErrSyntax, "GeoJSON missing 'type' field"))?;

    match ty.as_str() {
        "Point" => {
            let coords = json_as_coord_array(v.get("coordinates").ok_or_else(|| {
                CenError::new(CenStatus::ErrSyntax, "Point missing coordinates")
            })?)?;
            Ok(Geometry::Point(Point::new(coords[0], coords[1])))
        }
        "LineString" => {
            let pts = json_as_coord_list(v.get("coordinates").ok_or_else(|| {
                CenError::new(CenStatus::ErrSyntax, "LineString missing coordinates")
            })?)?;
            Ok(Geometry::LineString(LineString::new(pts)))
        }
        "Polygon" => {
            let rings = json_as_ring_list(v.get("coordinates").ok_or_else(|| {
                CenError::new(CenStatus::ErrSyntax, "Polygon missing coordinates")
            })?)?;
            if rings.is_empty() {
                return Err(CenError::new(CenStatus::ErrSyntax, "Polygon has no rings"));
            }
            Ok(Geometry::Polygon(Polygon::new(rings[0].clone(), rings[1..].to_vec())))
        }
        "MultiPoint" => {
            let pts = json_as_coord_list(v.get("coordinates").ok_or_else(|| {
                CenError::new(CenStatus::ErrSyntax, "MultiPoint missing coordinates")
            })?)?;
            Ok(Geometry::MultiPoint(MultiPoint::new(pts)))
        }
        "MultiLineString" => {
            let rings = json_as_ring_list(v.get("coordinates").ok_or_else(|| {
                CenError::new(CenStatus::ErrSyntax, "MultiLineString missing coordinates")
            })?)?;
            Ok(Geometry::MultiLineString(MultiLineString::new(rings)))
        }
        "MultiPolygon" => {
            let arr = match v.get("coordinates") {
                Some(JsonValue::Array(a)) => a,
                _ => return Err(CenError::new(CenStatus::ErrSyntax, "MultiPolygon missing coordinates")),
            };
            let mut polys = Vec::with_capacity(arr.len());
            for item in arr {
                let rings = json_as_ring_list(item)?;
                if rings.is_empty() {
                    return Err(CenError::new(CenStatus::ErrSyntax, "MultiPolygon child has no rings"));
                }
                polys.push(Polygon::new(rings[0].clone(), rings[1..].to_vec()));
            }
            Ok(Geometry::MultiPolygon(MultiPolygon::new(polys)))
        }
        "GeometryCollection" => {
            let arr = match v.get("geometries") {
                Some(JsonValue::Array(a)) => a,
                _ => {
                    return Err(CenError::new(
                        CenStatus::ErrSyntax,
                        "GeometryCollection missing 'geometries' field",
                    ));
                }
            };
            let mut gs = Vec::with_capacity(arr.len());
            for item in arr {
                gs.push(json_to_geometry(item)?);
            }
            Ok(Geometry::GeometryCollection(gs))
        }
        other => Err(CenError::new(
            CenStatus::ErrSyntax,
            format!("unknown GeoJSON geometry type: {}", other),
        )),
    }
}

fn json_as_coord_array(v: &JsonValue) -> CenResult<[f64; 2]> {
    let arr = match v {
        JsonValue::Array(a) => a,
        _ => return Err(CenError::new(CenStatus::ErrSyntax, "expected coordinate array")),
    };
    if arr.len() < 2 {
        return Err(CenError::new(
            CenStatus::ErrSyntax,
            "coordinate array must have at least 2 elements",
        ));
    }
    let x = match &arr[0] {
        JsonValue::Number(n) => *n,
        _ => return Err(CenError::new(CenStatus::ErrSyntax, "x coordinate not a number")),
    };
    let y = match &arr[1] {
        JsonValue::Number(n) => *n,
        _ => return Err(CenError::new(CenStatus::ErrSyntax, "y coordinate not a number")),
    };
    Ok([x, y])
}

fn json_as_coord_list(v: &JsonValue) -> CenResult<Vec<Point>> {
    let arr = match v {
        JsonValue::Array(a) => a,
        _ => return Err(CenError::new(CenStatus::ErrSyntax, "expected coordinates array")),
    };
    let mut pts = Vec::with_capacity(arr.len());
    for item in arr {
        let c = json_as_coord_array(item)?;
        pts.push(Point::new(c[0], c[1]));
    }
    Ok(pts)
}

fn json_as_ring_list(v: &JsonValue) -> CenResult<Vec<LineString>> {
    let arr = match v {
        JsonValue::Array(a) => a,
        _ => return Err(CenError::new(CenStatus::ErrSyntax, "expected rings array")),
    };
    let mut rings = Vec::with_capacity(arr.len());
    for item in arr {
        rings.push(LineString::new(json_as_coord_list(item)?));
    }
    Ok(rings)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq_geom(a: &Geometry, b: &Geometry) -> bool {
        // Structural equality on floats with small tolerance.
        match (a, b) {
            (Geometry::Point(p), Geometry::Point(q)) => {
                (p.x - q.x).abs() < 1e-9 && (p.y - q.y).abs() < 1e-9
            }
            (Geometry::LineString(a), Geometry::LineString(b)) => {
                a.points.len() == b.points.len()
                    && a.points.iter().zip(b.points.iter()).all(|(p, q)| {
                        (p.x - q.x).abs() < 1e-9 && (p.y - q.y).abs() < 1e-9
                    })
            }
            (Geometry::Polygon(a), Geometry::Polygon(b)) => {
                let rings_eq = |ra: &LineString, rb: &LineString| -> bool {
                    ra.points.len() == rb.points.len()
                        && ra.points.iter().zip(rb.points.iter()).all(|(p, q)| {
                            (p.x - q.x).abs() < 1e-9 && (p.y - q.y).abs() < 1e-9
                        })
                };
                rings_eq(&a.exterior, &b.exterior)
                    && a.interiors.len() == b.interiors.len()
                    && a
                        .interiors
                        .iter()
                        .zip(b.interiors.iter())
                        .all(|(ra, rb)| rings_eq(ra, rb))
            }
            _ => a == b,
        }
    }

    // ----------------- WKT round-trips -----------------

    #[test]
    fn wkt_point_roundtrip() {
        let g = Geometry::point(1.0, 2.0);
        let s = to_wkt(&g);
        assert_eq!(s, "POINT (1 2)");
        let g2 = from_wkt(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn wkt_linestring_roundtrip() {
        let g = Geometry::line_string(&[(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)]);
        let s = to_wkt(&g);
        let g2 = from_wkt(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn wkt_polygon_roundtrip() {
        let g = Geometry::polygon(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]);
        let s = to_wkt(&g);
        let g2 = from_wkt(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn wkt_multipoint_roundtrip() {
        let g = Geometry::MultiPoint(MultiPoint::new(vec![Point::new(1.0, 2.0), Point::new(3.0, 4.0)]));
        let s = to_wkt(&g);
        let g2 = from_wkt(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn wkt_multilinestring_roundtrip() {
        let g = Geometry::MultiLineString(MultiLineString::new(vec![
            LineString::from_xy(&[(0.0, 0.0), (1.0, 1.0)]),
            LineString::from_xy(&[(2.0, 2.0), (3.0, 3.0)]),
        ]));
        let s = to_wkt(&g);
        let g2 = from_wkt(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn wkt_multipolygon_roundtrip() {
        let g = Geometry::MultiPolygon(MultiPolygon::new(vec![
            Polygon::from_exterior(LineString::from_xy(&[
                (0.0, 0.0),
                (1.0, 0.0),
                (1.0, 1.0),
                (0.0, 0.0),
            ])),
            Polygon::from_exterior(LineString::from_xy(&[
                (2.0, 2.0),
                (3.0, 2.0),
                (3.0, 3.0),
                (2.0, 2.0),
            ])),
        ]));
        let s = to_wkt(&g);
        let g2 = from_wkt(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn wkt_geometrycollection_roundtrip() {
        let g = Geometry::GeometryCollection(vec![
            Geometry::point(1.0, 2.0),
            Geometry::line_string(&[(0.0, 0.0), (1.0, 1.0)]),
        ]);
        let s = to_wkt(&g);
        let g2 = from_wkt(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn wkt_negative_coordinates() {
        let g = Geometry::point(-1.5, 2.7);
        let s = to_wkt(&g);
        assert_eq!(s, "POINT (-1.5 2.7)");
        let g2 = from_wkt(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    // ----------------- WKB round-trips -----------------

    #[test]
    fn wkb_point_roundtrip() {
        let g = Geometry::point(1.5, -2.5);
        let bytes = to_wkb(&g);
        assert_eq!(bytes[0], BYTE_ORDER_LE);
        let g2 = from_wkb(&bytes).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn wkb_linestring_roundtrip() {
        let g = Geometry::line_string(&[(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)]);
        let bytes = to_wkb(&g);
        let g2 = from_wkb(&bytes).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn wkb_polygon_roundtrip() {
        let g = Geometry::polygon(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]);
        let bytes = to_wkb(&g);
        let g2 = from_wkb(&bytes).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn wkb_multipoint_roundtrip() {
        let g = Geometry::MultiPoint(MultiPoint::new(vec![Point::new(1.0, 2.0), Point::new(3.0, 4.0)]));
        let bytes = to_wkb(&g);
        let g2 = from_wkb(&bytes).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn wkb_multilinestring_roundtrip() {
        let g = Geometry::MultiLineString(MultiLineString::new(vec![
            LineString::from_xy(&[(0.0, 0.0), (1.0, 1.0)]),
            LineString::from_xy(&[(2.0, 2.0), (3.0, 3.0)]),
        ]));
        let bytes = to_wkb(&g);
        let g2 = from_wkb(&bytes).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn wkb_multipolygon_roundtrip() {
        let g = Geometry::MultiPolygon(MultiPolygon::new(vec![
            Polygon::from_exterior(LineString::from_xy(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)])),
            Polygon::from_exterior(LineString::from_xy(&[(2.0, 2.0), (3.0, 2.0), (3.0, 3.0), (2.0, 2.0)])),
        ]));
        let bytes = to_wkb(&g);
        let g2 = from_wkb(&bytes).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    // ----------------- GeoJSON round-trips -----------------

    #[test]
    fn geojson_point_roundtrip() {
        let g = Geometry::point(1.0, 2.0);
        let s = to_geojson(&g);
        assert!(s.contains("\"type\":\"Point\""));
        let g2 = from_geojson(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn geojson_linestring_roundtrip() {
        let g = Geometry::line_string(&[(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)]);
        let s = to_geojson(&g);
        let g2 = from_geojson(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn geojson_polygon_roundtrip() {
        let g = Geometry::polygon(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]);
        let s = to_geojson(&g);
        let g2 = from_geojson(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn geojson_multipoint_roundtrip() {
        let g = Geometry::MultiPoint(MultiPoint::new(vec![Point::new(1.0, 2.0), Point::new(3.0, 4.0)]));
        let s = to_geojson(&g);
        let g2 = from_geojson(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn geojson_multilinestring_roundtrip() {
        let g = Geometry::MultiLineString(MultiLineString::new(vec![
            LineString::from_xy(&[(0.0, 0.0), (1.0, 1.0)]),
            LineString::from_xy(&[(2.0, 2.0), (3.0, 3.0)]),
        ]));
        let s = to_geojson(&g);
        let g2 = from_geojson(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn geojson_multipolygon_roundtrip() {
        let g = Geometry::MultiPolygon(MultiPolygon::new(vec![
            Polygon::from_exterior(LineString::from_xy(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)])),
            Polygon::from_exterior(LineString::from_xy(&[(2.0, 2.0), (3.0, 2.0), (3.0, 3.0), (2.0, 2.0)])),
        ]));
        let s = to_geojson(&g);
        let g2 = from_geojson(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn geojson_geometrycollection_roundtrip() {
        let g = Geometry::GeometryCollection(vec![
            Geometry::point(1.0, 2.0),
            Geometry::line_string(&[(0.0, 0.0), (1.0, 1.0)]),
        ]);
        let s = to_geojson(&g);
        let g2 = from_geojson(&s).unwrap();
        assert!(approx_eq_geom(&g, &g2));
    }

    #[test]
    fn geojson_with_whitespace_parses() {
        let s = r#"{
            "type": "Point",
            "coordinates": [1.5, -2.5]
        }"#;
        let g = from_geojson(s).unwrap();
        match g {
            Geometry::Point(p) => {
                assert!((p.x - 1.5).abs() < 1e-9);
                assert!((p.y - (-2.5)).abs() < 1e-9);
            }
            _ => panic!("expected point"),
        }
    }

    #[test]
    fn wkt_bad_input_returns_error() {
        assert!(from_wkt("NOTAGEOMETRY").is_err());
        assert!(from_wkt("POINT (").is_err());
    }

    #[test]
    fn wkb_bad_input_returns_error() {
        assert!(from_wkb(&[]).is_err());
        assert!(from_wkb(&[1, 99, 0, 0, 0]).is_err()); // unknown type
    }

    #[test]
    fn geojson_bad_input_returns_error() {
        assert!(from_geojson("not json").is_err());
        assert!(from_geojson("{\"type\":\"Unknown\"}").is_err());
    }
}
