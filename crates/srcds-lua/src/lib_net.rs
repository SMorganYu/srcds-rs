//! The `net` library (server side).
//!
//! Wire facts (reference server, `docs/PROTOCOL.md`): a net message is a GMod packet
//! whose first byte is type 0, followed by the 16-bit `networkstring` id and the
//! body. Server → client it travels in `svc_GMod_ServerToClient`, client → server in
//! `clc_GMod_ClientToServer`; the server hands incoming bodies to Lua's
//! `net.Incoming(len, ply)` with `len` = bits after the type byte.
//!
//! Encodings: integers LSB-first; `WriteVector` / `WriteAngle` = three presence flags
//! then a coordinate per non-zero component (integer flag, fraction flag, sign,
//! integer − 1 in 14 bits, fraction in 5 bits); `WriteNormal` = x/y presence flags,
//! sign + 11-bit magnitude for each, z sign; strings are NUL-terminated bytes;
//! floats/doubles are raw IEEE bits.

use mlua::{Lua, MultiValue, Table, Value};

use crate::entity::{store, EntRef, EntUd};
use crate::vecang::{Angle, Vector};

/// LSB-first bit writer.
#[derive(Debug, Clone, Default)]
pub struct Bits {
    pub data: Vec<u8>,
    pub bits: usize,
}

const MAX_MESSAGE_BYTES: usize = 64 * 1024;

impl Bits {
    pub fn write(&mut self, v: u64, n: u32) -> bool {
        if self.bits + n as usize > MAX_MESSAGE_BYTES * 8 {
            return false;
        }
        for i in 0..n {
            let byte = self.bits / 8;
            if byte == self.data.len() {
                self.data.push(0);
            }
            if v >> i & 1 == 1 {
                self.data[byte] |= 1 << (self.bits % 8);
            }
            self.bits += 1;
        }
        true
    }

    fn coord(&mut self, f: f32) {
        let int = f.abs() as u32;
        let fract = ((f * 32.0) as i32).unsigned_abs() & 31;
        self.write(u64::from(int != 0), 1);
        self.write(u64::from(fract != 0), 1);
        if int != 0 || fract != 0 {
            self.write(u64::from(f <= -0.03125), 1);
            if int != 0 {
                self.write(u64::from(int - 1), 14);
            }
            if fract != 0 {
                self.write(u64::from(fract), 5);
            }
        }
    }

    pub fn vec3_coord(&mut self, v: [f32; 3]) {
        let has: Vec<bool> = v.iter().map(|c| *c >= 0.03125 || *c <= -0.03125).collect();
        for h in &has {
            self.write(u64::from(*h), 1);
        }
        for (c, h) in v.iter().zip(has) {
            if h {
                self.coord(*c);
            }
        }
    }

    fn normal_component(&mut self, f: f32) {
        let m = (((f * 2047.0) as i32).unsigned_abs()).min(2047);
        self.write(u64::from(f <= -0.000_488_52), 1);
        self.write(u64::from(m), 11);
    }

    pub fn vec3_normal(&mut self, v: [f32; 3]) {
        let hx = v[0] >= 0.000_488_52 || v[0] <= -0.000_488_52;
        let hy = v[1] >= 0.000_488_52 || v[1] <= -0.000_488_52;
        self.write(u64::from(hx), 1);
        self.write(u64::from(hy), 1);
        if hx {
            self.normal_component(v[0]);
        }
        if hy {
            self.normal_component(v[1]);
        }
        self.write(u64::from(v[2] <= -0.000_488_52), 1);
    }
}

/// LSB-first bit reader over an incoming message.
#[derive(Debug, Clone, Default)]
pub struct Reader {
    data: Vec<u8>,
    bits: usize,
    pos: usize,
}

impl Reader {
    pub fn new(data: Vec<u8>, bits: usize) -> Self {
        Self { bits: bits.min(data.len() * 8), data, pos: 0 }
    }
    pub fn left(&self) -> usize {
        self.bits - self.pos
    }
    pub fn read(&mut self, n: u32) -> u64 {
        let mut v = 0u64;
        for i in 0..n {
            if self.pos >= self.bits {
                return v;
            }
            if self.data[self.pos / 8] >> (self.pos % 8) & 1 == 1 {
                v |= 1 << i;
            }
            self.pos += 1;
        }
        v
    }
    fn coord(&mut self) -> f32 {
        let has_int = self.read(1) == 1;
        let has_fract = self.read(1) == 1;
        if !has_int && !has_fract {
            return 0.0;
        }
        let neg = self.read(1) == 1;
        let int = if has_int { self.read(14) as f32 + 1.0 } else { 0.0 };
        let fract = if has_fract { self.read(5) as f32 / 32.0 } else { 0.0 };
        let v = int + fract;
        if neg {
            -v
        } else {
            v
        }
    }
    pub fn vec3_coord(&mut self) -> [f32; 3] {
        let has = [self.read(1) == 1, self.read(1) == 1, self.read(1) == 1];
        let mut v = [0.0; 3];
        for k in 0..3 {
            if has[k] {
                v[k] = self.coord();
            }
        }
        v
    }
    fn normal_component(&mut self) -> f32 {
        let neg = self.read(1) == 1;
        let m = self.read(11) as f32 / 2047.0;
        if neg {
            -m
        } else {
            m
        }
    }
    pub fn vec3_normal(&mut self) -> [f32; 3] {
        let hx = self.read(1) == 1;
        let hy = self.read(1) == 1;
        let x = if hx { self.normal_component() } else { 0.0 };
        let y = if hy { self.normal_component() } else { 0.0 };
        let neg_z = self.read(1) == 1;
        let z2 = 1.0 - x * x - y * y;
        let z = if z2 > 0.0 { z2.sqrt() } else { 0.0 };
        [x, y, if neg_z { -z } else { z }]
    }
}

/// Who receives a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Targets {
    All,
    Only(Vec<EntRef>),
    AllExcept(Vec<EntRef>),
}

/// A finished outgoing GMod packet (type byte included).
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub targets: Targets,
    pub reliable: bool,
    pub data: Vec<u8>,
    pub bits: usize,
}

#[derive(Default)]
pub struct NetState {
    current: Option<(Bits, bool, String)>,
    pub incoming: Option<Reader>,
    pub queue: Vec<Outgoing>,
}

fn net(lua: &Lua) -> mlua::Result<mlua::AppDataRefMut<'_, NetState>> {
    lua.app_data_mut::<NetState>()
        .ok_or_else(|| mlua::Error::RuntimeError("net busy".into()))
}

fn write(lua: &Lua, f: impl FnOnce(&mut Bits)) -> mlua::Result<()> {
    let mut n = net(lua)?;
    if let Some((b, _, _)) = n.current.as_mut() {
        f(b);
    }
    Ok(())
}

fn read<R>(lua: &Lua, d: R, f: impl FnOnce(&mut Reader) -> R) -> mlua::Result<R> {
    let mut n = net(lua)?;
    Ok(match n.incoming.as_mut() {
        Some(r) => f(r),
        None => d,
    })
}

fn targets_of(v: &Value) -> Vec<EntRef> {
    match v {
        Value::UserData(u) => u.borrow::<EntUd>().map(|e| vec![e.0]).unwrap_or_default(),
        Value::Table(t) => t
            .clone()
            .sequence_values::<Value>()
            .flatten()
            .filter_map(|v| match v {
                Value::UserData(u) => u.borrow::<EntUd>().ok().map(|e| e.0),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn finish(lua: &Lua, targets: Targets) -> mlua::Result<()> {
    let mut n = net(lua)?;
    if let Some((b, reliable, _)) = n.current.take() {
        n.queue.push(Outgoing {
            targets,
            reliable,
            data: b.data,
            bits: b.bits,
        });
    }
    Ok(())
}

pub fn install(lua: &Lua) -> mlua::Result<()> {
    lua.set_app_data(NetState::default());
    let t: Table = match lua.globals().raw_get::<Value>("net")? {
        Value::Table(t) => t,
        _ => {
            let t = lua.create_table()?;
            lua.globals().raw_set("net", &t)?;
            t
        }
    };
    t.raw_set(
        "Start",
        lua.create_function(|lua, (name, unreliable): (String, Option<bool>)| {
            let id = {
                let e = lua.app_data_ref::<crate::lib_misc::Engine>().unwrap();
                e.network_strings.iter().position(|n| n.eq_ignore_ascii_case(&name)).map(|i| i + 1)
            };
            let Some(id) = id else {
                return Err(mlua::Error::RuntimeError(
                    "Calling net.Start with unpooled message name! (Did you forget to call util.AddNetworkString serverside?)".into(),
                ));
            };
            let mut b = Bits::default();
            b.write(0, 8);
            b.write(id as u64, 16);
            net(lua)?.current = Some((b, !unreliable.unwrap_or(false), name));
            Ok(true)
        })?,
    )?;
    t.raw_set("Abort", lua.create_function(|lua, ()| {
        net(lua)?.current = None;
        Ok(())
    })?)?;
    t.raw_set(
        "BytesWritten",
        lua.create_function(|lua, ()| {
            let n = net(lua)?;
            Ok(n.current.as_ref().map_or((None, None), |(b, _, _)| (Some(b.data.len()), Some(b.bits))))
        })?,
    )?;
    t.raw_set("Send", lua.create_function(|lua, v: Value| finish(lua, Targets::Only(targets_of(&v))))?)?;
    t.raw_set("SendOmit", lua.create_function(|lua, v: Value| finish(lua, Targets::AllExcept(targets_of(&v))))?)?;
    t.raw_set("Broadcast", lua.create_function(|lua, ()| finish(lua, Targets::All))?)?;
    t.raw_set("SendPVS", lua.create_function(|lua, _v: Value| finish(lua, Targets::All))?)?;
    t.raw_set("SendPAS", lua.create_function(|lua, _v: Value| finish(lua, Targets::All))?)?;
    t.raw_set("SendToServer", lua.create_function(|lua, ()| {
        net(lua)?.current = None;
        Ok(())
    })?)?;

    t.raw_set("WriteUInt", lua.create_function(|lua, (v, n): (f64, u32)| write(lua, |b| { b.write(v as i64 as u64, n.min(32)); }))?)?;
    t.raw_set("WriteInt", lua.create_function(|lua, (v, n): (f64, u32)| write(lua, |b| { b.write(v as i64 as u64, n.min(32)); }))?)?;
    t.raw_set(
        "WriteUInt64",
        lua.create_function(|lua, v: Value| {
            let x = match &v {
                Value::String(s) => s.to_string_lossy().parse::<u64>().unwrap_or(0),
                Value::Integer(i) => *i as u64,
                Value::Number(n) => *n as u64,
                _ => 0,
            };
            write(lua, |b| { b.write(x, 64); })
        })?,
    )?;
    t.raw_set(
        "WriteBit",
        lua.create_function(|lua, v: Value| {
            let bit = match v {
                Value::Boolean(b) => b,
                Value::Integer(i) => i != 0,
                Value::Number(n) => n != 0.0,
                _ => false,
            };
            write(lua, |b| { b.write(u64::from(bit), 1); })
        })?,
    )?;
    t.raw_set("WriteBool", t.raw_get::<Value>("WriteBit")?)?;
    t.raw_set("WriteFloat", lua.create_function(|lua, v: f32| write(lua, |b| { b.write(u64::from(v.to_bits()), 32); }))?)?;
    t.raw_set("WriteDouble", lua.create_function(|lua, v: f64| write(lua, |b| { b.write(v.to_bits(), 64); }))?)?;
    t.raw_set(
        "WriteString",
        lua.create_function(|lua, s: mlua::String| {
            let bytes = s.as_bytes().to_vec();
            write(lua, |b| {
                for c in bytes.iter().take_while(|c| **c != 0) {
                    b.write(u64::from(*c), 8);
                }
                b.write(0, 8);
            })
        })?,
    )?;
    t.raw_set(
        "WriteData",
        lua.create_function(|lua, (s, len): (mlua::String, Option<usize>)| {
            let bytes = s.as_bytes().to_vec();
            let n = len.unwrap_or(bytes.len()).min(bytes.len());
            write(lua, |b| {
                for c in &bytes[..n] {
                    b.write(u64::from(*c), 8);
                }
            })
        })?,
    )?;
    t.raw_set("WriteVector", lua.create_function(|lua, v: Vector| write(lua, |b| b.vec3_coord(v.0)))?)?;
    t.raw_set("WriteAngle", lua.create_function(|lua, a: Angle| write(lua, |b| b.vec3_coord(a.0)))?)?;
    t.raw_set("WriteNormal", lua.create_function(|lua, v: Vector| write(lua, |b| b.vec3_normal(v.0)))?)?;
    t.raw_set(
        "WriteMatrix",
        lua.create_function(|lua, _m: Value| write(lua, |b| {
            // identity matrix as 16 floats (matrices are not modelled yet)
            for i in 0..16 {
                let v: f32 = if i % 5 == 0 { 1.0 } else { 0.0 };
                b.write(u64::from(v.to_bits()), 32);
            }
        }))?,
    )?;

    t.raw_set("ReadHeader", lua.create_function(|lua, ()| read(lua, 0, |r| r.read(16) as i64))?)?;
    t.raw_set(
        "ReadUInt",
        lua.create_function(|lua, n: u32| read(lua, 0.0, |r| r.read(n.min(32)) as f64))?,
    )?;
    t.raw_set(
        "ReadInt",
        lua.create_function(|lua, n: u32| {
            let n = n.clamp(1, 32);
            read(lua, 0.0, |r| {
                let v = r.read(n) as i64;
                let sign = 1i64 << (n - 1);
                ((v ^ sign) - sign) as f64
            })
        })?,
    )?;
    t.raw_set("ReadUInt64", lua.create_function(|lua, ()| read(lua, String::from("0"), |r| r.read(64).to_string()))?)?;
    t.raw_set("ReadBit", lua.create_function(|lua, ()| read(lua, 0, |r| r.read(1) as i64))?)?;
    t.raw_set("ReadBool", lua.create_function(|lua, ()| read(lua, false, |r| r.read(1) == 1))?)?;
    t.raw_set("ReadFloat", lua.create_function(|lua, ()| read(lua, 0.0, |r| f32::from_bits(r.read(32) as u32)))?)?;
    t.raw_set("ReadDouble", lua.create_function(|lua, ()| read(lua, 0.0, |r| f64::from_bits(r.read(64))))?)?;
    t.raw_set(
        "ReadString",
        lua.create_function(|lua, ()| {
            let bytes = read(lua, Vec::new(), |r| {
                let mut v = Vec::new();
                while r.left() >= 8 && v.len() < 65536 {
                    let c = r.read(8) as u8;
                    if c == 0 {
                        break;
                    }
                    v.push(c);
                }
                v
            })?;
            lua.create_string(&bytes)
        })?,
    )?;
    t.raw_set(
        "ReadData",
        lua.create_function(|lua, n: usize| {
            let bytes = read(lua, Vec::new(), |r| {
                let n = n.min(r.left() / 8);
                (0..n).map(|_| r.read(8) as u8).collect::<Vec<u8>>()
            })?;
            lua.create_string(&bytes)
        })?,
    )?;
    t.raw_set("ReadVector", lua.create_function(|lua, ()| read(lua, Vector([0.0; 3]), |r| Vector(r.vec3_coord())))?)?;
    t.raw_set("ReadAngle", lua.create_function(|lua, ()| read(lua, Angle([0.0; 3]), |r| Angle(r.vec3_coord())))?)?;
    t.raw_set("ReadNormal", lua.create_function(|lua, ()| read(lua, Vector([0.0; 3]), |r| Vector(r.vec3_normal())))?)?;
    t.raw_set("ReadMatrix", lua.create_function(|lua, ()| {
        read(lua, (), |r| {
            r.read(32 * 16);
        })
    })?)?;
    t.raw_set(
        "BytesLeft",
        lua.create_function(|lua, ()| read(lua, (0usize, 0usize), |r| (r.left().div_ceil(8), r.left())))?,
    )?;
    // BroadcastLua: GMod packet type 5 + NUL-terminated code (as Player:SendLua)
    lua.globals().raw_set(
        "BroadcastLua",
        lua.create_function(|lua, code: String| {
            if let Some((data, bits)) = lua_packet(&code) {
                net(lua)?.queue.push(Outgoing {
                    targets: Targets::All,
                    reliable: true,
                    data,
                    bits,
                });
            }
            Ok(())
        })?,
    )?;
    let _ = MultiValue::new();
    Ok(())
}

/// GMod packet carrying Lua code for the client (type 5); `None` when too long or
/// when it contains the pattern the reference server refuses (`while true do end`).
pub fn lua_packet(code: &str) -> Option<(Vec<u8>, usize)> {
    if code.len() > 6000 || code.contains("while true do end") {
        return None;
    }
    let mut data = Vec::with_capacity(code.len() + 2);
    data.push(5);
    data.extend(code.bytes().filter(|b| *b != 0));
    data.push(0);
    let bits = data.len() * 8;
    Some((data, bits))
}

impl crate::GLua {
    /// A Lua net message body from a client (after the type byte): runs
    /// `net.Incoming(bits, ply)`.
    pub fn net_incoming(&self, ply: EntRef, data: Vec<u8>, bits: usize) {
        if let Ok(mut n) = net(&self.lua) {
            n.incoming = Some(Reader::new(data, bits));
        }
        let run = || -> mlua::Result<()> {
            let t: Table = self.lua.globals().get("net")?;
            let f: mlua::Function = t.get("Incoming")?;
            f.call::<()>((bits as i64, ply))
        };
        if let Err(e) = run() {
            crate::runtime::report(&self.state, &e);
        }
        if let Ok(mut n) = net(&self.lua) {
            n.incoming = None;
        }
    }

    /// Net messages finished since the last call.
    pub fn take_net_messages(&self) -> Vec<Outgoing> {
        net(&self.lua).map(|mut n| std::mem::take(&mut n.queue)).unwrap_or_default()
    }

    /// Network strings registered by Lua (`util.AddNetworkString`), id = index + 1.
    pub fn network_strings(&self) -> Vec<String> {
        self.lua
            .app_data_ref::<crate::lib_misc::Engine>()
            .map(|e| e.network_strings.clone())
            .unwrap_or_default()
    }
}

#[allow(dead_code)]
fn _store(lua: &Lua) {
    let _ = store(lua);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coord_roundtrip() {
        let mut b = Bits::default();
        b.vec3_coord([12.5, 0.0, -300.25]);
        b.vec3_normal([0.6, 0.0, -0.8]);
        let mut r = Reader::new(b.data.clone(), b.bits);
        assert_eq!(r.vec3_coord(), [12.5, 0.0, -300.25]);
        let n = r.vec3_normal();
        assert!((n[0] - 0.6).abs() < 1e-3 && n[1] == 0.0 && (n[2] + 0.8).abs() < 1e-2, "{n:?}");
    }

    #[test]
    fn message_roundtrip_through_lua() {
        let g = crate::GLua::new(std::path::Path::new(".")).unwrap();
        g.lua
            .load(
                "util.AddNetworkString('a') util.AddNetworkString('msg')
                 net.Start('msg') net.WriteUInt(300, 9) net.WriteString('hi') net.WriteFloat(1.5) net.WriteVector(Vector(1, 2, 3))
                 net.Broadcast()",
            )
            .exec()
            .unwrap();
        let out = g.take_net_messages();
        assert_eq!(out.len(), 1);
        let m = &out[0];
        assert_eq!(m.targets, Targets::All);
        let mut r = Reader::new(m.data.clone(), m.bits);
        assert_eq!(r.read(8), 0);
        assert_eq!(r.read(16), 2);
        assert_eq!(r.read(9), 300);
        // feed the body (after the type byte) back as an incoming message
        g.lua
            .load(
                "GOT = nil
                 net.Receivers = {}
                 net.Incoming = function(len) local id = net.ReadHeader() GOT = { id, net.ReadUInt(9), net.ReadString(), net.ReadFloat(), net.ReadVector() } end",
            )
            .exec()
            .unwrap();
        g.net_incoming(EntRef::NULL, m.data[1..].to_vec(), m.bits - 8);
        let ok: bool = g
            .lua
            .load("return GOT[1] == 2 and GOT[2] == 300 and GOT[3] == 'hi' and GOT[4] == 1.5 and GOT[5] == Vector(1,2,3)")
            .eval()
            .unwrap();
        assert!(ok);
    }
}
