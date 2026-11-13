//! Connection handshake (connectionless, `-1` header) between a game client and the
//! server: `A2S_GETCHALLENGE` → `S2C_CHALLENGE` → `C2S_CONNECT` → `S2C_CONNECTION`
//! (or `S2C_CONNREJECT`).
//!
//! Field layouts were derived from a capture of the unmodified GMod client talking to
//! the reference GMod dedicated server (2026.04.29) and cross-checked with the public
//! `proto_oob.h`-style type bytes. See `docs/PROTOCOL.md` §Handshake.

use crate::bytes::{ByteReader, ByteWriter};
use crate::error::{ProtocolError, Result};

pub const A2S_GETCHALLENGE: u8 = b'q';
pub const C2S_CONNECT: u8 = b'k';
pub const S2C_CHALLENGE: u8 = b'A';
pub const S2C_CONNECTION: u8 = b'B';
pub const S2C_CONNREJECT: u8 = b'9';

/// Magic value carried in `S2C_CHALLENGE` (`"3IOZ"` little-endian) used by clients to
/// detect protocol generations.
pub const S2C_MAGICVERSION: u32 = 0x5A4F_4933;
/// Network protocol version spoken by the GMod client (observed in `C2S_CONNECT`).
pub const PROTOCOL_VERSION: i32 = 24;
/// Authentication protocol "Steam certificates".
pub const AUTH_PROTOCOL_STEAM: i32 = 3;
/// Padding string the engine appends to several handshake packets.
const PAD_10: &str = "0000000000";
const PAD_6: &str = "000000";

/// `A2S_GETCHALLENGE` request: `'q'`, client challenge, padding string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GetChallenge {
    pub client_challenge: u32,
}

impl GetChallenge {
    /// Parses the body after the `'q'` byte. Padding is optional and never interpreted.
    pub fn parse(body: &[u8]) -> Result<Self> {
        let mut r = ByteReader::new(body);
        Ok(Self {
            client_challenge: r.u32()?,
        })
    }
}

/// `S2C_CHALLENGE` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChallengeResponse {
    pub server_challenge: u32,
    pub client_challenge: u32,
    pub auth_protocol: i32,
    /// SteamID of the game server. The reference server without a Steam login reports 1.
    pub server_steam_id: u64,
    pub secure: bool,
}

impl ChallengeResponse {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut w = ByteWriter::with_capacity(64);
        w.i32(-1)?;
        w.u8(S2C_CHALLENGE)?;
        w.u32(S2C_MAGICVERSION)?;
        w.u32(self.server_challenge)?;
        w.u32(self.client_challenge)?;
        w.i32(self.auth_protocol)?;
        w.u16(0)?; // steam2 encryption key length (unused)
        w.u64(self.server_steam_id)?;
        w.u8(self.secure as u8)?;
        w.cstr(PAD_6)?;
        Ok(w.into_inner())
    }

    pub fn decode(payload: &[u8]) -> Result<Self> {
        let mut r = ByteReader::new(payload);
        if r.i32()? != -1 || r.u8()? != S2C_CHALLENGE {
            return Err(ProtocolError::InvalidField {
                field: "s2c_challenge",
                reason: "bad header",
            });
        }
        if r.u32()? != S2C_MAGICVERSION {
            return Err(ProtocolError::InvalidField {
                field: "s2c_challenge",
                reason: "bad magic",
            });
        }
        let server_challenge = r.u32()?;
        let client_challenge = r.u32()?;
        let auth_protocol = r.i32()?;
        let key_len = r.u16()? as usize;
        r.bytes(key_len)?;
        let server_steam_id = r.u64()?;
        let secure = r.u8()? != 0;
        Ok(Self {
            server_challenge,
            client_challenge,
            auth_protocol,
            server_steam_id,
            secure,
        })
    }
}

/// Maximum accepted length of strings inside `C2S_CONNECT`.
const MAX_CONNECT_STRING: usize = 256;
/// Upper bound for the Steam auth blob carried in `C2S_CONNECT`.
pub const MAX_STEAM_COOKIE: usize = 2048;

/// `C2S_CONNECT` request (body after `'k'`).
///
/// The Steam cookie (SteamID + session ticket) is kept as an opaque, bounded slice.
/// It is the client's credential: it must never be logged or persisted.
#[derive(Clone, PartialEq, Eq)]
pub struct ConnectRequest<'a> {
    pub protocol: i32,
    pub auth_protocol: i32,
    pub server_challenge: u32,
    pub client_challenge: u32,
    /// GMod-specific 32-bit field between the challenges and the name (meaning not
    /// established; preserved, not interpreted).
    pub gmod_extra: u32,
    pub name: String,
    pub password: String,
    pub product_version: String,
    pub steam_cookie: &'a [u8],
}

impl std::fmt::Debug for ConnectRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectRequest")
            .field("protocol", &self.protocol)
            .field("auth_protocol", &self.auth_protocol)
            .field("name", &self.name)
            .field("password", &"<redacted>")
            .field("product_version", &self.product_version)
            .field("steam_cookie_len", &self.steam_cookie.len())
            .finish()
    }
}

impl<'a> ConnectRequest<'a> {
    pub fn parse(body: &'a [u8]) -> Result<Self> {
        let mut r = ByteReader::new(body);
        let protocol = r.i32()?;
        let auth_protocol = r.i32()?;
        let server_challenge = r.u32()?;
        let client_challenge = r.u32()?;
        let gmod_extra = r.u32()?;
        let name = r.cstr(MAX_CONNECT_STRING)?;
        let password = r.cstr(MAX_CONNECT_STRING)?;
        let product_version = r.cstr(MAX_CONNECT_STRING)?;
        let steam_cookie = if auth_protocol == AUTH_PROTOCOL_STEAM && r.remaining() >= 2 {
            let len = r.u16()? as usize;
            if len > MAX_STEAM_COOKIE {
                return Err(ProtocolError::Oversized {
                    size: len,
                    limit: MAX_STEAM_COOKIE,
                });
            }
            r.bytes(len)?
        } else {
            &[][..]
        };
        Ok(Self {
            protocol,
            auth_protocol,
            server_challenge,
            client_challenge,
            gmod_extra,
            name,
            password,
            product_version,
            steam_cookie,
        })
    }

    /// The client's SteamID64 as stated in the (unverified) cookie, if present.
    pub fn claimed_steam_id(&self) -> Option<u64> {
        let b = self.steam_cookie.get(..8)?;
        Some(u64::from_le_bytes(b.try_into().ok()?))
    }

    /// Encodes the request (used by tests and the in-repo test client).
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut w = ByteWriter::with_capacity(64 + self.steam_cookie.len() + 3 * 256);
        w.i32(-1)?;
        w.u8(C2S_CONNECT)?;
        w.i32(self.protocol)?;
        w.i32(self.auth_protocol)?;
        w.u32(self.server_challenge)?;
        w.u32(self.client_challenge)?;
        w.u32(self.gmod_extra)?;
        w.cstr(&self.name)?;
        w.cstr(&self.password)?;
        w.cstr(&self.product_version)?;
        if self.auth_protocol == AUTH_PROTOCOL_STEAM {
            w.u16(self.steam_cookie.len() as u16)?;
            w.bytes(self.steam_cookie)?;
        }
        Ok(w.into_inner())
    }
}

/// `S2C_CONNECTION`: the server accepted the client; netchannel traffic follows.
pub fn encode_connection(client_challenge: u32) -> Result<Vec<u8>> {
    let mut w = ByteWriter::with_capacity(32);
    w.i32(-1)?;
    w.u8(S2C_CONNECTION)?;
    w.u32(client_challenge)?;
    w.cstr(PAD_10)?;
    Ok(w.into_inner())
}

/// `S2C_CONNREJECT` with a human-readable reason shown by the client.
pub fn encode_reject(client_challenge: u32, reason: &str) -> Result<Vec<u8>> {
    let reason: String = reason.chars().filter(|&c| c != '\0').take(200).collect();
    let mut w = ByteWriter::with_capacity(16 + reason.len());
    w.i32(-1)?;
    w.u8(S2C_CONNREJECT)?;
    w.u32(client_challenge)?;
    w.cstr(&reason)?;
    Ok(w.into_inner())
}

/// Encodes an `A2S_GETCHALLENGE` (test-client side).
pub fn encode_get_challenge(client_challenge: u32) -> Result<Vec<u8>> {
    let mut w = ByteWriter::with_capacity(32);
    w.i32(-1)?;
    w.u8(A2S_GETCHALLENGE)?;
    w.u32(client_challenge)?;
    w.cstr(PAD_10)?;
    Ok(w.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic `S2C_CHALLENGE` with the exact layout observed from the reference
    /// server (challenge values are arbitrary test data).
    #[test]
    fn challenge_layout_matches_reference_shape() {
        let c = ChallengeResponse {
            server_challenge: 0x5B07_DFE9,
            client_challenge: 0x073F_5550,
            auth_protocol: AUTH_PROTOCOL_STEAM,
            server_steam_id: 1,
            secure: false,
        };
        let enc = c.encode().unwrap();
        assert_eq!(enc.len(), 39, "reference S2C_CHALLENGE is 39 bytes");
        assert_eq!(
            hex(&enc),
            "ffffffff4133494f5ae9df075b50553f0703000000000001000000000000000030303030303000"
        );
        assert_eq!(ChallengeResponse::decode(&enc).unwrap(), c);
    }

    #[test]
    fn connection_and_get_challenge_layout() {
        assert_eq!(
            hex(&encode_connection(0x073F_5550).unwrap()),
            "ffffffff4250553f073030303030303030303000"
        );
        assert_eq!(
            hex(&encode_get_challenge(0x073F_5550).unwrap()),
            "ffffffff7150553f073030303030303030303000"
        );
        let body = &encode_get_challenge(7).unwrap()[5..];
        assert_eq!(GetChallenge::parse(body).unwrap().client_challenge, 7);
        assert!(GetChallenge::parse(&body[..3]).is_err());
    }

    #[test]
    fn connect_roundtrip_and_truncation() {
        // synthetic cookie: SteamID64 0x0110_0001_0000_0003 + dummy ticket bytes
        let cookie = [0x03u8, 0, 0, 0, 1, 0, 0x10, 1, 9, 9, 9];
        let req = ConnectRequest {
            protocol: 24,
            auth_protocol: AUTH_PROTOCOL_STEAM,
            server_challenge: 1,
            client_challenge: 2,
            gmod_extra: 3,
            name: "player".into(),
            password: "".into(),
            product_version: "2026.04.29".into(),
            steam_cookie: &cookie,
        };
        let enc = req.encode().unwrap();
        let parsed = ConnectRequest::parse(&enc[5..]).unwrap();
        assert_eq!(parsed, req);
        assert_eq!(parsed.claimed_steam_id(), Some(0x0110_0001_0000_0003));
        assert!(format!("{parsed:?}").contains("<redacted>"));
        for cut in 0..enc.len() - 5 {
            let _ = ConnectRequest::parse(&enc[5..5 + cut]);
        }
        assert!(ConnectRequest::parse(&enc[5..20]).is_err());
    }

    #[test]
    fn oversized_cookie_rejected() {
        let mut body = Vec::new();
        for v in [24i32, 3, 1, 2, 3] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(b"n\0\0v\0");
        body.extend_from_slice(&(MAX_STEAM_COOKIE as u16 + 1).to_le_bytes());
        assert!(matches!(
            ConnectRequest::parse(&body),
            Err(ProtocolError::Oversized { .. })
        ));
    }

    #[test]
    fn reject_strips_nul() {
        let r = encode_reject(5, "bad\0reason").unwrap();
        assert_eq!(&r[9..], b"badreason\0");
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
