//! Player input commands carried by `clc_Move`.
//!
//! Each command is delta-encoded against the previous one in the same message (the
//! first against an all-zero command): every field is preceded by a "changed" bit.
//! Field order and widths of the common part follow the public Source layout; the
//! GMod-specific tail was established from live traffic of the unmodified client
//! (2026-11-14: [факт] all 16 261 captured `clc_Move` messages — walking, sprinting,
//! crouching, mouse wheel, holding the context-menu key — decode to exactly their
//! declared length):
//!
//! * `[changed][u8 button code]* 0` — the list of held input codes (`KEY_*` /
//!   `MOUSE_*` button codes; e.g. W = 33, D = 14, TAB = 67, MOUSE_LEFT = 107 matched
//!   the `buttons` bits sent alongside), terminated by a zero code;
//! * `[changed][i8]` — mouse wheel (±1/±2 while scrolling);
//! * `[bit]` — set while the context-menu key is held (world clicking; a plain value,
//!   not delta-encoded) [гипотеза: `IsWorldClicking`];
//! * `[changed][3 × i16]` — the cursor direction while world clicking, a unit vector
//!   ×1000 ([факт] captured with the cursor at the screen centre: (−957, −289, 20) =
//!   the view direction of pitch −1.15°, yaw −163.19°);
//! * `[changed][3 × f32]`, `[changed][3 × f32]` — never seen set, kept raw;
//! * `[changed]` of a sixth field never seen set; reported as
//!   [`ProtocolError::InvalidField`] so callers stop at that point.

use crate::bitbuf::BitReader;
use crate::error::{ProtocolError, Result};
use crate::netmsg::MAX_EDICT_BITS;

const WEAPON_SUBTYPE_BITS: u32 = 6;
/// Upper bound on held-key codes accepted per command.
/// Held-key list capacity. [факт] a full list (5 keys) has no 0 terminator: captured
/// packets with R, W, Space, LShift and Mouse1 held only decode exactly this way.
const MAX_KEYS: usize = 5;

/// Button bits (`IN_*`) the server interprets.
pub const IN_JUMP: u32 = 1 << 1;
pub const IN_DUCK: u32 = 1 << 2;
pub const IN_FORWARD: u32 = 1 << 3;
pub const IN_SPEED: u32 = 1 << 17;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserCmd {
    pub command_number: i32,
    pub tick_count: i32,
    /// Pitch, yaw, roll in degrees.
    pub view_angles: [f32; 3],
    pub forward_move: f32,
    pub side_move: f32,
    pub up_move: f32,
    pub buttons: u32,
    pub impulse: u8,
    pub weapon_select: u32,
    pub weapon_subtype: u32,
    pub mouse_dx: i16,
    pub mouse_dy: i16,
    /// GMod: button codes currently held.
    pub held_keys: Vec<u8>,
    /// GMod: mouse wheel delta.
    pub mouse_wheel: i8,
    /// GMod: set while the context-menu key is held (world clicking).
    pub world_clicking: bool,
    /// GMod world-clicking data, kept raw (meaning not established).
    pub gmod_raw48: u64,
    /// Plain flag after the 48-bit field (no change bit). [факт] set in every
    /// command while the chat box is open (37/37 captured packets); meaning
    /// [гипотеза] "typing".
    pub gmod_flag: bool,
    pub gmod_vec_a: [f32; 3],
    pub gmod_vec_b: [f32; 3],
}

/// The world-click cursor direction (normalised) from the 48-bit field.
pub fn world_click_direction(raw48: u64) -> Option<[f32; 3]> {
    let c = |i: u32| ((raw48 >> (16 * i)) & 0xFFFF) as u16 as i16 as f32;
    let v = [c(0), c(1), c(2)];
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    (len > 1.0).then(|| [v[0] / len, v[1] / len, v[2] / len])
}

fn changed(r: &mut BitReader<'_>) -> Result<bool> {
    r.read_bit()
}

/// Reads one command delta-encoded against `from`.
pub fn read_usercmd(r: &mut BitReader<'_>, from: &UserCmd) -> Result<UserCmd> {
    let mut c = from.clone();
    c.command_number = if changed(r)? {
        r.read_u32()? as i32
    } else {
        from.command_number.wrapping_add(1)
    };
    c.tick_count = if changed(r)? {
        r.read_u32()? as i32
    } else {
        from.tick_count.wrapping_add(1)
    };
    for a in c.view_angles.iter_mut() {
        if changed(r)? {
            *a = r.read_f32()?;
        }
    }
    for m in [&mut c.forward_move, &mut c.side_move, &mut c.up_move] {
        if changed(r)? {
            *m = r.read_f32()?;
        }
    }
    if changed(r)? {
        c.buttons = r.read_u32()?;
    }
    if changed(r)? {
        c.impulse = r.read_u8()?;
    }
    if changed(r)? {
        c.weapon_select = r.read_ubits(MAX_EDICT_BITS)?;
        if changed(r)? {
            c.weapon_subtype = r.read_ubits(WEAPON_SUBTYPE_BITS)?;
        }
    }
    if changed(r)? {
        c.mouse_dx = r.read_ubits(16)? as u16 as i16;
    }
    if changed(r)? {
        c.mouse_dy = r.read_ubits(16)? as u16 as i16;
    }
    if changed(r)? {
        c.held_keys.clear();
        while c.held_keys.len() < MAX_KEYS {
            let k = r.read_u8()?;
            if k == 0 {
                break;
            }
            c.held_keys.push(k);
        }
    }
    if changed(r)? {
        c.mouse_wheel = r.read_u8()? as i8;
    }
    c.world_clicking = r.read_bit()?;
    if changed(r)? {
        let lo = r.read_u32()? as u64;
        let hi = r.read_ubits(16)? as u64;
        c.gmod_raw48 = lo | hi << 32;
    }
    c.gmod_flag = r.read_bit()?;
    for v in [&mut c.gmod_vec_a, &mut c.gmod_vec_b] {
        if changed(r)? {
            for x in v.iter_mut() {
                *x = r.read_f32()?;
            }
        }
    }
    Ok(c)
}

/// Decodes the commands of one `clc_Move` (oldest first). On a command that cannot be
/// decoded, returns the ones before it together with the error.
pub fn read_move(data: &[u8], bits: usize, count: u32) -> (Vec<UserCmd>, Option<ProtocolError>) {
    let mut r = match BitReader::with_bit_len(data, bits) {
        Ok(r) => r,
        Err(e) => return (Vec::new(), Some(e)),
    };
    let mut out: Vec<UserCmd> = Vec::with_capacity(count.min(32) as usize);
    let null = UserCmd::default();
    for _ in 0..count.min(32) {
        let from = out.last().unwrap_or(&null);
        match read_usercmd(&mut r, from) {
            Ok(c) => out.push(c),
            Err(e) => return (out, Some(e)),
        }
    }
    (out, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(cmds: &[UserCmd]) -> (Vec<u8>, usize) {
        use crate::bitbuf::BitWriter;
        let mut w = BitWriter::with_capacity(1024);
        let null = UserCmd::default();
        let mut prev = &null;
        for c in cmds {
            let num = c.command_number != prev.command_number.wrapping_add(1);
            w.write_bit(num).unwrap();
            if num {
                w.write_u32(c.command_number as u32).unwrap();
            }
            let tick = c.tick_count != prev.tick_count.wrapping_add(1);
            w.write_bit(tick).unwrap();
            if tick {
                w.write_u32(c.tick_count as u32).unwrap();
            }
            let fl = [
                (c.view_angles[0], prev.view_angles[0]),
                (c.view_angles[1], prev.view_angles[1]),
                (c.view_angles[2], prev.view_angles[2]),
                (c.forward_move, prev.forward_move),
                (c.side_move, prev.side_move),
                (c.up_move, prev.up_move),
            ];
            for (a, b) in fl {
                w.write_bit(a != b).unwrap();
                if a != b {
                    w.write_f32(a).unwrap();
                }
            }
            w.write_bit(c.buttons != prev.buttons).unwrap();
            if c.buttons != prev.buttons {
                w.write_u32(c.buttons).unwrap();
            }
            w.write_bit(false).unwrap(); // impulse
            w.write_bit(false).unwrap(); // weapon
            w.write_bit(false).unwrap(); // mouse dx
            w.write_bit(false).unwrap(); // mouse dy
            let keys = c.held_keys != prev.held_keys;
            w.write_bit(keys).unwrap();
            if keys {
                for k in &c.held_keys {
                    w.write_u8(*k).unwrap();
                }
                w.write_u8(0).unwrap();
            }
            w.write_ubits(0, 6).unwrap();
            prev = c;
        }
        let n = w.bit_len();
        (w.into_bytes(), n)
    }

    #[test]
    fn idle_commands_are_20_bits_each() {
        // An unchanged command costs 13 change bits + key-list bit + 6 flags.
        let a = UserCmd {
            command_number: 97,
            tick_count: 680,
            view_angles: [0.0, 180.0, 0.0],
            ..Default::default()
        };
        let mut b = a.clone();
        b.command_number += 1;
        b.tick_count += 1;
        let mut c = b.clone();
        c.command_number += 1;
        c.tick_count += 1;
        let (d, n) = encode(&[a.clone(), b.clone(), c.clone()]);
        // [факт] the client sends exactly 156 bits for this shape.
        assert_eq!(n, 156);
        let (got, err) = read_move(&d, n, 3);
        assert!(err.is_none());
        assert_eq!(got, [a, b, c]);
    }

    #[test]
    fn held_keys_and_movement() {
        let a = UserCmd {
            command_number: 128,
            tick_count: 711,
            view_angles: [-0.783, 177.333, 0.0],
            side_move: 10000.0,
            buttons: (1 << 10) | (1 << 16),
            held_keys: vec![14, 67],
            ..Default::default()
        };
        let (d, n) = encode(std::slice::from_ref(&a));
        let (got, err) = read_move(&d, n, 1);
        assert!(err.is_none());
        assert_eq!(got[0], a);
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Captured from the unmodified client while holding the context-menu key and
    /// running (inputs only).
    #[test]
    fn world_clicking_messages_decode_exactly() {
        for (hex, bits) in [
            ("55080000e2170000c44e97fcbd0933322c0010875100200e6311040400e04243c8130000007888ffdb9f0200008000", 372),
            ("730800001e180000c44e97fcbd0933322c0010875100200e6311040400e04243c8138087f8bffd290000007888ffdb3f0200008083f8cdfd1f0000", 468),
        ] {
            let d = unhex(hex);
            let mut r = BitReader::with_bit_len(&d, bits).unwrap();
            let null = UserCmd::default();
            let mut prev = null.clone();
            for _ in 0..3 {
                prev = read_usercmd(&mut r, &prev).unwrap();
            }
            assert_eq!(r.bits_remaining(), 0, "{hex}");
            assert!(prev.world_clicking);
            // the cursor was at the screen centre: the view direction
            let d = world_click_direction(prev.gmod_raw48).unwrap();
            let (p, y) = (prev.view_angles[0].to_radians(), prev.view_angles[1].to_radians());
            let f = [p.cos() * y.cos(), p.cos() * y.sin(), -p.sin()];
            assert!((0..3).all(|i| (d[i] - f[i]).abs() < 0.01), "{d:?} {f:?}");
            assert_eq!(prev.forward_move, 10000.0);
            // W, D? and shift held: keys present
            assert!(!prev.held_keys.is_empty());
        }
    }

    /// Captured with five keys held (R, W, Space, LShift, Mouse1): the full list has
    /// no terminator; three commands consume the data exactly.
    #[test]
    fn five_held_keys_have_no_terminator() {
        for (hex, bits) in [
            ("bd090000f65200004c1f7e08aad06c152c00108711134004002047c8d31a000070014400007284043dad01000000", 364),
            ("bb090000f25200004c1f7e08aad06c152c00108711134004002047c8d31a000000000017400400204748d0d31a00", 364),
            ("890900008e520000acf4b204ba27e91a240010875100200e6313040400c8011002e042c8d31a00c02b6451a02907a8418020001d0000a4c1b1050ab7be190413140400f803b002e042c8d31a1b00", 620),
        ] {
            let d = unhex(hex);
            let mut r = BitReader::with_bit_len(&d, bits).unwrap();
            let mut prev = UserCmd::default();
            for _ in 0..3 {
                prev = read_usercmd(&mut r, &prev).unwrap();
            }
            assert_eq!(r.bits_remaining(), 0, "{hex}");
        }
    }

    /// Captured while the chat box was open: every command carries the plain GMod
    /// flag after the 48-bit field; three commands consume the data exactly.
    #[test]
    fn chat_open_messages_decode_exactly() {
        for (hex, bits) in [
            ("f70d0000e2390000541120103acddc320c00020020000002", 188),
            ("010e0000f6390000541120103acddc320c0082fcbf2dc380feff0201000008000000", 268),
            ("ff0d0000f2390000541120103acddc320c00020020c8ffdb320ce8ff2f100000", 252),
        ] {
            let d = unhex(hex);
            let (got, err) = read_move(&d, bits, 3);
            assert!(err.is_none(), "{hex}: {err:?}");
            assert_eq!(got.len(), 3);
            // the last sample was sent as the chat closed: the flag clears
            assert!(got[0].gmod_flag, "{hex}");
            let mut r = BitReader::with_bit_len(&d, bits).unwrap();
            let mut prev = UserCmd::default();
            for _ in 0..3 {
                prev = read_usercmd(&mut r, &prev).unwrap();
            }
            assert_eq!(r.bits_remaining(), 0, "{hex}");
        }
    }

    #[test]
    fn truncated_vector_stops_decoding() {
        let (mut d, n) = encode(&[UserCmd::default()]);
        // set the last flag bit (bit n-1): a vector follows that is not there
        d[(n - 1) / 8] |= 1 << ((n - 1) % 8);
        let (got, err) = read_move(&d, n, 1);
        assert!(got.is_empty());
        assert!(err.is_some());
    }
}
