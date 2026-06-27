//! Best-effort text extraction from the NSAttributedString typedstream blob
//! stored in `message.attributedBody`.
//!
//! Recent macOS often leaves `message.text` NULL and keeps the text in this
//! archived blob. The full format is Apple's typedstream, but the displayed
//! string is reliably stored as a length-prefixed UTF-8 run just after the
//! `NSString` class marker. We parse that run and ignore the rest. This is a
//! heuristic: if the layout does not match, it returns an empty string.

pub fn decode(b: &[u8]) -> String {
    let marker = b"NSString";
    let Some(i) = find(b, marker) else {
        return String::new();
    };
    let rest = &b[i + marker.len()..];

    // The string run is introduced by a 0x2b ('+') marker.
    let Some(j) = rest.iter().position(|&c| c == 0x2b) else {
        return String::new();
    };
    let p = &rest[j + 1..];

    let (n, off) = read_length(p);
    if n == 0 || off + n > p.len() {
        return String::new();
    }
    String::from_utf8_lossy(&p[off..off + n]).into_owned()
}

/// Decodes the typedstream length prefix. Values under 0x80 are a single byte;
/// 0x81 introduces a 2-byte little-endian length; 0x82 a 4-byte one.
fn read_length(p: &[u8]) -> (usize, usize) {
    match p.first() {
        None => (0, 0),
        Some(0x81) => {
            if p.len() < 3 {
                (0, 0)
            } else {
                (p[1] as usize | (p[2] as usize) << 8, 3)
            }
        }
        Some(0x82) => {
            if p.len() < 5 {
                (0, 0)
            } else {
                (
                    p[1] as usize
                        | (p[2] as usize) << 8
                        | (p[3] as usize) << 16
                        | (p[4] as usize) << 24,
                    5,
                )
            }
        }
        Some(&n) => (n as usize, 1),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_blob(len_prefix: &[u8], text: &str) -> Vec<u8> {
        let mut b = b"\x04\x0bstreamtyped\x81NSString\x01\x94\x84\x01".to_vec();
        b.push(0x2b);
        b.extend_from_slice(len_prefix);
        b.extend_from_slice(text.as_bytes());
        b
    }

    #[test]
    fn short_string() {
        let want = "hello world 👋";
        let blob = make_blob(&[want.len() as u8], want);
        assert_eq!(decode(&blob), want);
    }

    #[test]
    fn two_byte_length() {
        let want = "x".repeat(300);
        let n = want.len();
        let blob = make_blob(&[0x81, (n & 0xff) as u8, (n >> 8) as u8], &want);
        assert_eq!(decode(&blob), want);
    }

    #[test]
    fn no_marker() {
        assert_eq!(decode(b"nothing useful here"), "");
    }

    #[test]
    fn empty() {
        assert_eq!(decode(&[]), "");
    }
}
