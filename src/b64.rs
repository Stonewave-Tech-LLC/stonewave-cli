//! Base64 (standard and URL-safe alphabets) without a dependency. Small on purpose:
//! it reads SSH public keys and writes the keys and signatures of the relay protocol.
#![allow(dead_code)] // the URL-safe half is used by the relay protocol (next change)

const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn encode_with(alphabet: &[u8; 64], data: &[u8], pad: bool) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        let symbols = chunk.len() + 1;
        for i in 0..4 {
            if i < symbols {
                out.push(char::from(alphabet[((n >> (18 - 6 * i)) & 63) as usize]));
            } else if pad {
                out.push('=');
            }
        }
    }
    out
}

/// Standard alphabet with padding (what SSH public keys use).
pub fn encode(data: &[u8]) -> String {
    encode_with(STD, data, true)
}

/// URL-safe alphabet without padding (what the relay protocol uses).
pub fn encode_url(data: &[u8]) -> String {
    encode_with(URL, data, false)
}

fn decode_with(alphabet: &[u8; 64], text: &str, padded: bool) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let body = if padded {
        if bytes.len() % 4 != 0 {
            return None;
        }
        let pads = bytes.iter().rev().take_while(|&&b| b == b'=').count();
        if pads > 2 {
            return None;
        }
        &bytes[..bytes.len() - pads]
    } else {
        bytes
    };
    if body.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(body.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in body {
        let v = alphabet.iter().position(|&a| a == b)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be zero, so one value has one text.
    if acc != 0 {
        return None;
    }
    Some(out)
}

pub fn decode(text: &str) -> Option<Vec<u8>> {
    decode_with(STD, text, true)
}

pub fn decode_url(text: &str) -> Option<Vec<u8>> {
    decode_with(URL, text, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(encode_url(&[0xfb, 0xff, 0xfe]), "-__-");
        assert_eq!(encode(&[0xfb, 0xff, 0xfe]), "+//+");
    }

    #[test]
    fn round_trip() {
        for len in 0..40usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            assert_eq!(decode(&encode(&data)).unwrap(), data);
            assert_eq!(decode_url(&encode_url(&data)).unwrap(), data);
        }
    }

    #[test]
    fn rejects_bad_input() {
        assert!(decode("Zg=").is_none(), "bad padding length");
        assert!(decode("Z===").is_none(), "too much padding");
        assert!(decode("Zh==").is_none(), "non-zero leftover bits");
        assert!(decode("Zm9v!A==").is_none(), "bad symbol");
        assert!(decode_url("Zm9v+").is_none(), "standard symbol in url text");
        assert!(decode_url("A").is_none(), "one symbol cannot be a byte");
    }
}
