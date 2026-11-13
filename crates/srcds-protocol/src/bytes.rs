//! Byte-aligned little-endian reader/writer used by connectionless packets.

use crate::error::{ProtocolError, Result};

/// Bounded, non-panicking little-endian cursor over a byte slice.
#[derive(Debug, Clone)]
pub struct ByteReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(ProtocolError::Truncated {
                needed: n,
                remaining: self.remaining(),
            });
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    pub fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.array()?))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array()?))
    }
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_le_bytes(self.array()?))
    }

    /// Reads a NUL-terminated string of at most `limit` bytes (excluding the NUL).
    /// Invalid UTF-8 is replaced lossily; Source strings are byte strings.
    pub fn cstr(&mut self, limit: usize) -> Result<String> {
        let rest = self.rest();
        let window = &rest[..rest.len().min(limit + 1)];
        match window.iter().position(|&b| b == 0) {
            Some(n) => {
                let s = String::from_utf8_lossy(&window[..n]).into_owned();
                self.pos += n + 1;
                Ok(s)
            }
            None if rest.len() <= limit => Err(ProtocolError::Truncated {
                needed: 1,
                remaining: 0,
            }),
            None => Err(ProtocolError::UnterminatedString { limit }),
        }
    }
}

/// Little-endian writer with a hard capacity, so responses can never exceed a
/// configured datagram size.
#[derive(Debug, Clone)]
pub struct ByteWriter {
    buf: Vec<u8>,
    capacity: usize,
}

impl ByteWriter {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity.min(4096)),
            capacity,
        }
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    pub fn bytes(&mut self, b: &[u8]) -> Result<()> {
        let needed = self.buf.len() + b.len();
        if needed > self.capacity {
            return Err(ProtocolError::Overflow {
                needed,
                capacity: self.capacity,
            });
        }
        self.buf.extend_from_slice(b);
        Ok(())
    }

    pub fn u8(&mut self, v: u8) -> Result<()> {
        self.bytes(&[v])
    }
    pub fn u16(&mut self, v: u16) -> Result<()> {
        self.bytes(&v.to_le_bytes())
    }
    pub fn i16(&mut self, v: i16) -> Result<()> {
        self.bytes(&v.to_le_bytes())
    }
    pub fn u32(&mut self, v: u32) -> Result<()> {
        self.bytes(&v.to_le_bytes())
    }
    pub fn i32(&mut self, v: i32) -> Result<()> {
        self.bytes(&v.to_le_bytes())
    }
    pub fn u64(&mut self, v: u64) -> Result<()> {
        self.bytes(&v.to_le_bytes())
    }
    pub fn f32(&mut self, v: f32) -> Result<()> {
        self.bytes(&v.to_le_bytes())
    }

    /// Writes `s` followed by NUL. Interior NULs are rejected because they would
    /// silently truncate the field on the receiving side.
    pub fn cstr(&mut self, s: &str) -> Result<()> {
        if s.as_bytes().contains(&0) {
            return Err(ProtocolError::InvalidField {
                field: "string",
                reason: "interior NUL",
            });
        }
        self.bytes(s.as_bytes())?;
        self.u8(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_scalars_and_strings() {
        let mut w = ByteWriter::with_capacity(64);
        w.u8(1).unwrap();
        w.u16(0xBEEF).unwrap();
        w.i32(-5).unwrap();
        w.u64(0x0102030405060708).unwrap();
        w.f32(1.5).unwrap();
        w.cstr("gm_construct").unwrap();
        let data = w.into_inner();
        let mut r = ByteReader::new(&data);
        assert_eq!(r.u8().unwrap(), 1);
        assert_eq!(r.u16().unwrap(), 0xBEEF);
        assert_eq!(r.i32().unwrap(), -5);
        assert_eq!(r.u64().unwrap(), 0x0102030405060708);
        assert_eq!(r.f32().unwrap(), 1.5);
        assert_eq!(r.cstr(64).unwrap(), "gm_construct");
        assert!(r.is_empty());
    }

    #[test]
    fn truncated_reads_fail() {
        let mut r = ByteReader::new(&[1, 2, 3]);
        assert!(matches!(r.u32(), Err(ProtocolError::Truncated { .. })));
        // Failed read does not advance.
        assert_eq!(r.position(), 0);
        assert_eq!(r.u16().unwrap(), 0x0201);
    }

    #[test]
    fn unterminated_string_limits() {
        let mut r = ByteReader::new(b"abcdef");
        assert!(matches!(r.cstr(16), Err(ProtocolError::Truncated { .. })));
        assert!(matches!(
            r.cstr(3),
            Err(ProtocolError::UnterminatedString { limit: 3 })
        ));
        let mut r = ByteReader::new(b"abc\0");
        assert_eq!(r.cstr(3).unwrap(), "abc");
    }

    #[test]
    fn writer_capacity_enforced() {
        let mut w = ByteWriter::with_capacity(3);
        w.u16(1).unwrap();
        assert!(matches!(w.u16(1), Err(ProtocolError::Overflow { .. })));
        assert_eq!(w.len(), 2);
        assert!(w.cstr("a\0b").is_err());
    }
}
