//! Reference data vendored into a crate as gzip-compressed JSON.
//!
//! Large AWS datasets (the managed IAM policies, the Service Quotas defaults)
//! are embedded with `include_bytes!` as `.json.gz` to keep the binary small,
//! and decoded once on first use.

use std::io::Read;

use serde::de::DeserializeOwned;

/// Gunzip and parse embedded JSON. The data ships with the binary, so a
/// failure is a build defect: it panics with `what` in the message.
pub fn decode_gz_json<T: DeserializeOwned>(bytes: &[u8], what: &str) -> T {
    let mut json = String::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_string(&mut json)
        .unwrap_or_else(|e| panic!("embedded {what} is not valid gzip: {e}"));
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("embedded {what} is not valid JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gz(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    #[test]
    fn decodes_gzipped_json() {
        let v: Vec<u32> = decode_gz_json(&gz(b"[1,2,3]"), "test data");
        assert_eq!(v, [1, 2, 3]);
    }

    #[test]
    #[should_panic(expected = "embedded test data is not valid gzip")]
    fn rejects_non_gzip() {
        let _: Vec<u32> = decode_gz_json(b"[1]", "test data");
    }

    #[test]
    #[should_panic(expected = "embedded test data is not valid JSON")]
    fn rejects_bad_json() {
        let _: Vec<u32> = decode_gz_json(&gz(b"[1,"), "test data");
    }
}
