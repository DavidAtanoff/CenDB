//! Zero-allocation CSV parser with streaming support.
//!
//! Parses comma-delimited data field-by-field, returning `&[u8]` slices
//! into the original input buffer. No String allocations per field.

/// A parsed CSV record: a list of field byte slices.
#[derive(Debug, Clone)]
pub struct CsvRecord<'a> {
    pub fields: Vec<&'a [u8]>,
}

impl<'a> CsvRecord<'a> {
    #[inline]
    pub fn field_count(&self) -> usize {
        self.fields.len()
    }

    pub fn field(&self, idx: usize) -> Option<&'a [u8]> {
        self.fields.get(idx).copied()
    }
}

/// Zero-allocation CSV parser. Borrows the input buffer.
pub struct CsvParser<'a> {
    input: &'a [u8],
    pos: usize,
    delimiter: u8,
    has_header: bool,
    header_consumed: bool,
}

impl<'a> CsvParser<'a> {
    pub fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            pos: 0,
            delimiter: b',',
            has_header: true,
            header_consumed: false,
        }
    }

    pub fn with_delimiter(mut self, delimiter: u8) -> Self {
        self.delimiter = delimiter;
        self
    }

    pub fn no_header(mut self) -> Self {
        self.has_header = false;
        self
    }

    /// Parse and return the header row (if present).
    pub fn parse_header(&mut self) -> Option<CsvRecord<'a>> {
        if !self.has_header || self.header_consumed {
            return None;
        }
        self.header_consumed = true;
        self.parse_record()
    }

    /// Parse the next record. Returns `None` at end of input.
    pub fn next_record(&mut self) -> Option<CsvRecord<'a>> {
        if self.has_header && !self.header_consumed {
            self.header_consumed = true;
            self.parse_record(); // Skip header.
        }
        self.parse_record()
    }

    fn parse_record(&mut self) -> Option<CsvRecord<'a>> {
        if self.pos >= self.input.len() {
            return None;
        }

        let mut fields = Vec::with_capacity(16);

        loop {
            // Skip leading whitespace (not newlines).
            while self.pos < self.input.len()
                && (self.input[self.pos] == b' ' || self.input[self.pos] == b'\t')
            {
                self.pos += 1;
            }

            let field_start = self.pos;

            if self.pos < self.input.len() && self.input[self.pos] == b'"' {
                // Quoted field.
                self.pos += 1; // Skip opening quote.
                let content_start = self.pos;
                while self.pos < self.input.len() && self.input[self.pos] != b'"' {
                    self.pos += 1;
                }
                fields.push(&self.input[content_start..self.pos]);
                if self.pos < self.input.len() {
                    self.pos += 1; // Skip closing quote.
                }
                // Skip to delimiter or newline.
                while self.pos < self.input.len()
                    && self.input[self.pos] != self.delimiter
                    && self.input[self.pos] != b'\n'
                    && self.input[self.pos] != b'\r'
                {
                    self.pos += 1;
                }
            } else {
                // Unquoted field: scan to delimiter or newline.
                while self.pos < self.input.len()
                    && self.input[self.pos] != self.delimiter
                    && self.input[self.pos] != b'\n'
                    && self.input[self.pos] != b'\r'
                {
                    self.pos += 1;
                }
                fields.push(&self.input[field_start..self.pos]);
            }

            // Check what stopped us.
            if self.pos >= self.input.len() {
                break; // End of input.
            }

            let b = self.input[self.pos];
            if b == self.delimiter {
                self.pos += 1; // Skip delimiter, continue to next field.
                continue;
            }

            // Newline (handle \n, \r, \r\n).
            if b == b'\r' {
                self.pos += 1;
                if self.pos < self.input.len() && self.input[self.pos] == b'\n' {
                    self.pos += 1;
                }
            } else if b == b'\n' {
                self.pos += 1;
            }
            break;
        }

        if fields.is_empty() && self.pos == 0 {
            return None;
        }

        Some(CsvRecord { fields })
    }

    /// Parse all remaining records.
    pub fn collect_all(mut self) -> Vec<CsvRecord<'a>> {
        let mut records = Vec::new();
        while let Some(record) = self.next_record() {
            records.push(record);
        }
        records
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_csv() {
        let input = b"name,age,city\nAlice,30,Berlin\nBob,25,Munich";
        let mut parser = CsvParser::new(input);
        let header = parser.parse_header().unwrap();
        assert_eq!(header.field_count(), 3);
        assert_eq!(header.field(0), Some(b"name" as &[u8]));

        let row1 = parser.next_record().unwrap();
        assert_eq!(row1.field(0), Some(b"Alice" as &[u8]));
        assert_eq!(row1.field(1), Some(b"30" as &[u8]));
        assert_eq!(row1.field(2), Some(b"Berlin" as &[u8]));

        let row2 = parser.next_record().unwrap();
        assert_eq!(row2.field(0), Some(b"Bob" as &[u8]));
    }

    #[test]
    fn parse_quoted_fields() {
        let input = b"name,desc\n\"Alice\",\"Hello, World\"";
        let mut parser = CsvParser::new(input);
        parser.parse_header();
        let row = parser.next_record().unwrap();
        assert_eq!(row.field(0), Some(b"Alice" as &[u8]));
        assert_eq!(row.field(1), Some(b"Hello, World" as &[u8]));
    }

    #[test]
    fn parse_tsv() {
        let input = b"a\tb\tc\n1\t2\t3";
        let mut parser = CsvParser::new(input).with_delimiter(b'\t');
        parser.parse_header();
        let row = parser.next_record().unwrap();
        assert_eq!(row.field(0), Some(b"1" as &[u8]));
        assert_eq!(row.field(2), Some(b"3" as &[u8]));
    }

    #[test]
    fn parse_no_header() {
        let input = b"1,2,3\n4,5,6";
        let parser = CsvParser::new(input).no_header();
        let records = parser.collect_all();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].field(0), Some(b"1" as &[u8]));
    }

    #[test]
    fn parse_large_csv() {
        let mut input = String::from("id,name,email,age,score\n");
        for i in 0..10_000 {
            input.push_str(&format!(
                "{},user_{},user_{}@test.com,{},{}\n",
                i, i, i, 20 + i % 50, i as f64 * 1.5
            ));
        }
        let start = std::time::Instant::now();
        let parser = CsvParser::new(input.as_bytes());
        let records = parser.collect_all();
        let elapsed = start.elapsed();
        println!(
            "[csv_bench] parsed {} records in {:?} ({:.0} records/sec)",
            records.len(),
            elapsed,
            records.len() as f64 / elapsed.as_secs_f64()
        );
        assert_eq!(records.len(), 10_000);
        assert_eq!(records[0].field_count(), 5);
    }
}
