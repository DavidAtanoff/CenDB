//! Fuzzing utilities for PAX decoder and CenQL parser.
//!
//! These functions generate random byte arrays and strings, feed them
//! to the decoders, and verify that no panics occur — only clean
//! `HexError` returns.

use cendb_storage::pax::PaxBlockReader;
use cendb_storage::encoding::{
    decode_minipage, Encoding, BitPackedCodec, DeltaOfDeltaCodec,
    EncodingCodec, FrameOfReferenceCodec, RawCodec, RunLengthCodec, gorilla_decode,
};
use cendb_cenql::parse as parse_cenql;
use cendb_core::HexResult;

/// Fuzz the PAX block reader with random bytes.
/// Verifies that no panic occurs — only clean error returns.
pub fn fuzz_pax_block(bytes: &[u8]) -> HexResult<()> {
    if bytes.len() < 64 {
        return Ok(()); // Too short for a header.
    }
    let block_size = bytes.len() as u32;
    let reader = PaxBlockReader::new(bytes, block_size);

    // Try to read the header.
    let _ = reader.header();

    // Try to read directories and minipages.
    for i in 0..10 {
        let _ = reader.directory(i);
        let _ = reader.minipage_bytes(i);
        let _ = reader.decode_i64_column(i);
        let _ = reader.var_value(i, 0);
        let _ = reader.var_value(i, 1);
    }

    // Try to materialize rows.
    for row in 0..5 {
        let _ = reader.decode_i64_column(row % 3);
    }

    Ok(())
}

/// Aggressive PAX fuzzer: generates random byte arrays and feeds them
/// to the PAX reader. Returns the number of inputs tested.
pub fn fuzz_pax_block_aggressive(iterations: u32, seed: u64) -> u32 {
    let mut rng = crate::crash_simulator::Rng::new(seed);
    let mut tested = 0u32;

    for _ in 0..iterations {
        // Generate random bytes of various sizes.
        let size = rng.gen_range(64, 65536) as usize;
        let bytes: Vec<u8> = (0..size)
            .map(|_| (rng.next_u64() & 0xFF) as u8)
            .collect();

        // The fuzz function should never panic.
        let _ = fuzz_pax_block(&bytes);
        tested += 1;
    }

    tested
}

/// Fuzz the encoding decoders with random bytes.
pub fn fuzz_encoding_decoders(iterations: u32, seed: u64) -> u32 {
    let mut rng = crate::crash_simulator::Rng::new(seed);
    let mut tested = 0u32;

    let codecs: Vec<Box<dyn EncodingCodec>> = vec![
        Box::new(RawCodec),
        Box::new(BitPackedCodec),
        Box::new(FrameOfReferenceCodec),
        Box::new(DeltaOfDeltaCodec),
        Box::new(RunLengthCodec),
    ];

    for _ in 0..iterations {
        // Generate random bytes.
        let size = rng.gen_range(0, 1024) as usize;
        let bytes: Vec<u8> = (0..size)
            .map(|_| (rng.next_u64() & 0xFF) as u8)
            .collect();

        // Try to decode with each codec.
        for codec in &codecs {
            // The decode should never panic — only return Err.
            let _ = codec.decode(&bytes, rng.gen_range(0, 100) as usize);
        }

        // Try Gorilla decode.
        let _ = gorilla_decode(&bytes, rng.gen_range(0, 100) as usize);

        // Try decode_minipage with each encoding.
        let encs = [
            Encoding::Raw,
            Encoding::BitPacked { bits: 8 },
            Encoding::FrameOfReference { base: 0, bits: 8 },
            Encoding::DeltaOfDelta,
            Encoding::RunLength,
        ];
        for enc in &encs {
            let _ = decode_minipage(*enc, &bytes, rng.gen_range(0, 100) as usize);
        }

        tested += 1;
    }

    tested
}

/// Fuzz the CenQL parser with random strings.
/// Verifies that no panic or infinite loop occurs.
pub fn fuzz_cenql_parser(input: &str) {
    // The parser should return Ok or Err — never panic.
    let _ = parse_cenql(input);
}

/// Aggressive CenQL fuzzer: generates random strings from CenQL tokens
/// and arbitrary characters.
pub fn fuzz_cenql_parser_aggressive(iterations: u32, seed: u64) -> u32 {
    let mut rng = crate::crash_simulator::Rng::new(seed);
    let mut tested = 0u32;

    let tokens = [
        "from", "filter", "select", "sort", "asc", "desc", "take",
        "join", "on", "inner", "left", "right", "full",
        "group_by", "window", "tumbling", "hopping", "session",
        "match", "return", "distinct", "and", "or", "not",
        "last", "|", "{", "}", "(", ")", "[", "]", ",", ":",
        ".", "->", "<-", "*", "?", "==", "!=", "<", "<=", ">", ">=",
        "+", "-", "/", "true", "false", "5m", "1h", "30s",
        "100", "42", "3.14", "\"hello\"", "\"world\"",
        "users", "orders", "metrics", "name", "age", "price",
        "count", "sum", "mean", "max", "min", "percentile",
        "Person", "FOLLOWS", "CO_PURCHASED",
    ];

    for _ in 0..iterations {
        // Strategy 1: random tokens joined with spaces.
        let num_tokens = rng.gen_range(1, 20) as usize;
        let input: String = (0..num_tokens)
            .map(|_| {
                let idx = (rng.next_u64() as usize) % tokens.len();
                tokens[idx]
            })
            .collect::<Vec<_>>()
            .join(" ");
        fuzz_cenql_parser(&input);

        // Strategy 2: random ASCII characters.
        let len = rng.gen_range(1, 100) as usize;
        let input: String = (0..len)
            .map(|_| {
                let c = (rng.next_u64() % 128) as u8;
                c as char
            })
            .collect();
        fuzz_cenql_parser(&input);

        // Strategy 3: valid-looking pipeline with corrupted parts.
        let pipelines = [
            "from users | filter age > ",
            "from users | select { ",
            "from users | sort ",
            "from users | join ",
            "from users | group_by ",
            "from users | window ",
            "from users | match (",
            "from users | return ",
        ];
        let base = pipelines[(rng.next_u64() as usize) % pipelines.len()];
        let suffix: String = (0..rng.gen_range(0, 50))
            .map(|_| {
                let c = (rng.next_u64() % 128) as u8;
                c as char
            })
            .collect();
        fuzz_cenql_parser(&format!("{}{}", base, suffix));

        tested += 1;
    }

    tested
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzz_pax_1000_iterations() {
        let tested = fuzz_pax_block_aggressive(1000, 42);
        println!("[fuzz_pax] tested {} random byte arrays — no panics", tested);
        assert_eq!(tested, 1000);
    }

    #[test]
    fn fuzz_encoding_1000_iterations() {
        let tested = fuzz_encoding_decoders(1000, 42);
        println!("[fuzz_encoding] tested {} random inputs across all codecs — no panics", tested);
        assert_eq!(tested, 1000);
    }

    #[test]
    fn fuzz_cenql_1000_iterations() {
        let tested = fuzz_cenql_parser_aggressive(1000, 42);
        println!("[fuzz_cenql] tested {} random strings — no panics", tested);
        assert_eq!(tested, 1000);
    }

    #[test]
    fn fuzz_pax_empty_input() {
        let _ = fuzz_pax_block(&[]);
    }

    #[test]
    fn fuzz_pax_short_input() {
        let _ = fuzz_pax_block(&[0; 32]);
    }

    #[test]
    fn fuzz_pax_exact_header_size() {
        let _ = fuzz_pax_block(&[0xFF; 64]);
    }

    #[test]
    fn fuzz_pax_all_zeros() {
        let _ = fuzz_pax_block(&[0; 4096]);
    }

    #[test]
    fn fuzz_pax_all_ones() {
        let _ = fuzz_pax_block(&[0xFF; 4096]);
    }

    #[test]
    fn fuzz_cenql_empty_string() {
        fuzz_cenql_parser("");
    }

    #[test]
    fn fuzz_cenql_pipe_only() {
        fuzz_cenql_parser("|");
    }

    #[test]
    fn fuzz_cenql_deeply_nested() {
        let input = "from x".to_string() + &" | filter ".repeat(100);
        fuzz_cenql_parser(&input);
    }
}
