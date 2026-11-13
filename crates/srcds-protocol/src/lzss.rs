//! LZSS decompression for netchannel payloads.
//!
//! Format (public Source SDK 2013 `tier1/lzss.h` interface): an 8-byte header
//! `"LZSS"` + little-endian `u32` uncompressed size, followed by groups of one command
//! byte and eight items. A set command bit means a back-reference of two bytes
//! (`position = b0 << 4 | b1 >> 4`, `count = (b1 & 0xF) + 1`, copying from
//! `out[len - position - 1]`); a count of 1 terminates the stream. A clear bit means a
//! literal byte. This is an independent, bounds-checked implementation.

use crate::error::{ProtocolError, Result};

pub const LZSS_MAGIC: &[u8; 4] = b"LZSS";
pub const LZSS_HEADER_LEN: usize = 8;

/// Returns the declared uncompressed size if `data` starts with an LZSS header.
pub fn declared_size(data: &[u8]) -> Option<u32> {
    if data.len() < LZSS_HEADER_LEN || &data[..4] != LZSS_MAGIC {
        return None;
    }
    Some(u32::from_le_bytes([data[4], data[5], data[6], data[7]]))
}

/// Decompresses an LZSS blob. Fails (never panics) on truncated input, references
/// before the start of the output, or a declared size above `max_size` / mismatch.
pub fn decompress(data: &[u8], max_size: usize) -> Result<Vec<u8>> {
    let size = declared_size(data).ok_or(ProtocolError::InvalidField {
        field: "lzss",
        reason: "missing LZSS header",
    })? as usize;
    if size > max_size {
        return Err(ProtocolError::Oversized {
            size,
            limit: max_size,
        });
    }
    let mut out = Vec::with_capacity(size);
    let mut i = LZSS_HEADER_LEN;
    let truncated = || ProtocolError::Truncated {
        needed: 1,
        remaining: 0,
    };
    let mut cmd = 0u8;
    let mut left = 0u32;
    loop {
        if left == 0 {
            cmd = *data.get(i).ok_or_else(truncated)?;
            i += 1;
            left = 8;
        }
        left -= 1;
        if cmd & 1 != 0 {
            let b0 = *data.get(i).ok_or_else(truncated)? as usize;
            let b1 = *data.get(i + 1).ok_or_else(truncated)? as usize;
            i += 2;
            let position = (b0 << 4) | (b1 >> 4);
            let count = (b1 & 0x0F) + 1;
            if count == 1 {
                break;
            }
            if position + 1 > out.len() {
                return Err(ProtocolError::InvalidField {
                    field: "lzss",
                    reason: "back-reference before start",
                });
            }
            if out.len() + count > size {
                return Err(ProtocolError::Oversized {
                    size: out.len() + count,
                    limit: size,
                });
            }
            let src = out.len() - position - 1;
            for k in 0..count {
                let b = out[src + k];
                out.push(b);
            }
        } else {
            let b = *data.get(i).ok_or_else(truncated)?;
            i += 1;
            if out.len() >= size {
                return Err(ProtocolError::Oversized {
                    size: out.len() + 1,
                    limit: size,
                });
            }
            out.push(b);
        }
        cmd >>= 1;
    }
    if out.len() != size {
        return Err(ProtocolError::InvalidField {
            field: "lzss",
            reason: "size mismatch",
        });
    }
    Ok(out)
}

/// Encodes `data` as an LZSS stream containing only literals. Valid for any decoder;
/// used in tests and where compression is not worth it.
pub fn encode_literals(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(LZSS_HEADER_LEN + data.len() + data.len() / 8 + 4);
    out.extend_from_slice(LZSS_MAGIC);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    for chunk in data.chunks(8) {
        let terminator_here = chunk.len() < 8;
        // command bits: literals = 0; terminator (if it fits in this group) = 1
        let cmd: u8 = if terminator_here { 1 << chunk.len() } else { 0 };
        out.push(cmd);
        out.extend_from_slice(chunk);
        if terminator_here {
            out.extend_from_slice(&[0, 0]);
            return out;
        }
    }
    out.push(1);
    out.extend_from_slice(&[0, 0]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literals_roundtrip() {
        for n in [0usize, 1, 7, 8, 9, 100, 4096] {
            let data: Vec<u8> = (0..n).map(|i| (i * 31 % 251) as u8).collect();
            let enc = encode_literals(&data);
            assert_eq!(decompress(&enc, 1 << 20).unwrap(), data, "n={n}");
        }
    }

    #[test]
    fn backreference() {
        // "abcabcabc": literals a b c, then ref position=2 (3 back), count=6.
        let mut v = b"LZSS".to_vec();
        v.extend_from_slice(&9u32.to_le_bytes());
        v.push(0b0001_1000); // items: lit, lit, lit, ref, end-ref
        v.extend_from_slice(b"abc");
        v.extend_from_slice(&[0x00, 0x25]); // position 2, count 6
        v.extend_from_slice(&[0x00, 0x00]); // terminator
        assert_eq!(decompress(&v, 100).unwrap(), b"abcabcabc");
    }

    #[test]
    fn hostile_inputs() {
        assert!(decompress(b"", 10).is_err());
        assert!(decompress(b"LZSX\0\0\0\0", 10).is_err());
        let mut v = b"LZSS".to_vec();
        v.extend_from_slice(&1000u32.to_le_bytes());
        assert!(matches!(
            decompress(&v, 10),
            Err(ProtocolError::Oversized { .. })
        ));
        // reference before start
        let mut v = b"LZSS".to_vec();
        v.extend_from_slice(&4u32.to_le_bytes());
        v.extend_from_slice(&[0x01, 0x05, 0x03]);
        assert!(decompress(&v, 100).is_err());
        // truncated everywhere
        let enc = encode_literals(b"hello world, hello");
        for cut in 0..enc.len() {
            let _ = decompress(&enc[..cut], 100);
        }
    }
}
