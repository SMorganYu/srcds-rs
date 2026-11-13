//! Netchannel: sequenced datagrams with a reliable, fragmented sub-stream.
//!
//! Layout of one (uncompressed, unsplit) datagram, as established from captures of the
//! GMod client and the reference GMod DS (`docs/PROTOCOL.md` §Netchannel):
//!
//! ```text
//! i32 sequence | i32 ack | u8 flags | u16 checksum | u16 reliable_state
//! [u8 choked (flags&0x10)] [u32 challenge (flags&0x20)] | bit stream ...
//! ```
//!
//! * `checksum` = CRC32 of every byte after the checksum field, folded to 16 bits
//!   (`lo ^ hi`) — verified on all captured packets.
//! * GMod uses 16 reliable sub-channels: a 16-bit reliable state and a 4-bit sub-channel
//!   index (the classic engine used 8 / 3 bits).
//! * Reliable payload framing per stream: `single-block` (1 bit = 0, compressed flag,
//!   varint length) or fragments of 4096 bytes (`start` 18 bits, `count` 5 bits; first
//!   fragment carries file flag, compressed flag and a 30-bit size).
//!
//! Sub-channel acknowledgement: the sender flips bit *i* of its out-state when it
//! starts transmitting sub-channel *i*; the receiver flips bit *i* of its in-state when
//! it has fully read that sub-channel and echoes the in-state in every header. A
//! sub-channel is acknowledged when both bits agree, and must be resent when the peer
//! acknowledged a later sequence without the bits agreeing.

use std::collections::VecDeque;

use crate::bitbuf::{BitReader, BitWriter};
use crate::error::{ProtocolError, Result};
use crate::lzss;

pub const PACKET_FLAG_RELIABLE: u8 = 1 << 0;
pub const PACKET_FLAG_CHOKED: u8 = 1 << 4;
pub const PACKET_FLAG_CHALLENGE: u8 = 1 << 5;

pub const NET_HEADER_FLAG_SPLIT: i32 = -2;
pub const NET_HEADER_FLAG_COMPRESSED: i32 = -3;

pub const NETMSG_TYPE_BITS: u32 = 6;
pub const SUBCHANNEL_BITS: u32 = 4;
pub const MAX_SUBCHANNELS: usize = 1 << SUBCHANNEL_BITS;
pub const MAX_STREAMS: usize = 2;
pub const MAX_FILE_SIZE_BITS: u32 = 30;
pub const FRAGMENT_BITS: u32 = 12;
pub const FRAGMENT_SIZE: usize = 1 << FRAGMENT_BITS;
pub const NUM_FRAGMENT_BITS: u32 = 5;

/// Fixed header size without the optional choked byte.
pub const HEADER_LEN: usize = 4 + 4 + 1 + 2 + 2 + 4;
/// Largest reliable transfer we accept from a peer (clients upload sprays ~100 KiB).
pub const MAX_INCOMING_TRANSFER: usize = 4 << 20;
/// Largest decompressed datagram we accept.
pub const MAX_DECOMPRESSED_DATAGRAM: usize = 256 << 10;

/// Standard reflected CRC-32 (IEEE 802.3 polynomial 0xEDB88320).
pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *e = c;
        }
        t
    });
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

/// 16-bit packet checksum: CRC32 folded as `low16 ^ high16`.
pub fn checksum16(data: &[u8]) -> u16 {
    let c = crc32(data);
    (c as u16) ^ ((c >> 16) as u16)
}

/// Undoes outer `-3` (LZSS) framing if present. `-2` split framing must be
/// reassembled before calling this.
pub fn unwrap_compressed(datagram: &[u8]) -> Result<Option<Vec<u8>>> {
    if datagram.len() >= 4
        && i32::from_le_bytes(datagram[..4].try_into().unwrap()) == NET_HEADER_FLAG_COMPRESSED
    {
        return lzss::decompress(&datagram[4..], MAX_DECOMPRESSED_DATAGRAM).map(Some);
    }
    Ok(None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketHeader {
    pub sequence: i32,
    pub ack: i32,
    pub flags: u8,
    pub reliable_state: u16,
    pub choked: u8,
    pub challenge: Option<u32>,
}

/// Parses and checksum-verifies a netchannel header. Returns the header and the byte
/// offset at which the bit stream starts.
pub fn parse_header(d: &[u8]) -> Result<(PacketHeader, usize)> {
    if d.len() < 4 + 4 + 1 + 2 + 2 {
        return Err(ProtocolError::Truncated {
            needed: 13,
            remaining: d.len(),
        });
    }
    let sequence = i32::from_le_bytes(d[0..4].try_into().unwrap());
    let ack = i32::from_le_bytes(d[4..8].try_into().unwrap());
    let flags = d[8];
    let ck = u16::from_le_bytes([d[9], d[10]]);
    if checksum16(&d[11..]) != ck {
        return Err(ProtocolError::InvalidField {
            field: "netchan",
            reason: "checksum mismatch",
        });
    }
    let reliable_state = u16::from_le_bytes([d[11], d[12]]);
    let mut p = 13;
    let mut choked = 0;
    if flags & PACKET_FLAG_CHOKED != 0 {
        choked = *d.get(p).ok_or(ProtocolError::Truncated {
            needed: 1,
            remaining: 0,
        })?;
        p += 1;
    }
    let mut challenge = None;
    if flags & PACKET_FLAG_CHALLENGE != 0 {
        let b = d.get(p..p + 4).ok_or(ProtocolError::Truncated {
            needed: 4,
            remaining: d.len().saturating_sub(p),
        })?;
        challenge = Some(u32::from_le_bytes(b.try_into().unwrap()));
        p += 4;
    }
    if sequence <= 0 {
        return Err(ProtocolError::InvalidField {
            field: "netchan",
            reason: "non-positive sequence",
        });
    }
    Ok((
        PacketHeader {
            sequence,
            ack,
            flags,
            reliable_state,
            choked,
            challenge,
        },
        p,
    ))
}

/// One outgoing reliable transfer (a byte-aligned buffer of net messages).
#[derive(Debug)]
struct OutTransfer {
    data: Vec<u8>,
    num_fragments: usize,
    /// Fragments acknowledged so far (always a prefix).
    acked: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubState {
    ToSend,
    Waiting { send_seq: i32 },
}

#[derive(Debug, Clone, Copy)]
struct SubChannel {
    index: usize,
    start: usize,
    count: usize,
    state: SubState,
}

#[derive(Debug, Default)]
struct InTransfer {
    buf: Vec<u8>,
    size: usize,
    num_fragments: usize,
    received: Vec<bool>,
    compressed_size: Option<usize>,
    is_file: bool,
}

/// What a processed packet yielded.
#[derive(Debug, Default)]
pub struct Incoming {
    /// Completed reliable message buffers (stream 0), in order.
    pub reliable: Vec<Vec<u8>>,
    /// Completed file-stream transfers (stream 1): `(filename, bytes)`.
    pub files: Vec<(String, usize)>,
    /// The unreliable remainder of the datagram and the bit offset where it starts.
    pub unreliable: Vec<u8>,
    pub unreliable_start_bit: usize,
    pub header: Option<PacketHeader>,
}

/// Netchannel state for one peer.
#[derive(Debug)]
pub struct NetChannel {
    challenge: u32,
    out_seq: i32,
    in_seq: i32,
    out_seq_ack: i32,
    in_reliable_state: u16,
    out_reliable_state: u16,
    pending: BitWriter,
    queue: VecDeque<OutTransfer>,
    sub: Option<SubChannel>,
    next_sub_index: usize,
    recv: [Option<InTransfer>; MAX_STREAMS],
    /// Fragments placed in one datagram.
    pub fragments_per_packet: usize,
    /// Reliable transfers whose total size is at most this are sent as a single block.
    pub single_block_limit: usize,
}

const PENDING_CAPACITY: usize = MAX_INCOMING_TRANSFER;

impl NetChannel {
    pub fn new(challenge: u32) -> Self {
        Self {
            challenge,
            out_seq: 1,
            in_seq: 0,
            out_seq_ack: 0,
            in_reliable_state: 0,
            out_reliable_state: 0,
            pending: BitWriter::with_capacity(PENDING_CAPACITY),
            queue: VecDeque::new(),
            sub: None,
            next_sub_index: 0,
            recv: [None, None],
            fragments_per_packet: 1,
            single_block_limit: 1024,
        }
    }

    pub fn challenge(&self) -> u32 {
        self.challenge
    }
    pub fn in_sequence(&self) -> i32 {
        self.in_seq
    }
    pub fn out_sequence(&self) -> i32 {
        self.out_seq
    }

    /// Appends messages to the reliable stream (sent in order, after earlier ones).
    pub fn write_reliable(&mut self, msgs: &BitWriter) -> Result<()> {
        self.pending.append(msgs)
    }

    /// Closes the current reliable batch so later messages go into a new transfer.
    pub fn flush_reliable(&mut self) {
        if self.pending.bit_len() == 0 {
            return;
        }
        let mut w = std::mem::replace(
            &mut self.pending,
            BitWriter::with_capacity(PENDING_CAPACITY),
        );
        // Pad with NOP (zero) bits up to a byte boundary.
        let pad = w.bits_to_byte_boundary();
        if pad > 0 {
            let _ = w.write_ubits(0, pad);
        }
        let data = w.into_bytes();
        let num_fragments = data.len().div_ceil(FRAGMENT_SIZE).max(1);
        self.queue.push_back(OutTransfer {
            data,
            num_fragments,
            acked: 0,
        });
    }

    /// True if reliable data is queued or not yet acknowledged.
    pub fn has_pending_reliable(&self) -> bool {
        !self.queue.is_empty() || self.pending.bit_len() > 0
    }

    /// Processes one netchannel datagram (already reassembled and decompressed).
    /// Returns `Ok(None)` for stale/duplicate packets.
    pub fn process(&mut self, d: &[u8]) -> Result<Option<Incoming>> {
        let (h, off) = parse_header(d)?;
        match h.challenge {
            Some(c) if c != self.challenge => {
                return Err(ProtocolError::InvalidField {
                    field: "netchan",
                    reason: "bad challenge",
                })
            }
            None => {
                return Err(ProtocolError::InvalidField {
                    field: "netchan",
                    reason: "missing challenge",
                })
            }
            _ => {}
        }
        if h.sequence <= self.in_seq {
            return Ok(None);
        }
        if h.ack >= self.out_seq {
            return Err(ProtocolError::InvalidField {
                field: "netchan",
                reason: "ack of unsent sequence",
            });
        }
        // Acknowledgement of our sub-channel.
        if let Some(sub) = self.sub {
            let bit = 1u16 << sub.index;
            if let SubState::Waiting { send_seq } = sub.state {
                if (self.out_reliable_state & bit) == (h.reliable_state & bit) {
                    if let Some(t) = self.queue.front_mut() {
                        t.acked = sub.start + sub.count;
                        if t.acked >= t.num_fragments {
                            self.queue.pop_front();
                        }
                    }
                    self.sub = None;
                } else if send_seq <= h.ack {
                    if let Some(s) = self.sub.as_mut() {
                        s.state = SubState::ToSend;
                    }
                }
            }
        }
        self.in_seq = h.sequence;
        self.out_seq_ack = h.ack;

        let mut out = Incoming {
            header: Some(h),
            ..Default::default()
        };
        let mut r = BitReader::new(d);
        r.skip_bits(off * 8)?;
        if h.flags & PACKET_FLAG_RELIABLE != 0 {
            let index = r.read_ubits(SUBCHANNEL_BITS)? as usize;
            for stream in 0..MAX_STREAMS {
                if r.read_bit()? {
                    self.read_subchannel_data(&mut r, stream)?;
                }
            }
            self.in_reliable_state ^= 1 << index;
            for stream in 0..MAX_STREAMS {
                let done = matches!(&self.recv[stream], Some(t) if t.received.iter().all(|&x| x));
                if done {
                    let t = self.recv[stream].take().unwrap();
                    let data = match t.compressed_size {
                        Some(sz) => {
                            let v = lzss::decompress(&t.buf, MAX_INCOMING_TRANSFER)?;
                            if v.len() != sz {
                                return Err(ProtocolError::InvalidField {
                                    field: "netchan",
                                    reason: "uncompressed size mismatch",
                                });
                            }
                            v
                        }
                        None => t.buf,
                    };
                    if stream == 0 && !t.is_file {
                        out.reliable.push(data);
                    } else {
                        out.files.push((String::new(), data.len()));
                    }
                }
            }
        }
        out.unreliable_start_bit = r.bit_position();
        out.unreliable = d.to_vec();
        Ok(Some(out))
    }

    fn read_subchannel_data(&mut self, r: &mut BitReader<'_>, stream: usize) -> Result<()> {
        let single = !r.read_bit()?;
        let (mut start, mut count) = (0usize, 0usize);
        if !single {
            start = r.read_ubits(MAX_FILE_SIZE_BITS - FRAGMENT_BITS)? as usize;
            count = r.read_ubits(NUM_FRAGMENT_BITS)? as usize;
        }
        if start == 0 {
            let mut t = InTransfer::default();
            if single {
                if r.read_bit()? {
                    t.compressed_size = Some(r.read_ubits(MAX_FILE_SIZE_BITS)? as usize);
                }
                t.size = r.read_varint32()? as usize;
            } else {
                if r.read_bit()? {
                    let _transfer_id = r.read_u32()?;
                    let _name = r.read_string(260)?;
                    t.is_file = true;
                }
                if r.read_bit()? {
                    t.compressed_size = Some(r.read_ubits(MAX_FILE_SIZE_BITS)? as usize);
                }
                t.size = r.read_ubits(MAX_FILE_SIZE_BITS)? as usize;
            }
            if t.size > MAX_INCOMING_TRANSFER
                || t.compressed_size.unwrap_or(0) > MAX_INCOMING_TRANSFER
            {
                return Err(ProtocolError::Oversized {
                    size: t.size,
                    limit: MAX_INCOMING_TRANSFER,
                });
            }
            t.num_fragments = t.size.div_ceil(FRAGMENT_SIZE).max(1);
            t.buf = vec![0; t.size];
            t.received = vec![false; t.num_fragments];
            if single {
                count = t.num_fragments;
            }
            self.recv[stream] = Some(t);
        }
        let t = self.recv[stream]
            .as_mut()
            .ok_or(ProtocolError::InvalidField {
                field: "netchan",
                reason: "fragment without header",
            })?;
        if count == 0 || start + count > t.num_fragments {
            return Err(ProtocolError::InvalidField {
                field: "netchan",
                reason: "fragment out of range",
            });
        }
        let off = start * FRAGMENT_SIZE;
        let end = ((start + count) * FRAGMENT_SIZE).min(t.size);
        r.read_bytes(&mut t.buf[off..end])?;
        for f in start..start + count {
            t.received[f] = true;
        }
        Ok(())
    }

    /// Builds the next datagram: header, pending reliable fragment (if any), then the
    /// given unreliable messages. The result may exceed the routable MTU; callers split
    /// it with [`crate::split::split`].
    pub fn transmit(&mut self, unreliable: Option<&BitWriter>) -> Result<Vec<u8>> {
        self.flush_reliable();
        if self.sub.is_none() {
            if let Some(t) = self.queue.front() {
                let count = (t.num_fragments - t.acked).min(self.fragments_per_packet.max(1));
                let index = self.next_sub_index;
                self.next_sub_index = (index + 1) % MAX_SUBCHANNELS;
                self.out_reliable_state ^= 1 << index;
                self.sub = Some(SubChannel {
                    index,
                    start: t.acked,
                    count,
                    state: SubState::ToSend,
                });
            }
        }

        let mut w = BitWriter::with_capacity(
            HEADER_LEN + FRAGMENT_SIZE * self.fragments_per_packet.max(1) + 64 * 1024,
        );
        w.write_u32(self.out_seq as u32)?;
        w.write_u32(self.in_seq as u32)?;
        w.write_u8(0)?; // flags, patched below
        w.write_u16(0)?; // checksum, patched below
        w.write_u16(self.in_reliable_state)?;
        w.write_u32(self.challenge)?;
        let mut flags = PACKET_FLAG_CHALLENGE;

        if let Some(sub) = self.sub.as_mut() {
            if sub.state == SubState::ToSend {
                let t = self.queue.front().expect("sub-channel without transfer");
                flags |= PACKET_FLAG_RELIABLE;
                w.write_ubits(sub.index as u32, SUBCHANNEL_BITS)?;
                // stream 0: data
                w.write_bit(true)?;
                let single = sub.start == 0
                    && sub.count == t.num_fragments
                    && t.data.len() <= self.single_block_limit;
                if single {
                    w.write_bit(false)?; // single block
                    w.write_bit(false)?; // not compressed
                    w.write_varint32(t.data.len() as u32)?;
                    w.write_bytes(&t.data)?;
                } else {
                    w.write_bit(true)?;
                    w.write_ubits(sub.start as u32, MAX_FILE_SIZE_BITS - FRAGMENT_BITS)?;
                    w.write_ubits(sub.count as u32, NUM_FRAGMENT_BITS)?;
                    if sub.start == 0 {
                        w.write_bit(false)?; // not a file
                        w.write_bit(false)?; // not compressed
                        w.write_ubits(t.data.len() as u32, MAX_FILE_SIZE_BITS)?;
                    }
                    let off = sub.start * FRAGMENT_SIZE;
                    let end = ((sub.start + sub.count) * FRAGMENT_SIZE).min(t.data.len());
                    w.write_bytes(&t.data[off..end])?;
                }
                // stream 1: nothing
                w.write_bit(false)?;
                sub.state = SubState::Waiting {
                    send_seq: self.out_seq,
                };
            }
        }

        if let Some(u) = unreliable {
            w.append(u)?;
        }
        // Trailer: at least one NOP after the last message, never a remainder that
        // could be read as a full message type, then pad to a byte with ones and
        // record the pad count in the flags (as the reference encoder does).
        w.write_ubits(0, NETMSG_TYPE_BITS)?;
        let rem = w.bit_len() % 8;
        if rem > 0 && rem <= (8 - NETMSG_TYPE_BITS as usize) {
            w.write_ubits(0, NETMSG_TYPE_BITS)?;
        }
        let pad = w.bits_to_byte_boundary();
        if pad > 0 {
            w.write_ubits((1u32 << pad) - 1, pad)?;
            flags |= (pad as u8) << 5;
        }
        let mut d = w.into_bytes();
        d[8] = flags;
        let ck = checksum16(&d[11..]);
        d[9..11].copy_from_slice(&ck.to_le_bytes());
        self.out_seq += 1;
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    /// An empty keep-alive from the reference server (sequence 2, ack 1, in-state 1)
    /// with challenge 0x5B07DFE9: 17 bytes, checksum 0xF4C6.
    #[test]
    fn reference_keepalive_checksum() {
        let d = hex("020000000100000020c6f40100e9df075b");
        let (h, off) = parse_header(&d).unwrap();
        assert_eq!(off, 17);
        assert_eq!(h.sequence, 2);
        assert_eq!(h.ack, 1);
        assert_eq!(h.reliable_state, 1);
        assert_eq!(h.challenge, Some(0x5B07_DFE9));
        let mut bad = d.clone();
        bad[15] ^= 1;
        assert!(parse_header(&bad).is_err());
    }

    fn pair() -> (NetChannel, NetChannel) {
        (NetChannel::new(0xABCD_1234), NetChannel::new(0xABCD_1234))
    }

    fn msgs(n: usize, seed: u8) -> BitWriter {
        let mut w = BitWriter::with_capacity(n + 16);
        for i in 0..n {
            w.write_u8(seed.wrapping_add(i as u8)).unwrap();
        }
        w
    }

    #[test]
    fn reliable_single_block_roundtrip_and_ack() {
        let (mut a, mut b) = pair();
        a.write_reliable(&msgs(100, 7)).unwrap();
        let d = a.transmit(None).unwrap();
        let inc = b.process(&d).unwrap().unwrap();
        assert_eq!(inc.reliable.len(), 1);
        assert_eq!(inc.reliable[0], msgs(100, 7).into_bytes());
        // b acks
        let d = b.transmit(None).unwrap();
        a.process(&d).unwrap().unwrap();
        assert!(!a.has_pending_reliable());
        // duplicate packet ignored
        assert!(a.process(&d).unwrap().is_none());
    }

    #[test]
    fn reliable_fragmented_with_loss_and_resend() {
        let (mut a, mut b) = pair();
        let big = msgs(3 * FRAGMENT_SIZE + 123, 1);
        a.write_reliable(&big).unwrap();
        let mut got = Vec::new();
        let mut drop_next = true;
        for _ in 0..40 {
            let d = a.transmit(None).unwrap();
            if drop_next && d.len() > 100 {
                drop_next = false; // lose the first data packet
            } else if let Some(inc) = b.process(&d).unwrap() {
                got.extend(inc.reliable);
            }
            let back = b.transmit(None).unwrap();
            a.process(&back).unwrap();
            if !a.has_pending_reliable() {
                break;
            }
        }
        assert!(!a.has_pending_reliable(), "transfer must complete");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], big.into_bytes());
    }

    #[test]
    fn separate_batches_arrive_in_order() {
        let (mut a, mut b) = pair();
        a.write_reliable(&msgs(10, 1)).unwrap();
        a.flush_reliable();
        a.write_reliable(&msgs(20, 2)).unwrap();
        let mut got = Vec::new();
        for _ in 0..10 {
            let d = a.transmit(None).unwrap();
            got.extend(b.process(&d).unwrap().unwrap().reliable);
            let back = b.transmit(None).unwrap();
            a.process(&back).unwrap();
        }
        assert_eq!(
            got,
            vec![msgs(10, 1).into_bytes(), msgs(20, 2).into_bytes()]
        );
    }

    #[test]
    fn unreliable_passthrough_and_padding() {
        let (mut a, mut b) = pair();
        let mut u = BitWriter::with_capacity(16);
        u.write_ubits(0b10_1010, 6).unwrap();
        u.write_ubits(5, 3).unwrap();
        let d = a.transmit(Some(&u)).unwrap();
        let inc = b.process(&d).unwrap().unwrap();
        let mut r = BitReader::new(&inc.unreliable);
        r.skip_bits(inc.unreliable_start_bit).unwrap();
        assert_eq!(r.read_ubits(6).unwrap(), 0b10_1010);
        assert_eq!(r.read_ubits(3).unwrap(), 5);
        assert!(r.bits_remaining() >= 6);
        assert_eq!(r.read_ubits(6).unwrap(), 0, "trailing NOP");
    }

    #[test]
    fn rejects_bad_challenge_and_garbage() {
        let mut a = NetChannel::new(1);
        let mut b = NetChannel::new(2);
        let d = a.transmit(None).unwrap();
        assert!(b.process(&d).is_err());
        for cut in 0..d.len() {
            let _ = b.process(&d[..cut]);
        }
        // reliable flag with truncated body
        let mut a = NetChannel::new(2);
        a.write_reliable(&msgs(50, 3)).unwrap();
        let d = a.transmit(None).unwrap();
        for cut in 13..d.len() {
            let mut x = d[..cut].to_vec();
            let ck = checksum16(&x[11..]);
            x[9..11].copy_from_slice(&ck.to_le_bytes());
            let mut fresh = NetChannel::new(2);
            let _ = fresh.process(&x);
        }
    }

    #[test]
    fn hostile_fragment_sizes() {
        // Handcraft a reliable packet announcing an absurd size.
        let mut w = BitWriter::with_capacity(64);
        w.write_u32(1).unwrap();
        w.write_u32(0).unwrap();
        w.write_u8(PACKET_FLAG_RELIABLE | PACKET_FLAG_CHALLENGE)
            .unwrap();
        w.write_u16(0).unwrap();
        w.write_u16(0).unwrap();
        w.write_u32(9).unwrap();
        w.write_ubits(0, SUBCHANNEL_BITS).unwrap();
        w.write_bit(true).unwrap();
        w.write_bit(true).unwrap(); // fragmented
        w.write_ubits(0, 18).unwrap();
        w.write_ubits(1, 5).unwrap();
        w.write_bit(false).unwrap();
        w.write_bit(false).unwrap();
        w.write_ubits((1 << 30) - 1, 30).unwrap();
        let mut d = w.into_bytes();
        let ck = checksum16(&d[11..]);
        d[9..11].copy_from_slice(&ck.to_le_bytes());
        let mut c = NetChannel::new(9);
        assert!(matches!(
            c.process(&d),
            Err(ProtocolError::Oversized { .. })
        ));
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
