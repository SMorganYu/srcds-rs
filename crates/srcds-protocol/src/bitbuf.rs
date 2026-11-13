//! Bit-level reader/writer.
//!
//! Source netchannel payloads are bit streams. The documented convention (Source SDK
//! `bitbuf.h` interface; public Valve Developer Wiki) is that bits are packed
//! least-significant-bit first into a little-endian byte stream, so bit `i` of the
//! stream is bit `i % 8` of byte `i / 8`. This module implements that convention
//! independently; it is exercised by roundtrip and known-vector tests.

use crate::error::{ProtocolError, Result};

/// Bounded LSB-first bit reader. Reads past the end return `Truncated` and never
/// advance the cursor.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Total readable bits (may be less than `data.len() * 8`).
    limit: usize,
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            limit: data.len() * 8,
            pos: 0,
        }
    }

    /// Reader restricted to the first `bits` bits of `data`.
    pub fn with_bit_len(data: &'a [u8], bits: usize) -> Result<Self> {
        if bits > data.len() * 8 {
            return Err(ProtocolError::InvalidField {
                field: "bit_len",
                reason: "longer than buffer",
            });
        }
        Ok(Self {
            data,
            limit: bits,
            pos: 0,
        })
    }

    pub fn bit_position(&self) -> usize {
        self.pos
    }

    pub fn bits_remaining(&self) -> usize {
        self.limit - self.pos
    }

    fn need(&self, bits: usize) -> Result<()> {
        if bits > self.bits_remaining() {
            Err(ProtocolError::Truncated {
                needed: bits,
                remaining: self.bits_remaining(),
            })
        } else {
            Ok(())
        }
    }

    pub fn read_bit(&mut self) -> Result<bool> {
        self.need(1)?;
        let b = (self.data[self.pos >> 3] >> (self.pos & 7)) & 1;
        self.pos += 1;
        Ok(b != 0)
    }

    /// Reads `n` (0..=32) bits as an unsigned integer.
    pub fn read_ubits(&mut self, n: u32) -> Result<u32> {
        if n > 32 {
            return Err(ProtocolError::InvalidField {
                field: "bit_count",
                reason: "more than 32",
            });
        }
        self.need(n as usize)?;
        let mut out: u64 = 0;
        let mut got = 0u32;
        while got < n {
            let byte = self.data[self.pos >> 3] as u64;
            let off = (self.pos & 7) as u32;
            let take = (8 - off).min(n - got);
            let chunk = (byte >> off) & ((1u64 << take) - 1);
            out |= chunk << got;
            got += take;
            self.pos += take as usize;
        }
        Ok(out as u32)
    }

    /// Reads `n` (1..=32) bits as a two's-complement signed integer.
    pub fn read_sbits(&mut self, n: u32) -> Result<i32> {
        if n == 0 {
            return Err(ProtocolError::InvalidField {
                field: "bit_count",
                reason: "signed read of 0 bits",
            });
        }
        let v = self.read_ubits(n)?;
        let shift = 32 - n;
        Ok(((v << shift) as i32) >> shift)
    }

    pub fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_ubits(8)? as u8)
    }
    pub fn read_u16(&mut self) -> Result<u16> {
        Ok(self.read_ubits(16)? as u16)
    }
    pub fn read_u32(&mut self) -> Result<u32> {
        self.read_ubits(32)
    }
    pub fn read_f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.read_u32()?))
    }

    /// Protobuf-style base-128 varint (max 5 bytes for 32 bits).
    pub fn read_varint32(&mut self) -> Result<u32> {
        let start = self.pos;
        let mut out: u32 = 0;
        for i in 0..5 {
            let b = match self.read_u8() {
                Ok(b) => b,
                Err(e) => {
                    self.pos = start;
                    return Err(e);
                }
            };
            out |= ((b & 0x7f) as u32) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(out);
            }
        }
        self.pos = start;
        Err(ProtocolError::InvalidField {
            field: "varint32",
            reason: "more than 5 bytes",
        })
    }

    /// Copies `out.len()` bytes (not necessarily byte-aligned).
    pub fn read_bytes(&mut self, out: &mut [u8]) -> Result<()> {
        self.need(out.len() * 8)?;
        if self.pos & 7 == 0 {
            let start = self.pos >> 3;
            out.copy_from_slice(&self.data[start..start + out.len()]);
            self.pos += out.len() * 8;
        } else {
            for b in out.iter_mut() {
                *b = self.read_u8()?;
            }
        }
        Ok(())
    }

    /// Reads a NUL-terminated string of at most `limit` bytes (excluding NUL).
    pub fn read_string(&mut self, limit: usize) -> Result<String> {
        let start = self.pos;
        let mut bytes = Vec::new();
        loop {
            let b = match self.read_u8() {
                Ok(b) => b,
                Err(e) => {
                    self.pos = start;
                    return Err(e);
                }
            };
            if b == 0 {
                break;
            }
            if bytes.len() == limit {
                self.pos = start;
                return Err(ProtocolError::UnterminatedString { limit });
            }
            bytes.push(b);
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn skip_bits(&mut self, n: usize) -> Result<()> {
        self.need(n)?;
        self.pos += n;
        Ok(())
    }
}

/// LSB-first bit writer with a hard capacity in bytes.
#[derive(Debug, Clone)]
pub struct BitWriter {
    buf: Vec<u8>,
    bits: usize,
    capacity_bits: usize,
}

impl BitWriter {
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            buf: Vec::with_capacity(bytes.min(4096)),
            bits: 0,
            capacity_bits: bytes * 8,
        }
    }

    pub fn bit_len(&self) -> usize {
        self.bits
    }

    pub fn byte_len(&self) -> usize {
        self.bits.div_ceil(8)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    fn reserve(&mut self, bits: usize) -> Result<()> {
        let needed = self.bits + bits;
        if needed > self.capacity_bits {
            return Err(ProtocolError::Overflow {
                needed: needed.div_ceil(8),
                capacity: self.capacity_bits / 8,
            });
        }
        self.buf.resize(needed.div_ceil(8), 0);
        Ok(())
    }

    pub fn write_bit(&mut self, v: bool) -> Result<()> {
        self.reserve(1)?;
        if v {
            self.buf[self.bits >> 3] |= 1 << (self.bits & 7);
        }
        self.bits += 1;
        Ok(())
    }

    /// Writes the low `n` (0..=32) bits of `v`. Bits above `n` must be zero.
    pub fn write_ubits(&mut self, v: u32, n: u32) -> Result<()> {
        if n > 32 {
            return Err(ProtocolError::InvalidField {
                field: "bit_count",
                reason: "more than 32",
            });
        }
        if n < 32 && (v >> n) != 0 {
            return Err(ProtocolError::InvalidField {
                field: "value",
                reason: "does not fit in bit count",
            });
        }
        self.reserve(n as usize)?;
        let mut left = n;
        let mut val = v as u64;
        while left > 0 {
            let off = (self.bits & 7) as u32;
            let take = (8 - off).min(left);
            let chunk = (val & ((1u64 << take) - 1)) as u8;
            self.buf[self.bits >> 3] |= chunk << off;
            val >>= take;
            left -= take;
            self.bits += take as usize;
        }
        Ok(())
    }

    pub fn write_sbits(&mut self, v: i32, n: u32) -> Result<()> {
        if n == 0 || n > 32 {
            return Err(ProtocolError::InvalidField {
                field: "bit_count",
                reason: "signed write needs 1..=32 bits",
            });
        }
        let min = -(1i64 << (n - 1));
        let max = (1i64 << (n - 1)) - 1;
        if (v as i64) < min || (v as i64) > max {
            return Err(ProtocolError::InvalidField {
                field: "value",
                reason: "does not fit in bit count",
            });
        }
        let mask = if n == 32 { u32::MAX } else { (1u32 << n) - 1 };
        self.write_ubits((v as u32) & mask, n)
    }

    pub fn write_u8(&mut self, v: u8) -> Result<()> {
        self.write_ubits(v as u32, 8)
    }
    pub fn write_u16(&mut self, v: u16) -> Result<()> {
        self.write_ubits(v as u32, 16)
    }
    pub fn write_u32(&mut self, v: u32) -> Result<()> {
        self.write_ubits(v, 32)
    }
    pub fn write_f32(&mut self, v: f32) -> Result<()> {
        self.write_u32(v.to_bits())
    }

    pub fn write_varint32(&mut self, mut v: u32) -> Result<()> {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                return self.write_u8(byte);
            }
            self.write_u8(byte | 0x80)?;
        }
    }

    pub fn write_bytes(&mut self, b: &[u8]) -> Result<()> {
        self.reserve(b.len() * 8)?;
        // `reserve` grew the buffer; undo the growth accounting by writing bitwise.
        self.buf.truncate(self.bits.div_ceil(8));
        for &x in b {
            self.write_u8(x)?;
        }
        Ok(())
    }

    /// Appends the first `nbits` bits of `src` (LSB-first, as produced by another
    /// writer or read from the wire).
    pub fn append_bits(&mut self, src: &[u8], nbits: usize) -> Result<()> {
        if nbits > src.len() * 8 {
            return Err(ProtocolError::Truncated {
                needed: nbits.div_ceil(8),
                remaining: src.len(),
            });
        }
        let full = nbits / 8;
        if self.bits % 8 == 0 {
            self.reserve(full * 8)?;
            let start = self.bits / 8;
            self.buf[start..start + full].copy_from_slice(&src[..full]);
            self.bits += full * 8;
        } else {
            for &b in &src[..full] {
                self.write_u8(b)?;
            }
        }
        let rest = (nbits % 8) as u32;
        if rest > 0 {
            let last = src[full] & ((1u8 << rest) - 1);
            self.write_ubits(last as u32, rest)?;
        }
        Ok(())
    }

    /// Appends everything written to `other`.
    pub fn append(&mut self, other: &BitWriter) -> Result<()> {
        self.append_bits(&other.buf, other.bits)
    }

    /// Bits needed to reach the next byte boundary (0..=7).
    pub fn bits_to_byte_boundary(&self) -> u32 {
        ((8 - (self.bits % 8)) % 8) as u32
    }

    pub fn write_string(&mut self, s: &str) -> Result<()> {
        if s.as_bytes().contains(&0) {
            return Err(ProtocolError::InvalidField {
                field: "string",
                reason: "interior NUL",
            });
        }
        self.write_bytes(s.as_bytes())?;
        self.write_u8(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsb_first_known_vector() {
        // 3 bits 0b101, then 5 bits 0b11000 -> byte 0b11000_101 = 0xC5
        let mut w = BitWriter::with_capacity(4);
        w.write_ubits(0b101, 3).unwrap();
        w.write_ubits(0b11000, 5).unwrap();
        assert_eq!(w.as_bytes(), &[0xC5]);
        let mut r = BitReader::new(&[0xC5]);
        assert_eq!(r.read_ubits(3).unwrap(), 0b101);
        assert_eq!(r.read_ubits(5).unwrap(), 0b11000);
    }

    #[test]
    fn unaligned_roundtrip_mixed() {
        let mut w = BitWriter::with_capacity(128);
        w.write_bit(true).unwrap();
        w.write_ubits(0x1234_5678, 32).unwrap();
        w.write_sbits(-3, 5).unwrap();
        w.write_varint32(300).unwrap();
        w.write_varint32(u32::MAX).unwrap();
        w.write_string("hello").unwrap();
        w.write_f32(-0.25).unwrap();
        w.write_bytes(&[9, 8, 7]).unwrap();
        let bits = w.bit_len();
        let data = w.into_bytes();
        let mut r = BitReader::with_bit_len(&data, bits).unwrap();
        assert!(r.read_bit().unwrap());
        assert_eq!(r.read_ubits(32).unwrap(), 0x1234_5678);
        assert_eq!(r.read_sbits(5).unwrap(), -3);
        assert_eq!(r.read_varint32().unwrap(), 300);
        assert_eq!(r.read_varint32().unwrap(), u32::MAX);
        assert_eq!(r.read_string(16).unwrap(), "hello");
        assert_eq!(r.read_f32().unwrap(), -0.25);
        let mut b = [0u8; 3];
        r.read_bytes(&mut b).unwrap();
        assert_eq!(b, [9, 8, 7]);
        assert_eq!(r.bits_remaining(), 0);
        assert!(r.read_bit().is_err());
    }

    #[test]
    fn varint_known_encoding() {
        let mut w = BitWriter::with_capacity(8);
        w.write_varint32(300).unwrap();
        assert_eq!(w.as_bytes(), &[0xAC, 0x02]);
    }

    #[test]
    fn truncation_does_not_advance() {
        let mut r = BitReader::new(&[0xFF]);
        r.read_ubits(5).unwrap();
        assert!(matches!(
            r.read_ubits(4),
            Err(ProtocolError::Truncated { .. })
        ));
        assert_eq!(r.bit_position(), 5);
        assert_eq!(r.read_ubits(3).unwrap(), 0b111);
    }

    #[test]
    fn malformed_varint_and_string() {
        let mut r = BitReader::new(&[0xFF; 6]);
        assert!(r.read_varint32().is_err());
        assert_eq!(r.bit_position(), 0);
        let mut r = BitReader::new(b"abcdef");
        assert!(r.read_string(3).is_err());
        assert_eq!(r.bit_position(), 0);
        assert!(r.read_string(100).is_err()); // no terminator at all
    }

    #[test]
    fn writer_rejects_bad_values_and_overflow() {
        let mut w = BitWriter::with_capacity(1);
        assert!(w.write_ubits(4, 2).is_err());
        assert!(w.write_sbits(16, 5).is_err());
        assert!(w.write_ubits(0, 33).is_err());
        w.write_ubits(0xFF, 8).unwrap();
        assert!(w.write_bit(true).is_err());
        assert_eq!(w.bit_len(), 8);
    }

    #[test]
    fn exhaustive_small_widths() {
        for n in 1..=32u32 {
            let v = if n == 32 {
                0xDEAD_BEEF
            } else {
                0xDEAD_BEEF & ((1 << n) - 1)
            };
            let mut w = BitWriter::with_capacity(16);
            w.write_bit(true).unwrap(); // force misalignment
            w.write_ubits(v, n).unwrap();
            let data = w.into_bytes();
            let mut r = BitReader::new(&data);
            r.read_bit().unwrap();
            assert_eq!(r.read_ubits(n).unwrap(), v, "width {n}");
        }
    }
}
