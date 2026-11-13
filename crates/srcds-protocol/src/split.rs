//! Split connectionless packets (Source format, Valve Developer Wiki
//! "Server queries" → "Multi-packet Response Format").
//!
//! ```text
//! i32  header  = -2
//! i32  id      (bit 31 set => payload is bzip2-compressed; unsupported here)
//! u8   total   number of fragments
//! u8   number  index of this fragment (0-based)
//! u16  size    maximum fragment payload size used by the sender
//! ...  payload
//! ```
//!
//! The reassembled payload begins with the `-1` connectionless header.

use crate::bytes::{ByteReader, ByteWriter};
use crate::connectionless::SPLIT_HEADER;
use crate::error::{ProtocolError, Result};

pub const SPLIT_HEADER_LEN: usize = 12;
/// Payload bytes per fragment used by srcds-rs; total datagram = 1260 bytes,
/// comfortably below the 1400-byte routable limit.
pub const DEFAULT_SPLIT_PAYLOAD: usize = 1248;
/// Upper bound on fragments we produce or accept.
pub const MAX_FRAGMENTS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitHeader {
    pub id: i32,
    pub total: u8,
    pub number: u8,
    pub size: u16,
}

impl SplitHeader {
    pub fn compressed(&self) -> bool {
        self.id < 0
    }
}

/// Splits a complete connectionless datagram (starting with `-1`) into fragments.
pub fn split(payload: &[u8], id: i32, fragment_payload: usize) -> Result<Vec<Vec<u8>>> {
    if id < 0 {
        return Err(ProtocolError::InvalidField {
            field: "split_id",
            reason: "compression bit must be clear",
        });
    }
    if fragment_payload == 0 || fragment_payload > u16::MAX as usize {
        return Err(ProtocolError::InvalidField {
            field: "fragment_payload",
            reason: "out of range",
        });
    }
    let total = payload.len().div_ceil(fragment_payload).max(1);
    if total > MAX_FRAGMENTS {
        return Err(ProtocolError::Oversized {
            size: payload.len(),
            limit: MAX_FRAGMENTS * fragment_payload,
        });
    }
    let mut out = Vec::with_capacity(total);
    for (n, chunk) in payload.chunks(fragment_payload).enumerate() {
        let mut w = ByteWriter::with_capacity(SPLIT_HEADER_LEN + chunk.len());
        w.i32(SPLIT_HEADER)?;
        w.i32(id)?;
        w.u8(total as u8)?;
        w.u8(n as u8)?;
        w.u16(fragment_payload as u16)?;
        w.bytes(chunk)?;
        out.push(w.into_inner());
    }
    Ok(out)
}

/// Parses one fragment datagram; returns header and payload slice.
pub fn parse_fragment(datagram: &[u8]) -> Result<(SplitHeader, &[u8])> {
    let mut r = ByteReader::new(datagram);
    let h = r.i32()?;
    if h != SPLIT_HEADER {
        return Err(ProtocolError::InvalidHeader(h as u32));
    }
    let hdr = SplitHeader {
        id: r.i32()?,
        total: r.u8()?,
        number: r.u8()?,
        size: r.u16()?,
    };
    if hdr.total == 0 || hdr.number >= hdr.total || hdr.total as usize > MAX_FRAGMENTS {
        return Err(ProtocolError::InvalidField {
            field: "split",
            reason: "bad fragment numbering",
        });
    }
    if r.remaining() > hdr.size as usize {
        return Err(ProtocolError::Oversized {
            size: r.remaining(),
            limit: hdr.size as usize,
        });
    }
    Ok((hdr, r.rest()))
}

/// Bounded reassembler for a single split id (used by query tools and tests).
#[derive(Debug, Default)]
pub struct Reassembler {
    id: Option<i32>,
    parts: Vec<Option<Vec<u8>>>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one fragment; returns the full payload when complete.
    pub fn push(&mut self, datagram: &[u8]) -> Result<Option<Vec<u8>>> {
        let (h, body) = parse_fragment(datagram)?;
        if h.compressed() {
            return Err(ProtocolError::InvalidField {
                field: "split",
                reason: "compressed split packets unsupported",
            });
        }
        match self.id {
            Some(id) if id != h.id => {
                return Err(ProtocolError::InvalidField {
                    field: "split_id",
                    reason: "mismatched id",
                })
            }
            None => {
                self.id = Some(h.id);
                self.parts = vec![None; h.total as usize];
            }
            _ => {}
        }
        if self.parts.len() != h.total as usize {
            return Err(ProtocolError::InvalidField {
                field: "split_total",
                reason: "inconsistent fragment count",
            });
        }
        self.parts[h.number as usize] = Some(body.to_vec());
        if self.parts.iter().all(Option::is_some) {
            let full = self.parts.iter().flatten().flatten().copied().collect();
            self.id = None;
            self.parts.clear();
            return Ok(Some(full));
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_reassemble_out_of_order() {
        let payload: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let frags = split(&payload, 7, 1248).unwrap();
        assert_eq!(frags.len(), 5);
        assert!(frags.iter().all(|f| f.len() <= SPLIT_HEADER_LEN + 1248));
        let mut r = Reassembler::new();
        for f in frags.iter().rev().skip(1) {
            assert!(r.push(f).unwrap().is_none());
        }
        assert_eq!(r.push(&frags[4]).unwrap().unwrap(), payload);
    }

    #[test]
    fn malformed_fragments() {
        assert!(parse_fragment(b"\xff\xff\xff\xff").is_err());
        // total 0
        assert!(parse_fragment(b"\xfe\xff\xff\xff\x01\0\0\0\x00\x00\x10\x00").is_err());
        // number >= total
        assert!(parse_fragment(b"\xfe\xff\xff\xff\x01\0\0\0\x02\x02\x10\x00").is_err());
        // payload larger than declared size
        assert!(parse_fragment(b"\xfe\xff\xff\xff\x01\0\0\0\x02\x00\x01\x00ab").is_err());
        // truncated header
        assert!(parse_fragment(b"\xfe\xff\xff\xff\x01\0").is_err());
    }

    #[test]
    fn too_many_fragments_rejected() {
        let big = vec![0u8; MAX_FRAGMENTS * 10 + 1];
        assert!(split(&big, 1, 10).is_err());
        assert!(split(b"x", -1, 10).is_err());
    }

    #[test]
    fn mismatched_ids() {
        let a = split(&[1u8; 20], 1, 10).unwrap();
        let b = split(&[1u8; 20], 2, 10).unwrap();
        let mut r = Reassembler::new();
        r.push(&a[0]).unwrap();
        assert!(r.push(&b[1]).is_err());
    }
}
