//! RFC 2047 encoded-words, the form S3 uses to carry non-ASCII user metadata
//! (`x-amz-meta-*`) in HTTP headers.
//!
//! S3 decodes encoded-words in a metadata value before storing it and
//! encodes a stored value that is not pure US-ASCII as `=?UTF-8?B?...?=` when
//! returning it. A raw header byte outside US-ASCII is read as ISO-8859-1.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

/// Encode `value` for an HTTP header: unchanged when it is printable
/// US-ASCII, otherwise a single `=?UTF-8?B?<base64>?=` encoded-word over its
/// UTF-8 bytes. A control character (which a decoded encoded-word can carry)
/// is encoded too, since a header value cannot hold it.
pub fn encode(value: &str) -> String {
    if value
        .bytes()
        .all(|b| b == b'\t' || (0x20..0x7f).contains(&b))
    {
        value.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", BASE64.encode(value.as_bytes()))
    }
}

/// Decode a raw header value. A value made only of encoded-words (separated
/// by whitespace, which RFC 2047 says to drop between adjacent words) is
/// decoded; anything else is read byte-for-byte as ISO-8859-1, which leaves
/// a US-ASCII value unchanged.
pub fn decode(raw: &[u8]) -> String {
    let latin1 = || raw.iter().map(|&b| b as char).collect::<String>();
    let Ok(text) = std::str::from_utf8(raw) else {
        return latin1();
    };
    let mut words = text.split_ascii_whitespace().peekable();
    if words.peek().is_none() {
        return latin1();
    }
    let mut out = String::new();
    for word in words {
        match decode_word(word) {
            Some(decoded) => out.push_str(&decoded),
            None => return latin1(),
        }
    }
    out
}

/// Decode one `=?charset?encoding?text?=` encoded-word. Supports the UTF-8,
/// ISO-8859-1 and US-ASCII charsets and the `B` and `Q` encodings.
fn decode_word(word: &str) -> Option<String> {
    let inner = word.strip_prefix("=?")?.strip_suffix("?=")?;
    let mut parts = inner.splitn(3, '?');
    let charset = parts.next()?;
    let encoding = parts.next()?;
    let text = parts.next()?;
    // RFC 2047 requires at least one encoded character.
    if text.is_empty() {
        return None;
    }
    // RFC 2231 allows a `*language` suffix on the charset.
    let charset = charset.split('*').next()?.to_ascii_lowercase();
    let bytes = match encoding {
        "B" | "b" => BASE64.decode(text).ok()?,
        "Q" | "q" => decode_q(text)?,
        _ => return None,
    };
    match charset.as_str() {
        "utf-8" | "utf8" => String::from_utf8(bytes).ok(),
        "iso-8859-1" | "latin1" => Some(bytes.iter().map(|&b| b as char).collect()),
        "us-ascii" => bytes
            .is_ascii()
            .then(|| bytes.iter().map(|&b| b as char).collect()),
        _ => None,
    }
}

/// The `Q` encoding: `_` is a space and `=XX` is a hex-escaped byte.
fn decode_q(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'_' => out.push(b' '),
            b'=' => {
                let hex = bytes.get(i + 1..i + 3)?;
                if !hex.iter().all(u8::is_ascii_hexdigit) {
                    return None;
                }
                out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_round_trips_unchanged() {
        assert_eq!(encode("AMAZONS3"), "AMAZONS3");
        assert_eq!(decode(b"AMAZONS3"), "AMAZONS3");
        assert_eq!(decode(b""), "");
    }

    #[test]
    fn empty_encoded_word_is_kept_literally() {
        assert_eq!(decode(b"=?UTF-8?B??="), "=?UTF-8?B??=");
        assert_eq!(decode(b"=?UTF-8?Q??="), "=?UTF-8?Q??=");
    }

    #[test]
    fn raw_non_ascii_header_bytes_match_the_s3_docs_example() {
        // From the S3 user guide: `x-amz-meta-nonascii: ÄMÄZÕÑ S3` sent as
        // raw UTF-8 bytes comes back as this encoded-word, because S3 reads
        // the raw bytes as ISO-8859-1.
        let stored = decode("ÄMÄZÕÑ S3".as_bytes());
        assert_eq!(encode(&stored), "=?UTF-8?B?w4PChE3Dg8KEWsODwpXDg8KRIFMz?=");
    }

    #[test]
    fn encoded_words_decode_before_storing() {
        assert_eq!(decode(b"=?UTF-8?B?Y2Fmw6k=?="), "café");
        assert_eq!(decode(b"=?utf-8?q?caf=C3=A9_au_lait?="), "café au lait");
        assert_eq!(decode(b"=?ISO-8859-1?Q?caf=E9?="), "café");
        assert_eq!(decode(b"=?UTF-8?B?Y2Fm?= =?UTF-8?B?w6k=?="), "café");
        assert_eq!(encode("café"), "=?UTF-8?B?Y2Fmw6k=?=");
    }

    #[test]
    fn control_characters_are_encoded_for_the_response() {
        let stored = decode(b"=?UTF-8?B?YQpi?=");
        assert_eq!(stored, "a\nb");
        assert_eq!(encode(&stored), "=?UTF-8?B?YQpi?=");
        assert_eq!(encode("a\tb c"), "a\tb c");
    }

    #[test]
    fn malformed_or_mixed_words_stay_literal() {
        assert_eq!(decode(b"=?UTF-8?B?***?="), "=?UTF-8?B?***?=");
        assert_eq!(decode(b"=?UTF-8?X?abc?="), "=?UTF-8?X?abc?=");
        assert_eq!(decode(b"=?KOI8-R?B?abc?="), "=?KOI8-R?B?abc?=");
        assert_eq!(decode(b"=?UTF-8?Q?a=Z?="), "=?UTF-8?Q?a=Z?=");
        assert_eq!(decode(b"=?UTF-8?Q?a=+F?="), "=?UTF-8?Q?a=+F?=");
        assert_eq!(
            decode(b"hello =?UTF-8?B?Y2Fmw6k=?="),
            "hello =?UTF-8?B?Y2Fmw6k=?="
        );
    }
}
