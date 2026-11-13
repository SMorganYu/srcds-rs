//! A2S response messages (Valve Developer Wiki, "Server queries"; Source format,
//! not GoldSrc). Encoders write the complete datagram including the `-1` header;
//! decoders take the payload after the header and are used by tools and tests.

use crate::bytes::{ByteReader, ByteWriter};
use crate::connectionless::{kind, CONNECTIONLESS_HEADER};
use crate::error::{ProtocolError, Result};

/// Maximum length we accept for any A2S string when decoding.
pub const MAX_STRING: usize = 1024;

/// Extra Data Flag bits for A2S_INFO.
pub mod edf {
    pub const PORT: u8 = 0x80;
    pub const STEAMID: u8 = 0x10;
    pub const SOURCETV: u8 = 0x40;
    pub const KEYWORDS: u8 = 0x20;
    pub const GAMEID: u8 = 0x01;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTv {
    pub port: u16,
    pub name: String,
}

/// A2S_INFO response (`'I'`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoResponse {
    /// Network protocol version byte reported to query clients.
    pub protocol: u8,
    pub name: String,
    pub map: String,
    pub folder: String,
    pub game: String,
    /// Low 16 bits of the Steam AppID.
    pub app_id: u16,
    pub players: u8,
    pub max_players: u8,
    pub bots: u8,
    /// `'d'` dedicated, `'l'` listen, `'p'` SourceTV relay.
    pub server_type: u8,
    /// `'w'` Windows, `'l'` Linux, `'m'`/`'o'` macOS.
    pub environment: u8,
    /// 1 if password protected.
    pub visibility: u8,
    /// 1 if VAC secured.
    pub vac: u8,
    pub version: String,
    pub port: Option<u16>,
    pub steam_id: Option<u64>,
    pub source_tv: Option<SourceTv>,
    pub keywords: Option<String>,
    pub game_id: Option<u64>,
}

impl InfoResponse {
    pub fn edf(&self) -> u8 {
        let mut f = 0;
        if self.port.is_some() {
            f |= edf::PORT;
        }
        if self.steam_id.is_some() {
            f |= edf::STEAMID;
        }
        if self.source_tv.is_some() {
            f |= edf::SOURCETV;
        }
        if self.keywords.is_some() {
            f |= edf::KEYWORDS;
        }
        if self.game_id.is_some() {
            f |= edf::GAMEID;
        }
        f
    }

    pub fn encode(&self, w: &mut ByteWriter) -> Result<()> {
        w.i32(CONNECTIONLESS_HEADER)?;
        w.u8(kind::S2A_INFO)?;
        w.u8(self.protocol)?;
        w.cstr(&self.name)?;
        w.cstr(&self.map)?;
        w.cstr(&self.folder)?;
        w.cstr(&self.game)?;
        w.u16(self.app_id)?;
        w.u8(self.players)?;
        w.u8(self.max_players)?;
        w.u8(self.bots)?;
        w.u8(self.server_type)?;
        w.u8(self.environment)?;
        w.u8(self.visibility)?;
        w.u8(self.vac)?;
        w.cstr(&self.version)?;
        let flags = self.edf();
        if flags != 0 {
            w.u8(flags)?;
        }
        // Field order is fixed by the EDF specification.
        if let Some(p) = self.port {
            w.u16(p)?;
        }
        if let Some(id) = self.steam_id {
            w.u64(id)?;
        }
        if let Some(tv) = &self.source_tv {
            w.u16(tv.port)?;
            w.cstr(&tv.name)?;
        }
        if let Some(k) = &self.keywords {
            w.cstr(k)?;
        }
        if let Some(g) = self.game_id {
            w.u64(g)?;
        }
        Ok(())
    }

    /// Decodes the payload after the `-1` header.
    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut r = ByteReader::new(payload);
        let k = r.u8()?;
        if k != kind::S2A_INFO {
            return Err(ProtocolError::UnknownType(k));
        }
        let mut info = InfoResponse {
            protocol: r.u8()?,
            name: r.cstr(MAX_STRING)?,
            map: r.cstr(MAX_STRING)?,
            folder: r.cstr(MAX_STRING)?,
            game: r.cstr(MAX_STRING)?,
            app_id: r.u16()?,
            players: r.u8()?,
            max_players: r.u8()?,
            bots: r.u8()?,
            server_type: r.u8()?,
            environment: r.u8()?,
            visibility: r.u8()?,
            vac: r.u8()?,
            version: r.cstr(MAX_STRING)?,
            port: None,
            steam_id: None,
            source_tv: None,
            keywords: None,
            game_id: None,
        };
        if r.is_empty() {
            return Ok(info);
        }
        let f = r.u8()?;
        if f & edf::PORT != 0 {
            info.port = Some(r.u16()?);
        }
        if f & edf::STEAMID != 0 {
            info.steam_id = Some(r.u64()?);
        }
        if f & edf::SOURCETV != 0 {
            info.source_tv = Some(SourceTv {
                port: r.u16()?,
                name: r.cstr(MAX_STRING)?,
            });
        }
        if f & edf::KEYWORDS != 0 {
            info.keywords = Some(r.cstr(MAX_STRING)?);
        }
        if f & edf::GAMEID != 0 {
            info.game_id = Some(r.u64()?);
        }
        Ok(info)
    }
}

/// S2C_CHALLENGE (`'A'`).
pub fn encode_challenge(w: &mut ByteWriter, challenge: i32) -> Result<()> {
    w.i32(CONNECTIONLESS_HEADER)?;
    w.u8(kind::S2C_CHALLENGE)?;
    w.i32(challenge)
}

pub fn decode_challenge(payload: &[u8]) -> Result<i32> {
    let mut r = ByteReader::new(payload);
    let k = r.u8()?;
    if k != kind::S2C_CHALLENGE {
        return Err(ProtocolError::UnknownType(k));
    }
    r.i32()
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerEntry {
    pub index: u8,
    pub name: String,
    pub score: i32,
    /// Seconds connected.
    pub duration: f32,
}

/// A2S_PLAYER response (`'D'`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlayerResponse {
    pub players: Vec<PlayerEntry>,
}

impl PlayerResponse {
    pub fn encode(&self, w: &mut ByteWriter) -> Result<()> {
        let count = u8::try_from(self.players.len()).map_err(|_| ProtocolError::InvalidField {
            field: "players",
            reason: "more than 255 entries",
        })?;
        w.i32(CONNECTIONLESS_HEADER)?;
        w.u8(kind::S2A_PLAYER)?;
        w.u8(count)?;
        for p in &self.players {
            w.u8(p.index)?;
            w.cstr(&p.name)?;
            w.i32(p.score)?;
            w.f32(p.duration)?;
        }
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut r = ByteReader::new(payload);
        let k = r.u8()?;
        if k != kind::S2A_PLAYER {
            return Err(ProtocolError::UnknownType(k));
        }
        let n = r.u8()? as usize;
        // Each entry is at least 10 bytes; bound the allocation by the input size.
        let mut players = Vec::with_capacity(n.min(r.remaining() / 10));
        for _ in 0..n {
            players.push(PlayerEntry {
                index: r.u8()?,
                name: r.cstr(MAX_STRING)?,
                score: r.i32()?,
                duration: r.f32()?,
            });
        }
        Ok(Self { players })
    }
}

/// Ordering of rule names observed from the reference server: bytewise compare
/// of ASCII-uppercased names (so `_` sorts after letters). Hypothesis derived from
/// a 148-entry capture; see `tests/reference_fixtures.rs`.
pub fn rule_name_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes()
        .map(|c| c.to_ascii_uppercase())
        .cmp(b.bytes().map(|c| c.to_ascii_uppercase()))
}

/// A2S_RULES response (`'E'`). Encodes to a single logical payload which may need
/// splitting (see [`crate::split`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RulesResponse {
    pub rules: Vec<(String, String)>,
}

impl RulesResponse {
    pub fn encode(&self, w: &mut ByteWriter) -> Result<()> {
        let count = u16::try_from(self.rules.len()).map_err(|_| ProtocolError::InvalidField {
            field: "rules",
            reason: "more than 65535 entries",
        })?;
        w.i32(CONNECTIONLESS_HEADER)?;
        w.u8(kind::S2A_RULES)?;
        w.u16(count)?;
        for (k, v) in &self.rules {
            w.cstr(k)?;
            w.cstr(v)?;
        }
        Ok(())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut r = ByteReader::new(payload);
        let k = r.u8()?;
        if k != kind::S2A_RULES {
            return Err(ProtocolError::UnknownType(k));
        }
        let n = r.u16()? as usize;
        let mut rules = Vec::with_capacity(n.min(r.remaining() / 2));
        for _ in 0..n {
            rules.push((r.cstr(MAX_STRING)?, r.cstr(MAX_STRING)?));
        }
        Ok(Self { rules })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_info() -> InfoResponse {
        InfoResponse {
            protocol: 17,
            name: "srcds-rs".into(),
            map: "gm_construct".into(),
            folder: "garrysmod".into(),
            game: "Sandbox".into(),
            app_id: 4000,
            players: 0,
            max_players: 32,
            bots: 0,
            server_type: b'd',
            environment: b'w',
            visibility: 0,
            vac: 0,
            version: "2026.04.29".into(),
            port: Some(27015),
            steam_id: None,
            source_tv: Some(SourceTv {
                port: 27020,
                name: "tv".into(),
            }),
            keywords: Some("gm:sandbox".into()),
            game_id: Some(4000),
        }
    }

    #[test]
    fn info_roundtrip() {
        let info = sample_info();
        let mut w = ByteWriter::with_capacity(1400);
        info.encode(&mut w).unwrap();
        let data = w.into_inner();
        assert_eq!(&data[..5], b"\xff\xff\xff\xffI");
        assert_eq!(InfoResponse::decode(&data[4..]).unwrap(), info);
    }

    #[test]
    fn info_without_edf() {
        let mut info = sample_info();
        info.port = None;
        info.source_tv = None;
        info.keywords = None;
        info.game_id = None;
        let mut w = ByteWriter::with_capacity(1400);
        info.encode(&mut w).unwrap();
        let data = w.into_inner();
        assert_eq!(*data.last().unwrap(), 0, "ends with version NUL");
        assert_eq!(InfoResponse::decode(&data[4..]).unwrap(), info);
    }

    #[test]
    fn info_truncated_everywhere() {
        let mut w = ByteWriter::with_capacity(1400);
        sample_info().encode(&mut w).unwrap();
        let data = w.into_inner();
        // A response ending right after the version string (no EDF) is valid.
        let mut base = sample_info();
        base.port = None;
        base.source_tv = None;
        base.keywords = None;
        base.game_id = None;
        let mut w = ByteWriter::with_capacity(1400);
        base.encode(&mut w).unwrap();
        let no_edf_len = w.len();
        for cut in (4..data.len() - 1).filter(|&c| c != no_edf_len) {
            assert!(
                InfoResponse::decode(&data[4..cut]).is_err(),
                "cut at {cut} should fail"
            );
        }
    }

    #[test]
    fn player_roundtrip_and_hostile_count() {
        let resp = PlayerResponse {
            players: vec![PlayerEntry {
                index: 0,
                name: "Player".into(),
                score: 3,
                duration: 12.5,
            }],
        };
        let mut w = ByteWriter::with_capacity(1400);
        resp.encode(&mut w).unwrap();
        let data = w.into_inner();
        assert_eq!(PlayerResponse::decode(&data[4..]).unwrap(), resp);
        // Claims 255 players but carries none: must fail without huge allocation.
        assert!(PlayerResponse::decode(b"D\xff").is_err());
    }

    #[test]
    fn rules_roundtrip_and_hostile_count() {
        let resp = RulesResponse {
            rules: vec![("sv_gravity".into(), "600".into())],
        };
        let mut w = ByteWriter::with_capacity(1400);
        resp.encode(&mut w).unwrap();
        let data = w.into_inner();
        assert_eq!(RulesResponse::decode(&data[4..]).unwrap(), resp);
        assert!(RulesResponse::decode(b"E\xff\xff").is_err());
    }

    #[test]
    fn challenge_roundtrip() {
        let mut w = ByteWriter::with_capacity(16);
        encode_challenge(&mut w, -12345).unwrap();
        let d = w.into_inner();
        assert_eq!(d.len(), 9);
        assert_eq!(decode_challenge(&d[4..]).unwrap(), -12345);
        assert!(decode_challenge(b"A\x01").is_err());
        assert!(decode_challenge(b"B\x01\x02\x03\x04").is_err());
    }

    #[test]
    fn oversized_info_rejected_by_writer() {
        let mut info = sample_info();
        info.name = "x".repeat(2000);
        let mut w = ByteWriter::with_capacity(1400);
        assert!(matches!(
            info.encode(&mut w),
            Err(ProtocolError::Overflow { .. })
        ));
    }
}
