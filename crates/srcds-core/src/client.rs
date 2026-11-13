//! One connected game client: netchannel, signon progress, outgoing traffic.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use srcds_protocol::bitbuf::{BitReader, BitWriter};
use srcds_protocol::netchan::{self, NetChannel};
use srcds_protocol::netmsg::{self, ClientMessage, PacketEntities};
use srcds_protocol::split::{self, Reassembler, DEFAULT_SPLIT_PAYLOAD};
use tracing::{debug, info, warn};

use crate::signon::{self, GameInfo};

/// Drop a client that has been silent this long.
pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);
/// Longer allowance while the client loads the map.
pub const SIGNON_TIMEOUT: Duration = Duration::from_secs(120);
/// Largest datagram sent without split framing.
const MAX_UNSPLIT: usize = 1260;

#[derive(Debug)]
pub struct Client {
    pub addr: SocketAddr,
    pub slot: u8,
    pub user_id: i32,
    pub name: String,
    /// SteamID64 claimed in the connect packet. Not verified (no Steam session
    /// validation is performed by this server yet).
    pub claimed_steam_id: Option<u64>,
    /// Last signon state reported by the client.
    pub signon: u8,
    pub chan: NetChannel,
    reassembler: Reassembler,
    pub last_recv: Instant,
    pub connected_at: Instant,
    /// Set when the connection should be torn down, with the reason.
    pub drop_reason: Option<String>,
    split_id: i32,
    sent_first_snapshot: bool,
    pub fully_connected_at: Option<Instant>,
}

impl Client {
    pub fn new(
        addr: SocketAddr,
        slot: u8,
        user_id: i32,
        name: String,
        claimed_steam_id: Option<u64>,
        challenge: u32,
        now: Instant,
    ) -> Self {
        Self {
            addr,
            slot,
            user_id,
            name,
            claimed_steam_id,
            signon: netmsg::SIGNONSTATE_CHALLENGE,
            chan: NetChannel::new(challenge),
            reassembler: Reassembler::new(),
            last_recv: now,
            connected_at: now,
            drop_reason: None,
            split_id: 1,
            sent_first_snapshot: false,
            fully_connected_at: None,
        }
    }

    pub fn player_info(&self) -> Vec<u8> {
        let guid = self
            .claimed_steam_id
            .map(signon::steam2_id)
            .unwrap_or_else(|| "STEAM_ID_LAN".to_string());
        signon::player_info(&self.name, self.user_id, &guid, false)
    }

    pub fn timed_out(&self, now: Instant) -> bool {
        let limit = if self.signon >= netmsg::SIGNONSTATE_FULL || self.sent_first_snapshot {
            CLIENT_TIMEOUT
        } else {
            SIGNON_TIMEOUT
        };
        now.duration_since(self.last_recv) > limit
    }

    /// Handles one datagram from this client (any framing except connectionless).
    pub fn on_datagram(
        &mut self,
        data: &[u8],
        game: &GameInfo,
        tick: i32,
        players: &[(u8, Vec<u8>)],
        now: Instant,
    ) {
        let whole;
        let mut d = data;
        if d.len() >= 4
            && i32::from_le_bytes(d[..4].try_into().unwrap()) == netchan::NET_HEADER_FLAG_SPLIT
        {
            match self.reassembler.push(d) {
                Ok(Some(v)) => {
                    whole = v;
                    d = &whole;
                }
                Ok(None) => return,
                Err(e) => {
                    debug!(addr = %self.addr, error = %e, "bad split fragment");
                    return;
                }
            }
        }
        let decompressed;
        match netchan::unwrap_compressed(d) {
            Ok(Some(v)) => {
                decompressed = v;
                d = &decompressed;
            }
            Ok(None) => {}
            Err(e) => {
                debug!(addr = %self.addr, error = %e, "bad compressed packet");
                return;
            }
        }
        let inc = match self.chan.process(d) {
            Ok(Some(i)) => i,
            Ok(None) => return,
            Err(e) => {
                debug!(addr = %self.addr, error = %e, "dropped netchannel packet");
                return;
            }
        };
        self.last_recv = now;
        for buf in &inc.reliable {
            let mut r = BitReader::new(buf);
            let (msgs, err) = netmsg::decode_client_messages(&mut r);
            if let Some(e) = err {
                warn!(addr = %self.addr, error = %e, "undecodable reliable client message; rest of batch skipped");
            }
            for m in msgs {
                self.on_message(m, game, tick, players, true);
            }
        }
        let mut r = BitReader::new(&inc.unreliable);
        if r.skip_bits(inc.unreliable_start_bit).is_ok() {
            let (msgs, err) = netmsg::decode_client_messages(&mut r);
            if let Some(e) = err {
                debug!(addr = %self.addr, error = %e, "undecodable unreliable client message");
            }
            for m in msgs {
                self.on_message(m, game, tick, players, false);
            }
        }
    }

    fn queue(&mut self, batch: srcds_protocol::error::Result<BitWriter>) {
        match batch.and_then(|w| self.chan.write_reliable(&w)) {
            Ok(()) => self.chan.flush_reliable(),
            Err(e) => {
                warn!(addr = %self.addr, error = %e, "failed to build signon batch");
                self.drop_reason = Some(format!("Server failed to build signon data ({e})"));
            }
        }
    }

    fn on_message(
        &mut self,
        m: ClientMessage,
        game: &GameInfo,
        tick: i32,
        players: &[(u8, Vec<u8>)],
        _reliable: bool,
    ) {
        match m {
            ClientMessage::SetConVar(vars) => {
                for (k, v) in vars {
                    if k == "name" && !v.is_empty() {
                        self.name = v.chars().take(127).collect();
                    }
                }
            }
            ClientMessage::SignonState { state, spawn_count } => {
                info!(addr = %self.addr, state, spawn_count, "client signon state");
                self.signon = state;
                match state {
                    netmsg::SIGNONSTATE_CONNECTED => {
                        self.queue(signon::server_info_batch(game, self.slot));
                        if game.full_signon {
                            // Experimental: the full string-table / datatable / entity
                            // signon. Incomplete (no real datatables or baselines yet),
                            // so it does not lead to a playable session and can upset
                            // the client at the datatable stage. Off by default.
                            self.queue(signon::tables_batch(game, tick, players));
                        } else {
                            // Honest, graceful outcome: the handshake and ServerInfo
                            // exchange succeeded, but world signon (datatables, entity
                            // baselines, GLua) is not implemented. Send the unmodified
                            // client a plain net_Disconnect right after ServerInfo so it
                            // leaves cleanly, rather than feeding it data it cannot
                            // complete. Reliable ordering guarantees ServerInfo first.
                            let mut w = BitWriter::with_capacity(512);
                            let msg = "srcds-rs: connected, server info received. Full \
                                       signon (datatables, entities, GLua) is not \
                                       implemented yet, so the map cannot be joined.";
                            if netmsg::write_disconnect(&mut w, msg).is_ok() {
                                self.queue(Ok(w));
                            }
                        }
                    }
                    netmsg::SIGNONSTATE_NEW => {
                        self.queue(signon::prespawn_batch(game));
                    }
                    netmsg::SIGNONSTATE_PRESPAWN => {
                        self.queue(signon::spawn_batch(game, self.slot, tick));
                    }
                    netmsg::SIGNONSTATE_SPAWN => {
                        // The first snapshot goes out with the next transmit.
                    }
                    netmsg::SIGNONSTATE_FULL if self.fully_connected_at.is_none() => {
                        self.fully_connected_at = Some(Instant::now());
                        info!(addr = %self.addr, name = %self.name, "client fully connected");
                    }
                    _ => {}
                }
            }
            ClientMessage::Disconnect(reason) => {
                info!(addr = %self.addr, %reason, "client disconnected");
                self.drop_reason = Some(String::new());
            }
            ClientMessage::ClientInfo { send_table_crc, .. } => {
                debug!(addr = %self.addr, send_table_crc = format!("{send_table_crc:#010x}"), "client info");
            }
            ClientMessage::StringCmd(cmd) => {
                debug!(addr = %self.addr, %cmd, "client command");
            }
            _ => {}
        }
    }

    /// Unreliable messages for this tick.
    fn unreliable_for_tick(
        &mut self,
        game: &GameInfo,
        tick: i32,
    ) -> srcds_protocol::error::Result<Option<BitWriter>> {
        if self.signon < netmsg::SIGNONSTATE_SPAWN {
            return Ok(None);
        }
        let mut w = BitWriter::with_capacity(256);
        netmsg::write_tick(&mut w, tick, game.tick_interval, 0.0)?;
        // Empty full update: this server simulates no entities yet. The first one moves
        // the client from SPAWN to FULL.
        netmsg::write_packet_entities(
            &mut w,
            &PacketEntities {
                max_entries: 0,
                delta_from: None,
                baseline: 0,
                updated_entries: 0,
                update_baseline: false,
                data: &[],
                data_bits: 0,
            },
        )?;
        if !self.sent_first_snapshot {
            netmsg::write_fix_angle(&mut w, false, [0.0, 0.0, 0.0])?;
            self.sent_first_snapshot = true;
        }
        Ok(Some(w))
    }

    /// Builds this tick's datagrams (header + pending reliable data + unreliable).
    pub fn transmit(&mut self, game: &GameInfo, tick: i32) -> Vec<Vec<u8>> {
        let unrel = match self.unreliable_for_tick(game, tick) {
            Ok(u) => u,
            Err(e) => {
                warn!(addr = %self.addr, error = %e, "failed to build snapshot");
                None
            }
        };
        let d = match self.chan.transmit(unrel.as_ref()) {
            Ok(d) => d,
            Err(e) => {
                warn!(addr = %self.addr, error = %e, "netchannel transmit failed");
                return Vec::new();
            }
        };
        if d.len() <= MAX_UNSPLIT {
            return vec![d];
        }
        self.split_id = self.split_id.wrapping_add(1);
        match split::split(&d, self.split_id, DEFAULT_SPLIT_PAYLOAD) {
            Ok(v) => v,
            Err(e) => {
                warn!(addr = %self.addr, error = %e, len = d.len(), "split failed");
                Vec::new()
            }
        }
    }

    /// Datagram telling the client it is being disconnected.
    pub fn disconnect_datagram(&mut self, reason: &str) -> Option<Vec<u8>> {
        let mut w = BitWriter::with_capacity(512);
        netmsg::write_disconnect(&mut w, reason).ok()?;
        self.chan.transmit(Some(&w)).ok()
    }
}
