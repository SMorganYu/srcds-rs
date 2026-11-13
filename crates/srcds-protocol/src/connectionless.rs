//! Connectionless ("out-of-band") packet framing.
//!
//! Every UDP datagram on a Source game port starts with a 32-bit little-endian
//! header: `-1` (`FF FF FF FF`) for a single connectionless packet, `-2`
//! (`FE FF FF FF`) for one fragment of a split connectionless packet; anything else
//! is a netchannel packet whose first word is the sequence number.
//! Source: Valve Developer Wiki, "Server queries".

use crate::bytes::ByteReader;
use crate::error::{ProtocolError, Result};

pub const CONNECTIONLESS_HEADER: i32 = -1;
pub const SPLIT_HEADER: i32 = -2;

/// Request / response type bytes (first byte after the `-1` header).
pub mod kind {
    /// A2S_INFO request.
    pub const A2S_INFO: u8 = b'T';
    /// A2S_PLAYER request.
    pub const A2S_PLAYER: u8 = b'U';
    /// A2S_RULES request.
    pub const A2S_RULES: u8 = b'V';
    /// Deprecated A2S_SERVERQUERY_GETCHALLENGE (string present in engine.dll).
    pub const A2S_SERVERQUERY_GETCHALLENGE: u8 = b'W';
    /// A2A_PING (deprecated by Valve; not answered by srcds-rs).
    pub const A2A_PING: u8 = b'i';
    /// A2S_GETCHALLENGE, the first step of a game client connection (M2 research).
    pub const A2S_GETCHALLENGE: u8 = b'q';
    /// C2S_CONNECT (M2 research).
    pub const C2S_CONNECT: u8 = b'k';

    /// S2C_CHALLENGE response to query requests.
    pub const S2C_CHALLENGE: u8 = b'A';
    /// A2S_INFO response.
    pub const S2A_INFO: u8 = b'I';
    /// A2S_PLAYER response.
    pub const S2A_PLAYER: u8 = b'D';
    /// A2S_RULES response.
    pub const S2A_RULES: u8 = b'E';
}

/// Literal payload of an A2S_INFO request.
pub const INFO_QUERY_STRING: &[u8] = b"Source Engine Query\0";

/// Classification of a raw datagram by its first word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing<'a> {
    /// `-1` header; the slice is everything after the header.
    Connectionless(&'a [u8]),
    /// `-2` header; the slice is the whole datagram (split header included).
    Split(&'a [u8]),
    /// Netchannel datagram (sequenced); the slice is the whole datagram.
    Netchannel(&'a [u8]),
}

pub fn classify(datagram: &[u8]) -> Result<Framing<'_>> {
    let mut r = ByteReader::new(datagram);
    let header = r.i32()?;
    Ok(match header {
        CONNECTIONLESS_HEADER => Framing::Connectionless(r.rest()),
        SPLIT_HEADER => Framing::Split(datagram),
        _ => Framing::Netchannel(datagram),
    })
}

/// A parsed connectionless query request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryRequest {
    /// A2S_INFO with optional challenge (absent on the first, unchallenged request).
    Info { challenge: Option<i32> },
    /// A2S_PLAYER; `-1` means "please send me a challenge".
    Player { challenge: i32 },
    /// A2S_RULES; `-1` means "please send me a challenge".
    Rules { challenge: i32 },
    /// Deprecated explicit challenge request.
    ServerQueryGetChallenge,
}

/// Outcome of parsing a connectionless payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connectionless<'a> {
    Query(QueryRequest),
    /// A recognised but not (yet) handled type, e.g. connection handshake messages.
    Unhandled {
        kind: u8,
        body: &'a [u8],
    },
}

/// Parses the payload following a `-1` header.
///
/// Trailing bytes after a complete request are tolerated (real clients may pad
/// requests); they are never interpreted.
pub fn parse_connectionless(payload: &[u8]) -> Result<Connectionless<'_>> {
    let mut r = ByteReader::new(payload);
    let k = r.u8()?;
    let q = match k {
        kind::A2S_INFO => {
            let s = r.bytes(INFO_QUERY_STRING.len())?;
            if s != INFO_QUERY_STRING {
                return Err(ProtocolError::InvalidField {
                    field: "a2s_info",
                    reason: "bad query string",
                });
            }
            let challenge = if r.remaining() >= 4 {
                Some(r.i32()?)
            } else {
                None
            };
            QueryRequest::Info { challenge }
        }
        kind::A2S_PLAYER => QueryRequest::Player {
            challenge: r.i32()?,
        },
        kind::A2S_RULES => QueryRequest::Rules {
            challenge: r.i32()?,
        },
        kind::A2S_SERVERQUERY_GETCHALLENGE => QueryRequest::ServerQueryGetChallenge,
        other => {
            return Ok(Connectionless::Unhandled {
                kind: other,
                body: r.rest(),
            })
        }
    };
    Ok(Connectionless::Query(q))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_headers() {
        assert!(matches!(
            classify(&[0xFF, 0xFF, 0xFF, 0xFF, b'T']).unwrap(),
            Framing::Connectionless(&[b'T'])
        ));
        assert!(matches!(
            classify(&[0xFE, 0xFF, 0xFF, 0xFF, 0]).unwrap(),
            Framing::Split(_)
        ));
        assert!(matches!(
            classify(&[1, 0, 0, 0]).unwrap(),
            Framing::Netchannel(_)
        ));
        assert!(classify(&[0xFF, 0xFF]).is_err());
        assert!(classify(&[]).is_err());
    }

    #[test]
    fn parse_info_with_and_without_challenge() {
        let mut p = vec![b'T'];
        p.extend_from_slice(INFO_QUERY_STRING);
        assert_eq!(
            parse_connectionless(&p).unwrap(),
            Connectionless::Query(QueryRequest::Info { challenge: None })
        );
        p.extend_from_slice(&0x1122_3344i32.to_le_bytes());
        assert_eq!(
            parse_connectionless(&p).unwrap(),
            Connectionless::Query(QueryRequest::Info {
                challenge: Some(0x1122_3344)
            })
        );
        // Padding tolerated.
        p.extend_from_slice(&[0u8; 100]);
        assert!(parse_connectionless(&p).is_ok());
    }

    #[test]
    fn malformed_requests() {
        assert!(parse_connectionless(b"").is_err());
        assert!(parse_connectionless(b"TSource Engine Quer").is_err());
        assert!(parse_connectionless(b"TSource Engine QuerX\0").is_err());
        assert!(parse_connectionless(b"U\xff\xff").is_err());
        assert!(parse_connectionless(b"V").is_err());
        assert_eq!(
            parse_connectionless(b"U\xff\xff\xff\xff").unwrap(),
            Connectionless::Query(QueryRequest::Player { challenge: -1 })
        );
        assert!(matches!(
            parse_connectionless(b"qabc").unwrap(),
            Connectionless::Unhandled { kind: b'q', .. }
        ));
    }
}
