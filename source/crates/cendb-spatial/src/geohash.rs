//! Geohash encoding: linearize (lat, lon) into a string for spatial
//! locality-preserving indexing via ART.

/// Geohash precision (number of characters).
#[derive(Copy, Clone, Debug)]
pub struct GeohashPrecision(pub usize);

impl Default for GeohashPrecision {
    fn default() -> Self {
        Self(12) // ~3.7cm × 1.9cm precision
    }
}

const BASE32: &[u8] = b"0123456789bcdefghjkmnpqrstuvwxyz";

/// Encode a (lat, lon) pair into a geohash string.
pub fn encode_geohash(lat: f64, lon: f64, precision: GeohashPrecision) -> String {
    let mut hash = String::with_capacity(precision.0);
    let mut lat_range = (-90.0f64, 90.0f64);
    let mut lon_range = (-180.0f64, 180.0f64);
    let mut bit = 0;
    let mut idx = 0u32;
    let mut even = true;

    while hash.len() < precision.0 {
        let mid;
        if even {
            mid = (lon_range.0 + lon_range.1) / 2.0;
            if lon >= mid {
                idx |= 1 << (4 - bit);
                lon_range.0 = mid;
            } else {
                lon_range.1 = mid;
            }
        } else {
            mid = (lat_range.0 + lat_range.1) / 2.0;
            if lat >= mid {
                idx |= 1 << (4 - bit);
                lat_range.0 = mid;
            } else {
                lat_range.1 = mid;
            }
        }
        even = !even;
        bit += 1;
        if bit == 5 {
            hash.push(BASE32[idx as usize] as char);
            bit = 0;
            idx = 0;
        }
    }
    hash
}

/// Decode a geohash string back to (lat, lon) — returns the center of
/// the geohash cell.
pub fn decode_geohash(hash: &str) -> (f64, f64) {
    let mut lat_range = (-90.0f64, 90.0f64);
    let mut lon_range = (-180.0f64, 180.0f64);
    let mut even = true;

    for c in hash.chars() {
        let idx = BASE32.iter().position(|&b| b as char == c).unwrap_or(0) as u32;
        for bit in 0..5 {
            let val = (idx >> (4 - bit)) & 1;
            if even {
                let mid = (lon_range.0 + lon_range.1) / 2.0;
                if val == 1 {
                    lon_range.0 = mid;
                } else {
                    lon_range.1 = mid;
                }
            } else {
                let mid = (lat_range.0 + lat_range.1) / 2.0;
                if val == 1 {
                    lat_range.0 = mid;
                } else {
                    lat_range.1 = mid;
                }
            }
            even = !even;
        }
    }

    let lat = (lat_range.0 + lat_range.1) / 2.0;
    let lon = (lon_range.0 + lon_range.1) / 2.0;
    (lat, lon)
}

/// Get the bounding box of a geohash cell.
pub fn geohash_bounds(hash: &str) -> (f64, f64, f64, f64) {
    let mut lat_range = (-90.0f64, 90.0f64);
    let mut lon_range = (-180.0f64, 180.0f64);
    let mut even = true;

    for c in hash.chars() {
        let idx = BASE32.iter().position(|&b| b as char == c).unwrap_or(0) as u32;
        for bit in 0..5 {
            let val = (idx >> (4 - bit)) & 1;
            if even {
                let mid = (lon_range.0 + lon_range.1) / 2.0;
                if val == 1 {
                    lon_range.0 = mid;
                } else {
                    lon_range.1 = mid;
                }
            } else {
                let mid = (lat_range.0 + lat_range.1) / 2.0;
                if val == 1 {
                    lat_range.0 = mid;
                } else {
                    lat_range.1 = mid;
                }
            }
            even = !even;
        }
    }

    (lat_range.0, lat_range.1, lon_range.0, lon_range.1) // min_lat, max_lat, min_lon, max_lon
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let lat = 52.5200; // Berlin
        let lon = 13.4050;
        let hash = encode_geohash(lat, lon, GeohashPrecision(12));
        let (dec_lat, dec_lon) = decode_geohash(&hash);
        assert!((dec_lat - lat).abs() < 0.001);
        assert!((dec_lon - lon).abs() < 0.001);
    }

    #[test]
    fn nearby_points_have_common_prefix() {
        let h1 = encode_geohash(52.5200, 13.4050, GeohashPrecision(8));
        let h2 = encode_geohash(52.5201, 13.4051, GeohashPrecision(8));
        // Nearby points should share a long common prefix.
        let common = h1.chars().zip(h2.chars()).take_while(|(a, b)| a == b).count();
        assert!(common >= 6, "expected common prefix >= 6, got {} ({} vs {})", common, h1, h2);
    }

    #[test]
    fn distant_points_have_short_common_prefix() {
        let berlin = encode_geohash(52.52, 13.40, GeohashPrecision(8));
        let tokyo = encode_geohash(35.68, 139.76, GeohashPrecision(8));
        let common = berlin.chars().zip(tokyo.chars()).take_while(|(a, b)| a == b).count();
        assert!(common < 3, "distant points should have short common prefix, got {}", common);
    }

    #[test]
    fn geohash_bounds_correct() {
        // Use a point slightly offset from (0,0) to get a well-defined cell.
        let hash = encode_geohash(0.001, 0.001, GeohashPrecision(2));
        let (min_lat, max_lat, min_lon, max_lon) = geohash_bounds(&hash);
        // The cell should contain the point (0.001, 0.001).
        assert!(min_lat <= 0.001 && max_lat >= 0.001, "lat range {}..{} should contain 0.001", min_lat, max_lat);
        assert!(min_lon <= 0.001 && max_lon >= 0.001, "lon range {}..{} should contain 0.001", min_lon, max_lon);
    }
}
