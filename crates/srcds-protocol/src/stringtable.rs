//! Network string tables: creation messages and entry encoding.
//!
//! GMod `svc_CreateStringTable` header (solved from capture, `docs/PROTOCOL.md`):
//! `name\0 | log2(max_entries):5 | num_entries:(log2+1) | length:varint | fixed:1
//! [ud_size:12 ud_bits:4] | compressed:1 | data`. Entry encoding: per entry a
//! "next index" bit (else explicit index), a has-string bit, a substring bit (we never
//! use history), the string, a has-userdata bit and either `ud_bits` bits (fixed) or a
//! 19-bit byte length followed by the bytes.

use crate::bitbuf::BitWriter;
use crate::error::{ProtocolError, Result};
use crate::netchan::NETMSG_TYPE_BITS;
use crate::netmsg::{
    MAX_TABLES_BITS, STRINGTABLE_USERDATA_BITS, SVC_CREATESTRINGTABLE, SVC_UPDATESTRINGTABLE,
};

/// Maximum user-data size per entry we will encode.
pub const MAX_USERDATA: usize = 1 << 14;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserData {
    None,
    /// Fixed-size tables: `bits` bits of `value`.
    Fixed(u32),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub string: String,
    pub data: UserData,
}

#[derive(Debug, Clone)]
pub struct StringTable {
    pub name: String,
    /// log2 of the maximum number of entries.
    pub max_entries_log2: u32,
    /// `Some((bytes, bits))` for fixed-size user data.
    pub fixed_userdata: Option<(u32, u32)>,
    pub entries: Vec<Entry>,
}

impl StringTable {
    pub fn new(name: &str, max_entries_log2: u32, fixed_userdata: Option<(u32, u32)>) -> Self {
        Self {
            name: name.to_string(),
            max_entries_log2,
            fixed_userdata,
            entries: Vec::new(),
        }
    }

    pub fn max_entries(&self) -> usize {
        1usize << self.max_entries_log2
    }

    pub fn push(&mut self, s: &str, data: UserData) -> Result<usize> {
        if self.entries.len() >= self.max_entries() {
            return Err(ProtocolError::Overflow {
                needed: self.entries.len() + 1,
                capacity: self.max_entries(),
            });
        }
        self.entries.push(Entry {
            string: s.to_string(),
            data,
        });
        Ok(self.entries.len() - 1)
    }

    fn entry_bits(&self) -> u32 {
        self.max_entries_log2
    }

    /// Encodes entries `range` (all as consecutive new entries).
    fn encode_entries(&self, from: usize, to: usize) -> Result<BitWriter> {
        let mut w = BitWriter::with_capacity(
            64 + self.entries[from..to].iter().map(entry_size).sum::<usize>(),
        );
        let mut last: isize = from as isize - 1;
        for (i, e) in self.entries[from..to].iter().enumerate() {
            let idx = from + i;
            if idx as isize == last + 1 {
                w.write_bit(true)?;
            } else {
                w.write_bit(false)?;
                w.write_ubits(idx as u32, self.entry_bits())?;
            }
            last = idx as isize;
            w.write_bit(true)?; // has string
            w.write_bit(false)?; // no substring history
            w.write_string(&e.string)?;
            match (&e.data, self.fixed_userdata) {
                (UserData::None, _) => w.write_bit(false)?,
                (UserData::Fixed(v), Some((_, bits))) => {
                    w.write_bit(true)?;
                    w.write_ubits(*v, bits)?;
                }
                (UserData::Bytes(b), None) => {
                    if b.len() > MAX_USERDATA {
                        return Err(ProtocolError::Oversized {
                            size: b.len(),
                            limit: MAX_USERDATA,
                        });
                    }
                    w.write_bit(true)?;
                    w.write_ubits(b.len() as u32, STRINGTABLE_USERDATA_BITS)?;
                    w.write_bytes(b)?;
                }
                _ => {
                    return Err(ProtocolError::InvalidField {
                        field: "stringtable",
                        reason: "user data kind does not match table",
                    })
                }
            }
        }
        Ok(w)
    }

    /// Appends `svc_CreateStringTable` carrying all current entries.
    pub fn write_create(&self, w: &mut BitWriter) -> Result<()> {
        let data = self.encode_entries(0, self.entries.len())?;
        w.write_ubits(SVC_CREATESTRINGTABLE, NETMSG_TYPE_BITS)?;
        w.write_string(&self.name)?;
        w.write_ubits(self.max_entries_log2, 5)?;
        w.write_ubits(self.entries.len() as u32, self.max_entries_log2 + 1)?;
        w.write_varint32(data.bit_len() as u32)?;
        match self.fixed_userdata {
            Some((bytes, bits)) => {
                w.write_bit(true)?;
                w.write_ubits(bytes, 12)?;
                w.write_ubits(bits, 4)?;
            }
            None => w.write_bit(false)?,
        }
        w.write_bit(false)?; // not compressed
        w.append(&data)
    }

    /// Appends `svc_UpdateStringTable` for entries `from..` (newly added entries).
    pub fn write_update(&self, table_id: u32, from: usize, w: &mut BitWriter) -> Result<()> {
        let n = self.entries.len().saturating_sub(from);
        let data = self.encode_entries(from, self.entries.len())?;
        w.write_ubits(SVC_UPDATESTRINGTABLE, NETMSG_TYPE_BITS)?;
        w.write_ubits(table_id, MAX_TABLES_BITS)?;
        if n == 1 {
            w.write_bit(false)?;
        } else {
            w.write_bit(true)?;
            w.write_u16(n as u16)?;
        }
        w.write_ubits(data.bit_len() as u32, 20)?;
        w.append(&data)
    }
}

fn entry_size(e: &Entry) -> usize {
    e.string.len()
        + 8
        + match &e.data {
            UserData::Bytes(b) => b.len() + 4,
            _ => 4,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitbuf::BitReader;

    /// The captured reference `downloadables` table: one entry, no user data, 180 data
    /// bits, header with log2 = 13.
    #[test]
    fn downloadables_matches_reference_sizes() {
        let mut t = StringTable::new("downloadables", 13, None);
        t.push("maps\\gm_construct.bsp", UserData::None).unwrap();
        let mut w = BitWriter::with_capacity(256);
        t.write_create(&mut w).unwrap();
        // 6 type + 14*8 name + 5 + 14 entries + 16 varint + 1 fixed + 1 comp + 180 data
        // = 335 bits, exactly the span of the reference message (bits 70..405).
        assert_eq!(w.bit_len(), 405 - 70);
        // Decode it back with an independent reading of the layout.
        let b = w.into_bytes();
        let mut r = BitReader::new(&b);
        assert_eq!(r.read_ubits(6).unwrap(), SVC_CREATESTRINGTABLE);
        assert_eq!(r.read_string(64).unwrap(), "downloadables");
        assert_eq!(r.read_ubits(5).unwrap(), 13);
        assert_eq!(r.read_ubits(14).unwrap(), 1);
        assert_eq!(r.read_varint32().unwrap(), 180);
        assert!(!r.read_bit().unwrap());
        assert!(!r.read_bit().unwrap());
        assert!(r.read_bit().unwrap()); // next index
        assert!(r.read_bit().unwrap()); // has string
        assert!(!r.read_bit().unwrap()); // no substring
        assert_eq!(r.read_string(64).unwrap(), "maps\\gm_construct.bsp");
        assert!(!r.read_bit().unwrap()); // no user data
    }

    #[test]
    fn fixed_and_variable_userdata() {
        let mut t = StringTable::new("modelprecache", 13, Some((1, 2)));
        t.push("", UserData::Fixed(0)).unwrap();
        t.push("maps/gm_construct.bsp", UserData::Fixed(3)).unwrap();
        let mut w = BitWriter::with_capacity(256);
        t.write_create(&mut w).unwrap();
        let mut v = StringTable::new("server_query_info", 2, None);
        v.push(
            "QueryPort",
            UserData::Bytes(27015u32.to_le_bytes().to_vec()),
        )
        .unwrap();
        v.write_create(&mut w).unwrap();
        v.push("x", UserData::None).unwrap();
        v.write_update(9, 1, &mut w).unwrap();
        // mismatch is rejected
        let mut bad = StringTable::new("t", 2, None);
        bad.push("a", UserData::Fixed(1)).unwrap();
        assert!(bad.write_create(&mut w).is_err());
        let mut full = StringTable::new("t", 1, None);
        full.push("a", UserData::None).unwrap();
        full.push("b", UserData::None).unwrap();
        assert!(full.push("c", UserData::None).is_err());
    }
}
