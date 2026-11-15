//! Net messages carried inside netchannel bit streams (6-bit type, then fields).
//!
//! Type numbers follow the public Source message numbering; GMod-specific layouts
//! (`svc_ServerInfo` trailer, `svc_CreateStringTable` header, `net_File`, the GMod
//! messages 18/33) were established from captures — see `docs/PROTOCOL.md`.

use crate::bitbuf::{BitReader, BitWriter};
use crate::error::{ProtocolError, Result};
use crate::netchan::NETMSG_TYPE_BITS;

pub const NET_NOP: u32 = 0;
pub const NET_DISCONNECT: u32 = 1;
pub const NET_FILE: u32 = 2;
pub const NET_TICK: u32 = 3;
pub const NET_STRINGCMD: u32 = 4;
pub const NET_SETCONVAR: u32 = 5;
pub const NET_SIGNONSTATE: u32 = 6;

pub const SVC_PRINT: u32 = 7;
pub const SVC_SERVERINFO: u32 = 8;
pub const SVC_SENDTABLE: u32 = 9;
pub const SVC_CLASSINFO: u32 = 10;
pub const SVC_SETPAUSE: u32 = 11;
pub const SVC_CREATESTRINGTABLE: u32 = 12;
pub const SVC_UPDATESTRINGTABLE: u32 = 13;
pub const SVC_VOICEINIT: u32 = 14;
pub const SVC_SETVIEW: u32 = 18;
pub const SVC_FIXANGLE: u32 = 19;
pub const SVC_USERMESSAGE: u32 = 23;
pub const SVC_PACKETENTITIES: u32 = 26;
pub const SVC_GAMEEVENTLIST: u32 = 30;
pub const SVC_GMOD_SERVERTOCLIENT: u32 = 33;

pub const CLC_CLIENTINFO: u32 = 8;
pub const CLC_MOVE: u32 = 9;
pub const CLC_VOICEDATA: u32 = 10;
pub const CLC_BASELINEACK: u32 = 11;
pub const CLC_LISTENEVENTS: u32 = 12;
pub const CLC_RESPONDCVARVALUE: u32 = 13;
pub const CLC_FILECRCCHECK: u32 = 14;
pub const CLC_SAVEREPLAY: u32 = 15;
pub const CLC_CMDKEYVALUES: u32 = 16;
pub const CLC_FILEMD5CHECK: u32 = 17;
pub const CLC_GMOD_CLIENTTOSERVER: u32 = 18;

pub const SIGNONSTATE_NONE: u8 = 0;
pub const SIGNONSTATE_CHALLENGE: u8 = 1;
pub const SIGNONSTATE_CONNECTED: u8 = 2;
pub const SIGNONSTATE_NEW: u8 = 3;
pub const SIGNONSTATE_PRESPAWN: u8 = 4;
pub const SIGNONSTATE_SPAWN: u8 = 5;
pub const SIGNONSTATE_FULL: u8 = 6;

/// GMod entity index width (8192 edicts).
pub const MAX_EDICT_BITS: u32 = 13;
/// GMod string-table user-data length width (from capture).
pub const STRINGTABLE_USERDATA_BITS: u32 = 19;
/// GMod string-table count (32 tables → 5-bit table id).
pub const MAX_TABLES_BITS: u32 = 5;

const MAX_STRING: usize = 4096;

/// Messages a client sends that the server understands.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    Nop,
    Disconnect(String),
    File {
        transfer_id: u32,
    },
    Tick {
        tick: i32,
    },
    StringCmd(String),
    SetConVar(Vec<(String, String)>),
    SignonState {
        state: u8,
        spawn_count: i32,
    },
    ClientInfo {
        server_count: i32,
        send_table_crc: u32,
        is_hltv: bool,
        friends_id: u32,
        friends_name: String,
        custom_files: [u32; 4],
    },
    Move {
        new_commands: u32,
        backup_commands: u32,
        bits: usize,
        /// The delta-encoded user commands (`bits` bits), oldest first.
        data: Vec<u8>,
    },
    VoiceData {
        bits: usize,
    },
    BaselineAck {
        tick: i32,
        nr: u32,
    },
    ListenEvents,
    RespondCvarValue {
        cookie: i32,
        status: i32,
        name: String,
        value: String,
    },
    CmdKeyValues {
        bytes: usize,
    },
    /// GMod packet: first byte = type (0 = Lua net message: u16 network-string id
    /// then the message body; 2 = client Lua error text).
    GModClientToServer {
        bits: usize,
        data: Vec<u8>,
    },
}

/// Decodes all messages in `r` until fewer than 6 bits remain. On an unknown or
/// malformed message, returns the messages decoded so far plus the error.
pub fn decode_client_messages(
    r: &mut BitReader<'_>,
) -> (Vec<ClientMessage>, Option<ProtocolError>) {
    let mut out = Vec::new();
    while r.bits_remaining() >= NETMSG_TYPE_BITS as usize {
        match decode_client_message(r) {
            Ok(Some(ClientMessage::Nop)) => {}
            Ok(Some(m)) => out.push(m),
            Ok(None) => break,
            Err(e) => return (out, Some(e)),
        }
    }
    (out, None)
}

fn skip(r: &mut BitReader<'_>, bits: usize) -> Result<usize> {
    r.skip_bits(bits)?;
    Ok(bits)
}

/// Copies `bits` bits out of `r` into a byte buffer (LSB-first, zero padded).
pub fn take_bits(r: &mut BitReader<'_>, bits: usize) -> Result<Vec<u8>> {
    if bits > r.bits_remaining() {
        return Err(ProtocolError::Truncated {
            needed: bits,
            remaining: r.bits_remaining(),
        });
    }
    let mut out = Vec::with_capacity(bits.div_ceil(8));
    let mut left = bits;
    while left > 0 {
        let n = left.min(8) as u32;
        out.push(r.read_ubits(n)? as u8);
        left -= n as usize;
    }
    Ok(out)
}

fn decode_client_message(r: &mut BitReader<'_>) -> Result<Option<ClientMessage>> {
    let t = r.read_ubits(NETMSG_TYPE_BITS)?;
    Ok(Some(match t {
        NET_NOP => ClientMessage::Nop,
        NET_DISCONNECT => ClientMessage::Disconnect(r.read_string(MAX_STRING)?),
        NET_FILE => {
            let transfer_id = r.read_u32()?;
            let _flag = r.read_bit()?;
            let custom = r.read_bit()?;
            if custom {
                r.read_u32()?;
            } else {
                r.read_string(MAX_STRING)?;
            }
            ClientMessage::File { transfer_id }
        }
        NET_TICK => {
            let tick = r.read_sbits(32)?;
            r.read_ubits(16)?;
            r.read_ubits(16)?;
            ClientMessage::Tick { tick }
        }
        NET_STRINGCMD => ClientMessage::StringCmd(r.read_string(MAX_STRING)?),
        NET_SETCONVAR => {
            let n = r.read_u8()? as usize;
            let mut v = Vec::with_capacity(n.min(r.bits_remaining() / 16));
            for _ in 0..n {
                let k = r.read_string(MAX_STRING)?;
                let val = r.read_string(MAX_STRING)?;
                v.push((k, val));
            }
            ClientMessage::SetConVar(v)
        }
        NET_SIGNONSTATE => ClientMessage::SignonState {
            state: r.read_u8()?,
            spawn_count: r.read_sbits(32)?,
        },
        CLC_CLIENTINFO => {
            let server_count = r.read_sbits(32)?;
            let send_table_crc = r.read_u32()?;
            let is_hltv = r.read_bit()?;
            let friends_id = r.read_u32()?;
            let friends_name = r.read_string(MAX_STRING)?;
            let mut custom_files = [0u32; 4];
            for c in custom_files.iter_mut() {
                if r.read_bit()? {
                    *c = r.read_u32()?;
                }
            }
            ClientMessage::ClientInfo {
                server_count,
                send_table_crc,
                is_hltv,
                friends_id,
                friends_name,
                custom_files,
            }
        }
        CLC_MOVE => {
            let new_commands = r.read_ubits(4)?;
            let backup_commands = r.read_ubits(3)?;
            let len = r.read_u16()? as usize;
            ClientMessage::Move {
                new_commands,
                backup_commands,
                bits: len,
                data: take_bits(r, len)?,
            }
        }
        CLC_VOICEDATA => {
            let len = r.read_u16()? as usize;
            ClientMessage::VoiceData {
                bits: skip(r, len)?,
            }
        }
        CLC_BASELINEACK => ClientMessage::BaselineAck {
            tick: r.read_sbits(32)?,
            nr: r.read_ubits(1)?,
        },
        CLC_LISTENEVENTS => {
            // 512 event bits (16 x u32)
            r.skip_bits(512)?;
            ClientMessage::ListenEvents
        }
        CLC_RESPONDCVARVALUE => ClientMessage::RespondCvarValue {
            cookie: r.read_sbits(32)?,
            status: r.read_sbits(4)?,
            name: r.read_string(MAX_STRING)?,
            value: r.read_string(MAX_STRING)?,
        },
        CLC_CMDKEYVALUES => {
            let n = r.read_u32()? as usize;
            ClientMessage::CmdKeyValues {
                bytes: skip(
                    r,
                    n.checked_mul(8).ok_or(ProtocolError::InvalidField {
                        field: "cmdkeyvalues",
                        reason: "length overflow",
                    })?,
                )? / 8,
            }
        }
        CLC_GMOD_CLIENTTOSERVER => {
            let len = r.read_ubits(20)? as usize;
            ClientMessage::GModClientToServer {
                bits: len,
                data: take_bits(r, len)?,
            }
        }
        other => return Err(ProtocolError::UnknownType(other as u8)),
    }))
}

// ----------------------------------------------------------------------------------
// Server → client encoders. Each appends one message (type + fields) to `w`.
// ----------------------------------------------------------------------------------

fn ty(w: &mut BitWriter, t: u32) -> Result<()> {
    w.write_ubits(t, NETMSG_TYPE_BITS)
}

pub fn write_print(w: &mut BitWriter, text: &str) -> Result<()> {
    ty(w, SVC_PRINT)?;
    w.write_string(text)
}

pub fn write_disconnect(w: &mut BitWriter, reason: &str) -> Result<()> {
    ty(w, NET_DISCONNECT)?;
    w.write_string(reason)
}

pub fn write_tick(w: &mut BitWriter, tick: i32, host_frametime: f32, stddev: f32) -> Result<()> {
    // Frame times are sent scaled by NET_TICK_SCALEUP (100000) in 16 bits.
    let scale = |v: f32| ((v * 100_000.0).clamp(0.0, 65_535.0)) as u32;
    ty(w, NET_TICK)?;
    w.write_sbits(tick, 32)?;
    w.write_ubits(scale(host_frametime), 16)?;
    w.write_ubits(scale(stddev), 16)
}

pub fn write_string_cmd(w: &mut BitWriter, cmd: &str) -> Result<()> {
    ty(w, NET_STRINGCMD)?;
    w.write_string(cmd)
}

pub fn write_set_convars(w: &mut BitWriter, vars: &[(&str, &str)]) -> Result<()> {
    if vars.len() > 255 {
        return Err(ProtocolError::InvalidField {
            field: "setconvar",
            reason: "more than 255 vars",
        });
    }
    ty(w, NET_SETCONVAR)?;
    w.write_u8(vars.len() as u8)?;
    for (k, v) in vars {
        w.write_string(k)?;
        w.write_string(v)?;
    }
    Ok(())
}

pub fn write_signon_state(w: &mut BitWriter, state: u8, spawn_count: i32) -> Result<()> {
    ty(w, NET_SIGNONSTATE)?;
    w.write_u8(state)?;
    w.write_sbits(spawn_count, 32)
}

/// `svc_ServerInfo` with the GMod trailer (two strings: observed empty string, then the
/// gamemode name).
#[derive(Debug, Clone)]
pub struct ServerInfo<'a> {
    pub protocol: i16,
    pub server_count: i32,
    pub is_hltv: bool,
    pub is_dedicated: bool,
    pub client_crc: u32,
    pub max_classes: u16,
    pub map_md5: [u8; 16],
    pub player_slot: u8,
    pub max_clients: u8,
    pub tick_interval: f32,
    pub os: u8,
    pub game_dir: &'a str,
    pub map_name: &'a str,
    pub sky_name: &'a str,
    pub host_name: &'a str,
    pub gmod_extra: &'a str,
    pub gamemode: &'a str,
}

pub fn write_server_info(w: &mut BitWriter, s: &ServerInfo<'_>) -> Result<()> {
    ty(w, SVC_SERVERINFO)?;
    w.write_sbits(s.protocol as i32, 16)?;
    w.write_sbits(s.server_count, 32)?;
    w.write_bit(s.is_hltv)?;
    w.write_bit(s.is_dedicated)?;
    w.write_u32(s.client_crc)?;
    w.write_u16(s.max_classes)?;
    w.write_bytes(&s.map_md5)?;
    w.write_u8(s.player_slot)?;
    w.write_u8(s.max_clients)?;
    w.write_f32(s.tick_interval)?;
    w.write_u8(s.os)?;
    w.write_string(s.game_dir)?;
    w.write_string(s.map_name)?;
    w.write_string(s.sky_name)?;
    w.write_string(s.host_name)?;
    w.write_string(s.gmod_extra)?;
    w.write_string(s.gamemode)?;
    // [факт] After the two GMod strings the client's svc_ServerInfo reader consumes a
    // further trailer: a live client reaches SignonState(FULL) only when at least 11
    // extra bits are present (10 or fewer → the read overflows and the client aborts
    // the signon). The reference server left these bits set (its 18 trailing bits after
    // the gamemode string were all ones). Their meaning is unestablished [гипотеза]; we
    // send zeroes, which the client accepts, and any bits it does not consume read back
    // as net_NOP. 16 is a safe round width ≥ the observed threshold.
    w.write_ubits(0, SERVERINFO_GMOD_TRAILER_BITS)?;
    Ok(())
}

/// Width of the GMod-specific `svc_ServerInfo` trailer (see `write_server_info`).
pub const SERVERINFO_GMOD_TRAILER_BITS: u32 = 16;

/// `svc_ClassInfo` telling the client to build class tables from its own DLL.
pub fn write_class_info_create_on_client(w: &mut BitWriter, num_classes: u16) -> Result<()> {
    ty(w, SVC_CLASSINFO)?;
    w.write_u16(num_classes)?;
    w.write_bit(true)
}

pub fn write_voice_init(w: &mut BitWriter, codec: &str, quality: u8) -> Result<()> {
    ty(w, SVC_VOICEINIT)?;
    w.write_string(codec)?;
    w.write_u8(quality)
}

/// `svc_GameEventList` with no event descriptors.
pub fn write_empty_game_event_list(w: &mut BitWriter) -> Result<()> {
    ty(w, SVC_GAMEEVENTLIST)?;
    w.write_ubits(0, 9)?;
    w.write_ubits(0, 20)
}

pub fn write_set_view(w: &mut BitWriter, entity: u32) -> Result<()> {
    ty(w, SVC_SETVIEW)?;
    w.write_ubits(entity, MAX_EDICT_BITS)
}

/// `svc_FixAngle` with angles in degrees.
pub fn write_fix_angle(w: &mut BitWriter, relative: bool, angles: [f32; 3]) -> Result<()> {
    ty(w, SVC_FIXANGLE)?;
    w.write_bit(relative)?;
    for a in angles {
        let v = ((a.rem_euclid(360.0) / 360.0) * 65536.0) as u32 & 0xFFFF;
        w.write_ubits(v, 16)?;
    }
    Ok(())
}

/// GMod message 33 with an opaque payload of `bits` bits.
pub fn write_gmod_server_to_client(w: &mut BitWriter, payload: &[u8], bits: usize) -> Result<()> {
    ty(w, SVC_GMOD_SERVERTOCLIENT)?;
    w.write_ubits(bits as u32, 20)?;
    w.append_bits(payload, bits)
}

/// `svc_UserMessage`: [факт] u8 message type, 11-bit length in bits, data (every
/// packet of the reference demo with user messages parses exactly with this layout).
pub fn write_user_message(w: &mut BitWriter, msg_type: u8, data: &[u8], bits: usize) -> Result<()> {
    if bits >= 1 << 11 {
        return Err(crate::error::ProtocolError::Oversized { size: bits / 8, limit: 255 });
    }
    ty(w, SVC_USERMESSAGE)?;
    w.write_ubits(msg_type as u32, 8)?;
    w.write_ubits(bits as u32, 11)?;
    w.append_bits(data, bits)
}

/// User message ids in the game DLL's registration order. [факт] order read from the
/// reference server; sizes of `Train` (1 byte), `ResetHUD` (1) and `Battery` (2) match
/// the reference demo.
pub const UM_SAYTEXT: u8 = 3;
pub const UM_TEXTMSG: u8 = 4;

/// `svc_PacketEntities` header + raw entity data.
#[derive(Debug, Clone)]
pub struct PacketEntities<'a> {
    pub max_entries: u32,
    pub delta_from: Option<i32>,
    pub baseline: u32,
    pub updated_entries: u32,
    pub update_baseline: bool,
    pub data: &'a [u8],
    pub data_bits: usize,
}

/// Width of the `svc_PacketEntities` payload length. [факт] GMod uses 24 bits (stock
/// Source: 20): with 24, every reference-server packet in a recorded demo decodes to
/// exactly the declared length; with 20 none does (4 bits left over).
pub const PACKETENTITIES_LENGTH_BITS: u32 = 24;

pub fn write_packet_entities(w: &mut BitWriter, p: &PacketEntities<'_>) -> Result<()> {
    if p.data_bits >= 1 << PACKETENTITIES_LENGTH_BITS {
        return Err(ProtocolError::InvalidField {
            field: "packetentities",
            reason: "payload too long",
        });
    }
    ty(w, SVC_PACKETENTITIES)?;
    w.write_ubits(p.max_entries, MAX_EDICT_BITS)?;
    w.write_bit(p.delta_from.is_some())?;
    if let Some(f) = p.delta_from {
        w.write_sbits(f, 32)?;
    }
    w.write_ubits(p.baseline, 1)?;
    w.write_ubits(p.updated_entries, MAX_EDICT_BITS)?;
    w.write_ubits(p.data_bits as u32, PACKETENTITIES_LENGTH_BITS)?;
    w.write_bit(p.update_baseline)?;
    w.append_bits(p.data, p.data_bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signon_and_setconvar_decode() {
        let mut w = BitWriter::with_capacity(256);
        write_set_convars(&mut w, &[("name", "player"), ("rate", "80000")]).unwrap();
        write_signon_state(&mut w, 2, -1).unwrap();
        let b = w.into_bytes();
        let mut r = BitReader::new(&b);
        let (m, e) = decode_client_messages(&mut r);
        assert!(e.is_none(), "{e:?}");
        assert_eq!(
            m,
            vec![
                ClientMessage::SetConVar(vec![
                    ("name".into(), "player".into()),
                    ("rate".into(), "80000".into())
                ]),
                ClientMessage::SignonState {
                    state: 2,
                    spawn_count: -1
                }
            ]
        );
    }

    #[test]
    fn client_info_and_gmod_message() {
        let mut w = BitWriter::with_capacity(256);
        w.write_ubits(CLC_CLIENTINFO, 6).unwrap();
        w.write_sbits(1, 32).unwrap();
        w.write_u32(0x546b_7a01).unwrap();
        w.write_bit(false).unwrap();
        w.write_u32(42).unwrap();
        w.write_string("").unwrap();
        w.write_bit(true).unwrap();
        w.write_u32(0xd726_a068).unwrap();
        for _ in 0..3 {
            w.write_bit(false).unwrap();
        }
        w.write_ubits(CLC_GMOD_CLIENTTOSERVER, 6).unwrap();
        w.write_ubits(24, 20).unwrap();
        w.write_ubits(0x00_0004, 24).unwrap();
        write_signon_state(&mut w, 3, 1).unwrap();
        let b = w.into_bytes();
        let (m, e) = decode_client_messages(&mut BitReader::new(&b));
        assert!(e.is_none());
        assert_eq!(m.len(), 3);
        assert!(matches!(
            m[0],
            ClientMessage::ClientInfo {
                custom_files: [0xd726_a068, 0, 0, 0],
                ..
            }
        ));
        assert!(matches!(&m[1], ClientMessage::GModClientToServer { bits: 24, data } if data.len() == 3));
    }

    #[test]
    fn unknown_and_truncated() {
        let mut w = BitWriter::with_capacity(8);
        w.write_ubits(63, 6).unwrap();
        let b = w.into_bytes();
        let (_, e) = decode_client_messages(&mut BitReader::new(&b));
        assert!(e.is_some());
        let mut w = BitWriter::with_capacity(64);
        write_set_convars(&mut w, &[("a", "b")]).unwrap();
        let b = w.into_bytes();
        for cut in 0..b.len() {
            let _ = decode_client_messages(&mut BitReader::new(&b[..cut]));
        }
        // hostile length in move
        let mut w = BitWriter::with_capacity(64);
        w.write_ubits(CLC_MOVE, 6).unwrap();
        w.write_ubits(1, 4).unwrap();
        w.write_ubits(0, 3).unwrap();
        w.write_u16(60000).unwrap();
        let b = w.into_bytes();
        assert!(decode_client_messages(&mut BitReader::new(&b)).1.is_some());
    }

    #[test]
    fn server_info_layout_matches_reference() {
        // Field values of the captured reference svc_ServerInfo; the encoder must
        // reproduce the same bit length (6-bit type + fields = 752 bits there).
        let s = ServerInfo {
            protocol: 24,
            server_count: 1,
            is_hltv: false,
            is_dedicated: true,
            client_crc: 0xFFFF_FFFF,
            max_classes: 250,
            map_md5: [0; 16],
            player_slot: 0,
            max_clients: 8,
            tick_interval: 0.015,
            os: b'w',
            game_dir: "garrysmod",
            map_name: "gm_construct",
            sky_name: "painted",
            host_name: "srcds-ref vanilla",
            gmod_extra: "",
            gamemode: "sandbox",
        };
        let mut w = BitWriter::with_capacity(512);
        write_server_info(&mut w, &s).unwrap();
        // stock + 2 GMod strings + the 16-bit GMod trailer.
        assert_eq!(
            w.bit_len(),
            (1902 - 1150) + SERVERINFO_GMOD_TRAILER_BITS as usize
        );
    }
}
