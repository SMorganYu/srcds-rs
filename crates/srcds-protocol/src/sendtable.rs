//! Server-defined send tables and entity property encoding.
//!
//! srcds-rs describes its own networked classes to the client (`svc_SendTable` for each
//! table, then `svc_ClassInfo` listing the classes) instead of asking the client to use
//! the tables of its local game DLL. The client links every table and property to its
//! own receive tables *by name* and decodes values with the encoding the server
//! declared, so the server only has to name tables/properties that exist on the client
//! and pick matching value types; bit widths and flags are the server's choice.
//!
//! Wire facts used here (see `docs/PROTOCOL.md`):
//! * table description = name, 10-bit prop count, per prop: 5-bit type, name, 16-bit
//!   flags, then a sub-table name (datatable props) or low/high floats + 7-bit width;
//!   [факт] the client's own demo data tables decode bit-exactly with these widths;
//! * property order on the wire = the engine's flattening of the table hierarchy
//!   (collapsible base classes inline, other sub-tables first, then
//!   `CHANGES_OFTEN` props moved to the front); [факт] reproduced indices of
//!   reference-server traffic;
//! * property index = 1 "more" bit + variable-length uint of `(index - last - 1)`,
//!   list terminated by a 0 bit; [факт] from reference traffic.

use crate::bitbuf::BitWriter;
use crate::error::{ProtocolError, Result};

/// Property value types (`SendPropType`).
pub const DPT_INT: u32 = 0;
pub const DPT_FLOAT: u32 = 1;
pub const DPT_VECTOR: u32 = 2;
pub const DPT_VECTORXY: u32 = 3;
pub const DPT_STRING: u32 = 4;
pub const DPT_ARRAY: u32 = 5;
pub const DPT_DATATABLE: u32 = 6;

/// Property flags (`SPROP_*`) that srcds-rs uses or must recognise.
pub const SPROP_UNSIGNED: u32 = 1 << 0;
pub const SPROP_COORD: u32 = 1 << 1;
pub const SPROP_NOSCALE: u32 = 1 << 2;
pub const SPROP_EXCLUDE: u32 = 1 << 6;
pub const SPROP_INSIDEARRAY: u32 = 1 << 8;
pub const SPROP_CHANGES_OFTEN: u32 = 1 << 10;
pub const SPROP_COLLAPSIBLE: u32 = 1 << 12;

const PROPINFOBITS_NUMPROPS: u32 = 10;
const PROPINFOBITS_TYPE: u32 = 5;
const PROPINFOBITS_FLAGS: u32 = 16;
const PROPINFOBITS_NUMBITS: u32 = 7;
const PROPINFOBITS_NUMELEMENTS: u32 = 10;
/// String length prefix width (`DT_MAX_STRING_BITS`).
pub const STRING_LENGTH_BITS: u32 = 9;
/// Entity serial number width in an enter-PVS header.
pub const SERIAL_NUMBER_BITS: u32 = 10;

use crate::netmsg::{SVC_CLASSINFO, SVC_SENDTABLE};

/// How one property is encoded.
#[derive(Debug, Clone, PartialEq)]
pub enum PropKind {
    /// Integer of `bits` bits (signed unless the prop has `SPROP_UNSIGNED`).
    Int { bits: u32 },
    /// Float: raw 32-bit when `SPROP_NOSCALE`, otherwise quantised to `bits` in
    /// `[low, high]`.
    Float { bits: u32, low: f32, high: f32 },
    /// Three floats with the same encoding as [`PropKind::Float`].
    Vector { bits: u32, low: f32, high: f32 },
    /// Length-prefixed string (up to 511 bytes).
    String,
    /// Reference to another table by name.
    DataTable(&'static str),
    /// Fixed array of `count` integers of `bits` bits. On the wire the table lists the
    /// element (flagged `SPROP_INSIDEARRAY`) right before the array property.
    IntArray { bits: u32, count: u32 },
    /// A double, as GMod networks time values ([факт] reference server.dll: a
    /// `DPT_VectorXY` with `SPROP_NOSCALE` whose proxy copies the double's low and
    /// high 32-bit words into the two components).
    Double,
}

impl PropKind {
    fn type_id(&self) -> u32 {
        match self {
            PropKind::Int { .. } => DPT_INT,
            PropKind::Float { .. } => DPT_FLOAT,
            PropKind::Vector { .. } => DPT_VECTOR,
            PropKind::Double => DPT_VECTORXY,
            PropKind::String => DPT_STRING,
            PropKind::DataTable(_) => DPT_DATATABLE,
            PropKind::IntArray { .. } => DPT_ARRAY,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SendProp {
    pub name: &'static str,
    pub kind: PropKind,
    pub flags: u32,
}

impl SendProp {
    pub fn int(name: &'static str, bits: u32, unsigned: bool) -> Self {
        Self {
            name,
            kind: PropKind::Int { bits },
            flags: if unsigned { SPROP_UNSIGNED } else { 0 },
        }
    }
    /// Raw 32-bit float.
    pub fn float(name: &'static str) -> Self {
        Self {
            name,
            kind: PropKind::Float {
                bits: 32,
                low: 0.0,
                high: 0.0,
            },
            flags: SPROP_NOSCALE,
        }
    }
    /// A GMod double (time values).
    pub fn double(name: &'static str) -> Self {
        Self {
            name,
            kind: PropKind::Double,
            flags: SPROP_NOSCALE,
        }
    }
    /// Three raw 32-bit floats.
    pub fn vector(name: &'static str) -> Self {
        Self {
            name,
            kind: PropKind::Vector {
                bits: 32,
                low: 0.0,
                high: 0.0,
            },
            flags: SPROP_NOSCALE,
        }
    }
    /// Array of `count` integers (`unsigned` elements unless false).
    pub fn int_array(name: &'static str, bits: u32, count: u32, unsigned: bool) -> Self {
        Self {
            name,
            kind: PropKind::IntArray { bits, count },
            flags: if unsigned { SPROP_UNSIGNED } else { 0 },
        }
    }

    pub fn string(name: &'static str) -> Self {
        Self {
            name,
            kind: PropKind::String,
            flags: 0,
        }
    }
    /// Base-class link (inlined when flattening).
    pub fn baseclass(table: &'static str) -> Self {
        Self {
            name: "baseclass",
            kind: PropKind::DataTable(table),
            flags: SPROP_COLLAPSIBLE,
        }
    }
    /// Member sub-table (flattened as its own node).
    pub fn table(name: &'static str, table: &'static str) -> Self {
        Self {
            name,
            kind: PropKind::DataTable(table),
            flags: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SendTable {
    pub name: &'static str,
    pub props: Vec<SendProp>,
}

/// A networked class: network name and its top-level table.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerClass {
    pub name: &'static str,
    pub table: &'static str,
}

/// All tables known to the server, looked up by name.
#[derive(Debug, Clone, Default)]
pub struct TableSet {
    pub tables: Vec<SendTable>,
}

impl TableSet {
    pub fn get(&self, name: &str) -> Result<&SendTable> {
        self.tables
            .iter()
            .find(|t| t.name == name)
            .ok_or(ProtocolError::InvalidField {
                field: "sendtable",
                reason: "referenced table not defined",
            })
    }
}

/// One property in a class's flattened (wire) order.
#[derive(Debug, Clone, PartialEq)]
pub struct FlatProp {
    /// Table that declares the property.
    pub table: &'static str,
    pub name: &'static str,
    pub kind: PropKind,
    pub flags: u32,
}

/// Flattens `root` the way the engine orders properties on the wire: walking each
/// table, collapsible sub-tables (base classes) contribute their properties inline to
/// the current node, other sub-tables become child nodes whose properties are emitted
/// as soon as they are reached, and a node's own properties follow its children.
/// Finally properties flagged `SPROP_CHANGES_OFTEN` are moved to the front.
pub fn flatten(set: &TableSet, root: &str) -> Result<Vec<FlatProp>> {
    fn iterate(
        set: &TableSet,
        table: &SendTable,
        own: &mut Vec<FlatProp>,
        out: &mut Vec<FlatProp>,
        depth: u32,
    ) -> Result<()> {
        if depth > 32 {
            return Err(ProtocolError::InvalidField {
                field: "sendtable",
                reason: "table nesting too deep",
            });
        }
        for p in &table.props {
            if p.flags & (SPROP_EXCLUDE | SPROP_INSIDEARRAY) != 0 {
                continue;
            }
            match &p.kind {
                PropKind::DataTable(sub) => {
                    let sub = set.get(sub)?;
                    if p.flags & SPROP_COLLAPSIBLE != 0 {
                        iterate(set, sub, own, out, depth + 1)?;
                    } else {
                        build(set, sub, out, depth + 1)?;
                    }
                }
                kind => own.push(FlatProp {
                    table: table.name,
                    name: p.name,
                    kind: kind.clone(),
                    flags: p.flags,
                }),
            }
        }
        Ok(())
    }
    fn build(set: &TableSet, table: &SendTable, out: &mut Vec<FlatProp>, depth: u32) -> Result<()> {
        let mut own = Vec::new();
        iterate(set, table, &mut own, out, depth)?;
        out.append(&mut own);
        Ok(())
    }
    let mut out = Vec::new();
    build(set, set.get(root)?, &mut out, 0)?;
    // Priority pass: repeatedly take the first CHANGES_OFTEN prop at or after `start`
    // and swap it into `start` (the engine's exact, non-stable selection order).
    let mut start = 0;
    while let Some(i) = (start..out.len()).find(|&i| out[i].flags & SPROP_CHANGES_OFTEN != 0) {
        out.swap(i, start);
        start += 1;
    }
    Ok(out)
}

/// Appends `svc_SendTable` describing `table`.
pub fn write_send_table(w: &mut BitWriter, table: &SendTable, needs_decoder: bool) -> Result<()> {
    // The description length is a 16-bit bit count, so it never exceeds 8 KiB.
    let mut d = BitWriter::with_capacity(8192);
    d.write_string(table.name)?;
    // arrays are described by two entries: the element template, then the array
    let entries = table.props.len()
        + table
            .props
            .iter()
            .filter(|p| matches!(p.kind, PropKind::IntArray { .. }))
            .count();
    if entries >= 1 << PROPINFOBITS_NUMPROPS {
        return Err(ProtocolError::InvalidField {
            field: "sendtable",
            reason: "too many props",
        });
    }
    d.write_ubits(entries as u32, PROPINFOBITS_NUMPROPS)?;
    for p in &table.props {
        if let PropKind::IntArray { bits, count } = &p.kind {
            d.write_ubits(DPT_INT, PROPINFOBITS_TYPE)?;
            d.write_string(p.name)?;
            d.write_ubits((p.flags | SPROP_INSIDEARRAY) & 0xFFFF, PROPINFOBITS_FLAGS)?;
            d.write_f32(0.0)?;
            d.write_f32(0.0)?;
            d.write_ubits(*bits, PROPINFOBITS_NUMBITS)?;
            d.write_ubits(DPT_ARRAY, PROPINFOBITS_TYPE)?;
            d.write_string(p.name)?;
            d.write_ubits(0, PROPINFOBITS_FLAGS)?;
            d.write_ubits(*count, PROPINFOBITS_NUMELEMENTS)?;
            continue;
        }
        d.write_ubits(p.kind.type_id(), PROPINFOBITS_TYPE)?;
        d.write_string(p.name)?;
        d.write_ubits(p.flags & 0xFFFF, PROPINFOBITS_FLAGS)?;
        match &p.kind {
            PropKind::DataTable(sub) => d.write_string(sub)?,
            PropKind::Int { bits } => {
                d.write_f32(0.0)?;
                d.write_f32(0.0)?;
                d.write_ubits(*bits, PROPINFOBITS_NUMBITS)?;
            }
            PropKind::Float { bits, low, high } | PropKind::Vector { bits, low, high } => {
                d.write_f32(*low)?;
                d.write_f32(*high)?;
                d.write_ubits(*bits, PROPINFOBITS_NUMBITS)?;
            }
            PropKind::String => {
                d.write_f32(0.0)?;
                d.write_f32(0.0)?;
                d.write_ubits(0, PROPINFOBITS_NUMBITS)?;
            }
            PropKind::Double => {
                d.write_f32(0.0)?;
                d.write_f32(0.0)?;
                d.write_ubits(32, PROPINFOBITS_NUMBITS)?;
            }
            PropKind::IntArray { .. } => unreachable!("handled above"),
        }
    }
    if d.bit_len() > u16::MAX as usize {
        return Err(ProtocolError::InvalidField {
            field: "sendtable",
            reason: "description too long",
        });
    }
    w.write_ubits(SVC_SENDTABLE, 6)?;
    w.write_bit(needs_decoder)?;
    w.write_u16(d.bit_len() as u16)?;
    w.append(&d)
}

/// Number of bits used for class ids when `n` classes exist (`log2(n) + 1`).
pub fn class_bits(n: usize) -> u32 {
    usize::BITS - n.max(1).leading_zeros()
}

/// Appends `svc_ClassInfo` listing `classes` (class id = position).
pub fn write_class_info(w: &mut BitWriter, classes: &[ServerClass]) -> Result<()> {
    if classes.is_empty() || classes.len() > i16::MAX as usize {
        return Err(ProtocolError::InvalidField {
            field: "classinfo",
            reason: "bad class count",
        });
    }
    w.write_ubits(SVC_CLASSINFO, 6)?;
    w.write_u16(classes.len() as u16)?;
    w.write_bit(false)?;
    let bits = class_bits(classes.len());
    for (i, c) in classes.iter().enumerate() {
        w.write_ubits(i as u32, bits)?;
        w.write_string(c.name)?;
        w.write_string(c.table)?;
    }
    Ok(())
}

/// Writes a variable-length unsigned integer: 2-bit selector, then 4/8/12/32 bits.
pub fn write_ubitvar(w: &mut BitWriter, v: u32) -> Result<()> {
    let (sel, bits) = match v {
        0..=0xF => (0, 4),
        0x10..=0xFF => (1, 8),
        0x100..=0xFFF => (2, 12),
        _ => (3, 32),
    };
    w.write_ubits(sel, 2)?;
    w.write_ubits(v, bits)
}

/// A property value.
#[derive(Debug, Clone, PartialEq)]
pub enum PropValue {
    Int(i32),
    Float(f32),
    Vector([f32; 3]),
    String(String),
    IntArray(Vec<i32>),
    Double(f64),
}

impl PropValue {
    /// Whether this is the zero/empty value (what an empty baseline decodes to).
    pub fn is_zero(&self) -> bool {
        match self {
            PropValue::Int(v) => *v == 0,
            PropValue::Float(v) => *v == 0.0,
            PropValue::Vector(v) => v.iter().all(|c| *c == 0.0),
            PropValue::String(s) => s.is_empty(),
            PropValue::IntArray(v) => v.iter().all(|x| *x == 0),
            PropValue::Double(v) => *v == 0.0,
        }
    }

    /// Zero/empty value of the given kind (what a client assumes before any update).
    pub fn default_for(kind: &PropKind) -> PropValue {
        match kind {
            PropKind::Int { .. } | PropKind::DataTable(_) => PropValue::Int(0),
            PropKind::Float { .. } => PropValue::Float(0.0),
            PropKind::Vector { .. } => PropValue::Vector([0.0; 3]),
            PropKind::String => PropValue::String(String::new()),
            PropKind::IntArray { count, .. } => PropValue::IntArray(vec![0; *count as usize]),
            PropKind::Double => PropValue::Double(0.0),
        }
    }
}

fn write_float(
    w: &mut BitWriter,
    v: f32,
    bits: u32,
    low: f32,
    high: f32,
    flags: u32,
) -> Result<()> {
    if flags & SPROP_NOSCALE != 0 {
        return w.write_f32(v);
    }
    if flags & SPROP_COORD != 0 || bits == 0 || bits > 32 || high <= low {
        return Err(ProtocolError::InvalidField {
            field: "sendprop",
            reason: "unsupported float encoding",
        });
    }
    let max = if bits == 32 {
        u32::MAX as f64
    } else {
        ((1u64 << bits) - 1) as f64
    };
    let t = ((v.clamp(low, high) - low) as f64 / (high - low) as f64 * max).round();
    w.write_ubits(t as u32, bits)
}

/// Encodes one property value with the property's declared encoding.
pub fn write_value(w: &mut BitWriter, p: &FlatProp, v: &PropValue) -> Result<()> {
    let mismatch = ProtocolError::InvalidField {
        field: "sendprop",
        reason: "value does not match property type",
    };
    match (&p.kind, v) {
        (PropKind::Int { bits }, PropValue::Int(x)) => {
            if p.flags & SPROP_UNSIGNED != 0 {
                let mask = if *bits >= 32 {
                    u32::MAX
                } else {
                    (1u32 << bits) - 1
                };
                w.write_ubits(*x as u32 & mask, *bits)
            } else {
                w.write_sbits(*x, *bits)
            }
        }
        (PropKind::Float { bits, low, high }, PropValue::Float(x)) => {
            write_float(w, *x, *bits, *low, *high, p.flags)
        }
        (PropKind::Vector { bits, low, high }, PropValue::Vector(v)) => {
            for c in v {
                write_float(w, *c, *bits, *low, *high, p.flags)?;
            }
            Ok(())
        }
        (PropKind::Double, PropValue::Double(v)) => {
            let bits = v.to_bits();
            w.write_ubits(bits as u32, 32)?;
            w.write_ubits((bits >> 32) as u32, 32)
        }
        (PropKind::String, PropValue::String(s)) => {
            let b = s.as_bytes();
            if b.len() >= 1 << STRING_LENGTH_BITS {
                return Err(ProtocolError::InvalidField {
                    field: "sendprop",
                    reason: "string too long",
                });
            }
            w.write_ubits(b.len() as u32, STRING_LENGTH_BITS)?;
            w.write_bytes(b)
        }
        (PropKind::IntArray { bits, count }, PropValue::IntArray(v)) => {
            if v.len() > *count as usize {
                return Err(mismatch);
            }
            // element count width: floor(log2(count)) + 1
            let count_bits = u32::BITS - count.max(&1).leading_zeros();
            w.write_ubits(v.len() as u32, count_bits)?;
            for x in v {
                if p.flags & SPROP_UNSIGNED != 0 {
                    let mask = if *bits >= 32 { u32::MAX } else { (1u32 << bits) - 1 };
                    w.write_ubits(*x as u32 & mask, *bits)?;
                } else {
                    w.write_sbits(*x, *bits)?;
                }
            }
            Ok(())
        }
        _ => Err(mismatch),
    }
}

/// Writes a property list: `(flat index, value)` pairs in ascending index order.
pub fn write_prop_list(
    w: &mut BitWriter,
    flat: &[FlatProp],
    props: &[(usize, &PropValue)],
) -> Result<()> {
    let mut last: i64 = -1;
    for (idx, v) in props {
        let i = *idx as i64;
        if i <= last || *idx >= flat.len() {
            return Err(ProtocolError::InvalidField {
                field: "proplist",
                reason: "indices must be ascending and in range",
            });
        }
        w.write_bit(true)?;
        write_ubitvar(w, (i - last - 1) as u32)?;
        write_value(w, &flat[*idx], v)?;
        last = i;
    }
    w.write_bit(false)
}

/// One entry of a `svc_PacketEntities` payload.
#[derive(Debug, Clone)]
pub enum EntityUpdate<'a> {
    /// Entity (re)enters the client's view: class, serial and properties relative to
    /// the class baseline.
    Enter {
        index: u32,
        class_id: u32,
        serial: u32,
        props: Vec<(usize, &'a PropValue)>,
    },
    /// Changed properties of an entity the client already has.
    Delta {
        index: u32,
        props: Vec<(usize, &'a PropValue)>,
    },
    /// Entity leaves the client's view; `delete` also destroys it client-side.
    Leave { index: u32, delete: bool },
}

impl EntityUpdate<'_> {
    pub fn index(&self) -> u32 {
        match self {
            EntityUpdate::Enter { index, .. }
            | EntityUpdate::Delta { index, .. }
            | EntityUpdate::Leave { index, .. } => *index,
        }
    }
}

/// Writes the entity list of `svc_PacketEntities`. `updates` must be in ascending
/// entity order; `flat_of(class_id)` gives the flattened props of a class and
/// `class_of(index)` the class of an already-known entity (for deltas).
pub fn write_entity_updates<'f>(
    w: &mut BitWriter,
    updates: &[EntityUpdate<'_>],
    class_count: usize,
    is_delta: bool,
    flat_of: impl Fn(u32) -> Option<&'f [FlatProp]>,
    class_of: impl Fn(u32) -> Option<u32>,
) -> Result<()> {
    let cbits = class_bits(class_count);
    let mut base: i64 = -1;
    let bad = |reason| ProtocolError::InvalidField {
        field: "packetentities",
        reason,
    };
    for u in updates {
        let idx = u.index() as i64;
        if idx <= base || idx >= 1 << crate::netmsg::MAX_EDICT_BITS {
            return Err(bad("entity indices must be ascending and in range"));
        }
        write_ubitvar(w, (idx - base - 1) as u32)?;
        base = idx;
        match u {
            EntityUpdate::Leave { delete, .. } => {
                w.write_bit(true)?;
                w.write_bit(*delete)?;
            }
            EntityUpdate::Enter {
                class_id,
                serial,
                props,
                ..
            } => {
                w.write_bit(false)?;
                w.write_bit(true)?;
                if *class_id as usize >= class_count {
                    return Err(bad("class id out of range"));
                }
                w.write_ubits(*class_id, cbits)?;
                w.write_ubits(
                    *serial & ((1 << SERIAL_NUMBER_BITS) - 1),
                    SERIAL_NUMBER_BITS,
                )?;
                let flat = flat_of(*class_id).ok_or(bad("unknown class"))?;
                write_prop_list(w, flat, props)?;
            }
            EntityUpdate::Delta { index, props } => {
                w.write_bit(false)?;
                w.write_bit(false)?;
                let class = class_of(*index).ok_or(bad("delta for unknown entity"))?;
                let flat = flat_of(class).ok_or(bad("unknown class"))?;
                write_prop_list(w, flat, props)?;
            }
        }
    }
    if is_delta {
        // No explicit deletions list (removed entities are sent as Leave+delete).
        w.write_bit(false)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitbuf::BitReader;

    fn set() -> TableSet {
        TableSet {
            tables: vec![
                SendTable {
                    name: "DT_Leaf",
                    props: vec![
                        SendProp::baseclass("DT_Base"),
                        SendProp::int("leaf_a", 4, true),
                        SendProp::table("local", "DT_Local"),
                        SendProp {
                            flags: SPROP_CHANGES_OFTEN | SPROP_UNSIGNED,
                            ..SendProp::int("leaf_often", 8, true)
                        },
                    ],
                },
                SendTable {
                    name: "DT_Base",
                    props: vec![SendProp::int("base_a", 8, true), SendProp::vector("base_v")],
                },
                SendTable {
                    name: "DT_Local",
                    props: vec![SendProp::float("local_f")],
                },
            ],
        }
    }

    #[test]
    fn flatten_matches_engine_order() {
        let names: Vec<_> = flatten(&set(), "DT_Leaf")
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        // child node (local) first, then the node's own props with the base class
        // inlined in declaration position; CHANGES_OFTEN swapped to the front.
        assert_eq!(
            names,
            ["leaf_often", "base_a", "base_v", "leaf_a", "local_f"]
        );
    }

    #[test]
    fn ubitvar_widths() {
        for (v, bits) in [
            (0u32, 6usize),
            (15, 6),
            (16, 10),
            (255, 10),
            (256, 14),
            (4096, 34),
        ] {
            let mut w = BitWriter::with_capacity(16);
            write_ubitvar(&mut w, v).unwrap();
            assert_eq!(w.bit_len(), bits, "v={v}");
            let b = w.into_bytes();
            let mut r = BitReader::new(&b);
            let sel = r.read_ubits(2).unwrap();
            let got = r.read_ubits([4, 8, 12, 32][sel as usize]).unwrap();
            assert_eq!(got, v);
        }
    }

    #[test]
    fn send_table_layout() {
        let s = set();
        let mut w = BitWriter::with_capacity(256);
        write_send_table(&mut w, s.get("DT_Base").unwrap(), true).unwrap();
        let b = w.into_bytes();
        let mut r = BitReader::new(&b);
        assert_eq!(r.read_ubits(6).unwrap(), SVC_SENDTABLE);
        assert!(r.read_bit().unwrap());
        let len = r.read_u16().unwrap() as usize;
        let start = r.bit_position();
        assert_eq!(r.read_string(64).unwrap(), "DT_Base");
        assert_eq!(r.read_ubits(10).unwrap(), 2);
        assert_eq!(r.read_ubits(5).unwrap(), DPT_INT);
        assert_eq!(r.read_string(64).unwrap(), "base_a");
        assert_eq!(r.read_ubits(16).unwrap(), SPROP_UNSIGNED);
        r.read_f32().unwrap();
        r.read_f32().unwrap();
        assert_eq!(r.read_ubits(7).unwrap(), 8);
        assert_eq!(r.read_ubits(5).unwrap(), DPT_VECTOR);
        assert_eq!(r.read_string(64).unwrap(), "base_v");
        assert_eq!(r.read_ubits(16).unwrap(), SPROP_NOSCALE);
        r.read_f32().unwrap();
        r.read_f32().unwrap();
        r.read_ubits(7).unwrap();
        assert_eq!(r.bit_position() - start, len);
    }

    #[test]
    fn entity_list_roundtrip_shape() {
        let s = set();
        let flat = flatten(&s, "DT_Leaf").unwrap();
        let v1 = PropValue::Int(5);
        let v2 = PropValue::Vector([1.0, 2.0, 3.0]);
        let ups = [
            EntityUpdate::Enter {
                index: 0,
                class_id: 0,
                serial: 7,
                props: vec![(1, &v1), (2, &v2)],
            },
            EntityUpdate::Leave {
                index: 20,
                delete: true,
            },
        ];
        let mut w = BitWriter::with_capacity(64);
        write_entity_updates(&mut w, &ups, 1, true, |_| Some(&flat[..]), |_| Some(0)).unwrap();
        let b = w.into_bytes();
        let mut r = BitReader::new(&b);
        // ent 0: offset 0, leave=0 enter=1, class (1 bit), serial
        assert_eq!(r.read_ubits(2).unwrap(), 0);
        assert_eq!(r.read_ubits(4).unwrap(), 0);
        assert!(!r.read_bit().unwrap());
        assert!(r.read_bit().unwrap());
        assert_eq!(r.read_ubits(1).unwrap(), 0);
        assert_eq!(r.read_ubits(10).unwrap(), 7);
        // prop 1 (delta 1 → var 1), u8 5; prop 2 (var 0), 3 floats
        assert!(r.read_bit().unwrap());
        assert_eq!(r.read_ubits(2).unwrap(), 0);
        assert_eq!(r.read_ubits(4).unwrap(), 1);
        assert_eq!(r.read_ubits(8).unwrap(), 5);
        assert!(r.read_bit().unwrap());
        assert_eq!(r.read_ubits(6).unwrap(), 0);
        assert_eq!(r.read_f32().unwrap(), 1.0);
        r.read_f32().unwrap();
        r.read_f32().unwrap();
        assert!(!r.read_bit().unwrap());
        // ent 20: offset 19 (8-bit form), leave + delete
        assert_eq!(r.read_ubits(2).unwrap(), 1);
        assert_eq!(r.read_ubits(8).unwrap(), 19);
        assert!(r.read_bit().unwrap());
        assert!(r.read_bit().unwrap());
        // end of deletions
        assert!(!r.read_bit().unwrap());
    }

    #[test]
    fn class_bits_like_engine() {
        assert_eq!(class_bits(1), 1);
        assert_eq!(class_bits(2), 2);
        assert_eq!(class_bits(3), 2);
        assert_eq!(class_bits(250), 8);
        assert_eq!(class_bits(256), 9);
    }

    #[test]
    fn quantised_float() {
        let p = FlatProp {
            table: "t",
            name: "f",
            kind: PropKind::Float {
                bits: 8,
                low: 0.0,
                high: 255.0,
            },
            flags: 0,
        };
        let mut w = BitWriter::with_capacity(8);
        write_value(&mut w, &p, &PropValue::Float(100.0)).unwrap();
        assert_eq!(w.as_bytes()[0], 100);
        assert!(write_value(&mut w, &p, &PropValue::Int(1)).is_err());
    }
}
