//! Game events: the descriptor list sent at signon (`svc_GameEventList`) and events
//! fired to clients (`svc_GameEvent`), read from the game's `resource/*events.res`.
//!
//! [факт] wire format (reference demo, parses bit-exact): list = 9-bit count, 20-bit
//! data length, then per event a 9-bit id, the name, and its keys as (3-bit type,
//! name) pairs ended by type 0. Types: 1 string, 2 float, 3 long, 4 short, 5 byte,
//! 6 bool, 7 uint64 ("local" keys are not networked).

use srcds_protocol::bitbuf::BitWriter;
use srcds_protocol::error::Result;

use crate::vpk::Content;

pub const SVC_GAMEEVENT: u32 = 25;
pub const SVC_GAMEEVENTLIST: u32 = 30;

#[derive(Debug, Clone, PartialEq)]
pub struct EventDesc {
    pub id: u32,
    pub name: String,
    pub keys: Vec<(String, u8)>,
}

/// A value of an event key.
#[derive(Debug, Clone, PartialEq)]
pub enum EventValue {
    Str(String),
    Int(i64),
    Float(f32),
    Bool(bool),
}

#[derive(Debug, Clone, Default)]
pub struct GameEvents {
    pub events: Vec<EventDesc>,
}

fn kv_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '"' => {
                let mut s = String::new();
                for c in it.by_ref() {
                    if c == '"' {
                        break;
                    }
                    s.push(c);
                }
                out.push(s);
            }
            '{' | '}' => out.push(c.to_string()),
            '/' if it.peek() == Some(&'/') => {
                for c in it.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => {}
            c => {
                let mut s = c.to_string();
                while let Some(&n) = it.peek() {
                    if n.is_whitespace() || n == '"' || n == '{' || n == '}' {
                        break;
                    }
                    s.push(n);
                    it.next();
                }
                out.push(s);
            }
        }
    }
    out
}

fn type_id(t: &str) -> Option<u8> {
    Some(match t.to_ascii_lowercase().as_str() {
        "string" => 1,
        "float" => 2,
        "long" => 3,
        "short" => 4,
        "byte" => 5,
        "bool" => 6,
        "uint64" => 7,
        _ => return None, // "local" and unknown: not networked
    })
}

impl GameEvents {
    /// Parses one events file (`"name" { "event" { "key" "type" ... } ... }`).
    pub fn add_file(&mut self, text: &str) {
        let toks = kv_tokens(text);
        let mut i = 0;
        // skip the root name and its "{"
        while i < toks.len() && toks[i] != "{" {
            i += 1;
        }
        i += 1;
        while i < toks.len() && toks[i] != "}" {
            let name = toks[i].clone();
            i += 1;
            if toks.get(i).map(String::as_str) != Some("{") {
                continue;
            }
            i += 1;
            let mut keys = Vec::new();
            while i + 1 < toks.len() && toks[i] != "}" {
                if let Some(t) = type_id(&toks[i + 1]) {
                    keys.push((toks[i].clone(), t));
                }
                i += 2;
            }
            i += 1; // "}"
            if self.events.iter().any(|e| e.name.eq_ignore_ascii_case(&name)) || self.events.len() >= 511 {
                continue;
            }
            let id = self.events.len() as u32;
            self.events.push(EventDesc { id, name, keys });
        }
    }

    /// Engine, game, mod and HLTV event files, in that order (the reference server's
    /// list starts with `server_spawn` and ends with the `hltv_*` events).
    pub fn load(content: &Content) -> GameEvents {
        let mut g = GameEvents::default();
        for f in [
            "resource/serverevents.res",
            "resource/gameevents.res",
            "resource/modevents.res",
            "resource/hltvevents.res",
        ] {
            if let Some(b) = content.read(f, 256 * 1024) {
                g.add_file(&String::from_utf8_lossy(&b));
            }
        }
        tracing::info!(events = g.events.len(), "game events loaded");
        g
    }

    pub fn write_list(&self, w: &mut BitWriter) -> Result<()> {
        let mut d = BitWriter::with_capacity(16 * 1024);
        for e in &self.events {
            d.write_ubits(e.id, 9)?;
            d.write_string(&e.name)?;
            for (k, t) in &e.keys {
                d.write_ubits(*t as u32, 3)?;
                d.write_string(k)?;
            }
            d.write_ubits(0, 3)?;
        }
        w.write_ubits(SVC_GAMEEVENTLIST, 6)?;
        w.write_ubits(self.events.len() as u32, 9)?;
        w.write_ubits(d.bit_len() as u32, 20)?;
        w.append(&d)
    }

    /// `svc_GameEvent` for `name` with the given values (missing keys are zero/empty).
    /// Returns false when the event is unknown.
    pub fn write_event(&self, w: &mut BitWriter, name: &str, values: &[(&str, EventValue)]) -> Result<bool> {
        let Some(e) = self.events.iter().find(|e| e.name == name) else { return Ok(false) };
        let mut d = BitWriter::with_capacity(512);
        d.write_ubits(e.id, 9)?;
        for (k, t) in &e.keys {
            let v = values.iter().find(|(n, _)| n == k).map(|(_, v)| v);
            let int = |v: Option<&EventValue>| match v {
                Some(EventValue::Int(i)) => *i,
                Some(EventValue::Bool(b)) => *b as i64,
                Some(EventValue::Float(f)) => *f as i64,
                _ => 0,
            };
            match t {
                1 => d.write_string(match v {
                    Some(EventValue::Str(s)) => s,
                    _ => "",
                })?,
                2 => d.write_f32(match v {
                    Some(EventValue::Float(f)) => *f,
                    Some(EventValue::Int(i)) => *i as f32,
                    _ => 0.0,
                })?,
                3 => d.write_ubits(int(v) as u32, 32)?,
                4 => d.write_ubits(int(v) as u32 & 0xFFFF, 16)?,
                5 => d.write_ubits(int(v) as u32 & 0xFF, 8)?,
                6 => d.write_bit(int(v) != 0)?,
                _ => {
                    let x = int(v) as u64;
                    d.write_ubits(x as u32, 32)?;
                    d.write_ubits((x >> 32) as u32, 32)?;
                }
            }
        }
        if d.bit_len() >= 1 << 11 {
            return Ok(false);
        }
        w.write_ubits(SVC_GAMEEVENT, 6)?;
        w.write_ubits(d.bit_len() as u32, 11)?;
        w.append(&d)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_file_and_writes() {
        let mut g = GameEvents::default();
        g.add_file(
            "\"gameevents\" {\n \"player_connect\" { \"name\" \"string\" \"index\" \"byte\" \"userid\" \"short\" \"x\" \"local\" }\n \"host_quit\" {}\n}",
        );
        assert_eq!(g.events.len(), 2);
        assert_eq!(g.events[0].keys, [("name".to_string(), 1), ("index".to_string(), 5), ("userid".to_string(), 4)]);
        let mut w = BitWriter::with_capacity(256);
        g.write_list(&mut w).unwrap();
        let mut w2 = BitWriter::with_capacity(256);
        assert!(g
            .write_event(&mut w2, "player_connect", &[("name", EventValue::Str("a".into())), ("userid", EventValue::Int(2))])
            .unwrap());
        // 6 type + 11 length + 9 id + "a\0" + 8 + 16
        assert_eq!(w2.bit_len(), 6 + 11 + 9 + 16 + 8 + 16);
    }

    /// The real files give the reference server's list (`SRCDS_RS_GMOD_DIR`).
    #[test]
    fn real_event_files() {
        let Ok(root) = std::env::var("SRCDS_RS_GMOD_DIR") else { return };
        let g = GameEvents::load(&Content::mount(std::path::Path::new(&root)));
        let names: Vec<&str> = g.events.iter().map(|e| e.name.as_str()).collect();
        eprintln!("{} events: first {:?} last {:?}", names.len(), names.first(), names.last());
        assert_eq!(names.first(), Some(&"server_spawn"));
        assert!(names.contains(&"player_connect") && names.contains(&"player_death") && names.contains(&"hltv_chat"));
    }
}
