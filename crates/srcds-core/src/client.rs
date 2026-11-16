//! One connected game client: netchannel, signon progress, outgoing traffic.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use srcds_protocol::bitbuf::{BitReader, BitWriter};
use srcds_protocol::netchan::{self, NetChannel};
use srcds_protocol::netmsg::{self, ClientMessage, PacketEntities};
use srcds_protocol::split::{self, Reassembler, DEFAULT_SPLIT_PAYLOAD};
use tracing::{debug, info, warn};

use crate::entities::{self, EntityState, PlayerPawn, Snapshot};
use crate::movement;
use crate::signon::{self, GameInfo};
use srcds_protocol::usercmd::{self, UserCmd};

/// Snapshots kept per client as possible delta bases (≈1 s at 66 Hz).
const SNAPSHOT_HISTORY: usize = 64;
/// Ticks to wait for the client to confirm a full update before sending another
/// (it may still be loading the map's content).
const FULL_UPDATE_RETRY_TICKS: i32 = 200;

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
    /// Tick of the last full (non-delta) snapshot sent, -1 = none.
    last_full_tick: i32,
    /// Diagnostics: snapshots logged so far, last snapshot tick.
    snap_log: u32,
    last_snapshot_tick: i32,
    pub fully_connected_at: Option<Instant>,
    /// The player's pawn once spawned (from PRESPAWN on).
    pub pawn: Option<PlayerPawn>,
    /// Serial number of this client's player entity.
    pub serial: u32,
    /// Last snapshot tick the client reported having (`net_Tick`), -1 = none.
    delta_tick: i32,
    snapshots: VecDeque<Snapshot>,
    /// Number of the last executed user command.
    last_command: Option<i32>,
    /// The client's `fov_desired` setting (sent in its user info convars).
    pub fov_desired: Option<f32>,
    /// Server-wide console commands typed by this player (`status`, `ping`), answered
    /// by the server loop, which has the global view.
    pub pending_commands: Vec<String>,
    /// Send times of recent outgoing sequence numbers, for round-trip measurement.
    sent_at: VecDeque<(i32, Instant)>,
    /// Smoothed round-trip time in milliseconds.
    pub rtt_ms: Option<f32>,
    /// This player's entity in the Lua state, once created.
    pub lua_ent: Option<srcds_lua::entity::EntRef>,
    /// Console commands for Lua (`concommand`s, `say`), run by the server loop.
    pub lua_commands: Vec<String>,
    /// Buttons of the last executed user command.
    pub buttons: i32,
    /// GMod packets from the client (`clc_GMod_ClientToServer`: data, bits) for Lua.
    pub gmod_packets: Vec<(Vec<u8>, usize)>,
    /// The client's user-info convars (`net_SetConVar`), for `Player:GetInfo`.
    pub user_info: std::collections::HashMap<String, String>,
    /// User info changed since Lua last copied it.
    pub user_info_dirty: bool,
    last_raw48: u64,
    last_flag: bool,
    /// GMod button codes held in the last command.
    held_keys: Vec<u8>,
    /// Button presses / releases (code, down) for `PlayerButtonDown` / `Up`.
    pub button_events: Vec<(u8, bool)>,
    /// Cursor direction while world clicking (context menu held), else `None`.
    pub world_click: Option<[f32; 3]>,
    /// Executed commands' weapon input, for the weapon logic.
    pub weapon_cmds: Vec<crate::weapon_sim::WeaponCmd>,
    last_cmd_buttons: i32,
    /// Noclip toggles from `impulse 154` (new state), for the `PlayerNoClip` check.
    pub noclip_events: Vec<bool>,
    /// Flashlight toggle requests (`impulse 100`).
    pub flashlight_requests: u32,
    /// Weapon entity the client asked to switch to (`weaponselect`).
    pub weapon_select: Option<u32>,
    /// Ticks the pending selection has waited.
    pub weapon_select_age: u32,
    /// Accumulated mouse wheel and mouse movement since the last tick.
    pub mouse_wheel: i32,
    pub mouse_delta: (i32, i32),
    /// Dynamic string-table entries this client has (models, network strings), once
    /// its string tables were created.
    pub tables_known: Option<(usize, usize)>,
    /// Dynamic models the client has acknowledged (its reliable stream drained).
    models_acked: usize,
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
            last_full_tick: -1,
            snap_log: 0,
            last_snapshot_tick: -1,
            fully_connected_at: None,
            pawn: None,
            serial: (user_id as u32).wrapping_mul(7) & 0x3FF,
            delta_tick: -1,
            snapshots: VecDeque::new(),
            last_command: None,
            fov_desired: None,
            pending_commands: Vec::new(),
            sent_at: VecDeque::new(),
            rtt_ms: None,
            lua_ent: None,
            lua_commands: Vec::new(),
            buttons: 0,
            gmod_packets: Vec::new(),
            user_info: std::collections::HashMap::new(),
            user_info_dirty: true,
            weapon_select: None,
            weapon_select_age: 0,
            last_raw48: 0,
            last_flag: false,
            noclip_events: Vec::new(),
            flashlight_requests: 0,
            held_keys: Vec::new(),
            button_events: Vec::new(),
            world_click: None,
            weapon_cmds: Vec::new(),
            last_cmd_buttons: 0,
            mouse_wheel: 0,
            mouse_delta: (0, 0),
            tables_known: None,
            models_acked: 0,
        }
    }

    /// Queues console text for the player (reliable `svc_Print`).
    pub fn print(&mut self, text: &str) {
        let mut w = BitWriter::with_capacity(text.len() + 64);
        if netmsg::write_print(&mut w, text).is_ok() {
            self.queue(Ok(w));
        }
    }

    /// Runs a console command on the client (reliable `net_StringCmd`), like
    /// `Player:ConCommand`.
    pub fn send_command(&mut self, cmd: &str) {
        let mut w = BitWriter::with_capacity(cmd.len() + 64);
        if netmsg::write_string_cmd(&mut w, cmd).is_ok() {
            self.queue(Ok(w));
        }
    }

    /// Renders the view from entity `index` (`svc_SetView`; the player's own index =
    /// the normal view).
    pub fn send_set_view(&mut self, index: u32) {
        let mut w = BitWriter::with_capacity(64);
        if netmsg::write_set_view(&mut w, index).is_ok() {
            self.queue(Ok(w));
        }
    }

    /// Sends a game event (reliable `svc_GameEvent`).
    pub fn send_game_event(&mut self, events: &crate::gameevents::GameEvents, name: &str, values: &[(&str, crate::gameevents::EventValue)]) {
        let mut w = BitWriter::with_capacity(512);
        if let Ok(true) = events.write_event(&mut w, name, values) {
            self.queue(Ok(w));
        }
    }

    /// Queues a user message (reliable).
    pub fn send_user_message(&mut self, msg_type: u8, data: &[u8]) {
        let mut w = BitWriter::with_capacity(data.len() + 16);
        if netmsg::write_user_message(&mut w, msg_type, data, data.len() * 8).is_ok() {
            self.queue(Ok(w));
        }
    }

    /// `TextMsg`: [факт] u8 destination (1 notify, 2 console, 3 chat, 4 centre),
    /// the text, then four parameter strings (empty).
    pub fn text_msg(&mut self, dest: u8, text: &str) {
        let mut d = vec![dest];
        d.extend(text.bytes().filter(|b| *b != 0).take(240));
        d.push(0);
        d.extend_from_slice(&[0, 0, 0, 0]);
        self.send_user_message(netmsg::UM_TEXTMSG, &d);
    }

    /// `SayText`: [факт] u8 sender entity index, text, u8 1 (chat), u8 team only,
    /// u8 sender dead.
    pub fn say_text(&mut self, sender: u8, text: &str, team: bool, dead: bool) {
        let mut d = vec![sender];
        d.extend(text.bytes().filter(|b| *b != 0).take(240));
        d.extend_from_slice(&[0, 1, team as u8, dead as u8]);
        self.send_user_message(netmsg::UM_SAYTEXT, &d);
    }

    /// Queues a GMod packet (`svc_GMod_ServerToClient`, reliable).
    pub fn send_gmod(&mut self, data: &[u8], bits: usize) {
        let mut w = BitWriter::with_capacity(data.len() + 16);
        if netmsg::write_gmod_server_to_client(&mut w, data, bits).is_ok() {
            self.queue(Ok(w));
        }
    }

    /// Ping as shown to players (one-way-ish: the round trip, like the engine shows).
    pub fn ping_ms(&self) -> i32 {
        self.rtt_ms.map_or(0, |r| r.round() as i32)
    }

    fn note_ack(&mut self, ack: i32, now: Instant) {
        while let Some(&(seq, at)) = self.sent_at.front() {
            if seq.wrapping_sub(ack) > 0 {
                break;
            }
            self.sent_at.pop_front();
            if seq == ack {
                let ms = now.duration_since(at).as_secs_f32() * 1000.0;
                self.rtt_ms = Some(match self.rtt_ms {
                    Some(r) => r * 0.9 + ms * 0.1,
                    None => ms,
                });
            }
        }
    }

    /// Places the player's pawn at the map's spawn point for this slot.
    pub(crate) fn spawn(&mut self, game: &GameInfo, tick: i32) {
        let sp = &game.map.spawn_points;
        let (origin, yaw) = match sp.get(self.slot as usize % sp.len().max(1)) {
            Some(p) => (p.origin, p.yaw),
            None => ([0.0, 0.0, 64.0], 0.0),
        };
        info!(addr = %self.addr, ?origin, yaw, "player spawned");
        let mut mv = movement::MoveState::new(origin);
        mv.max_speed = game.movement.walk_speed;
        self.pawn = Some(PlayerPawn {
            mv,
            angles: [0.0, yaw, 0.0],
            tick_base: tick,
            sim_tick: tick,
            move_type: entities::MOVETYPE_WALK,
            health: 100,
            model_index: game.player_model_index(),
            default_fov: self.fov_desired.unwrap_or(entities::DEFAULT_FOV),
            params: game.movement,
            inventory: Default::default(),
            flashlight: false,
            hands: entities::INVALID_EHANDLE,
            dt: Vec::new(),
            ammo: Vec::new(),
            player_color: [0.24, 0.34, 0.41],
            weapon_color: [0.3, 1.8, 2.1],
            next_attack: 0.0,
            last_weapon: entities::INVALID_EHANDLE,
            player_class: 0,
        });
        self.last_command = None;
    }

    /// `setpos x y z`: moves the player (feet) there and stops it.
    fn setpos(&mut self, cmd: &str) {
        let v: Vec<f32> = cmd
            .split_whitespace()
            .skip(1)
            .filter_map(|t| t.trim_end_matches(';').parse().ok())
            .collect();
        let Some(p) = self.pawn.as_mut() else {
            return;
        };
        if v.len() < 3 || !v.iter().all(|c| c.is_finite() && c.abs() < 32768.0) {
            self.print("usage: setpos x y z\n");
            return;
        }
        p.mv.origin = [v[0], v[1], v[2]];
        p.mv.velocity = [0.0; 3];
        p.mv.on_ground = false;
        info!(addr = %self.addr, origin = ?p.mv.origin, "setpos");
    }

    /// `noclip` console command: toggles free flight.
    fn toggle_noclip(&mut self) {
        if let Some(p) = self.pawn.as_mut() {
            p.move_type = if p.move_type == entities::MOVETYPE_NOCLIP {
                entities::MOVETYPE_WALK
            } else {
                entities::MOVETYPE_NOCLIP
            };
            p.mv.on_ground = false;
            let on = p.move_type == entities::MOVETYPE_NOCLIP;
            info!(addr = %self.addr, name = %self.name, noclip = on, "noclip toggled");
            self.print(if on { "noclip ON\n" } else { "noclip OFF\n" });
        }
    }

    /// Per-tick bookkeeping of the pawn (movement itself runs per received command).
    pub fn simulate(&mut self, _tick: i32) {
        let fov = self.fov_desired.unwrap_or(entities::DEFAULT_FOV);
        if let Some(p) = self.pawn.as_mut() {
            p.default_fov = fov;
        }
    }

    /// Runs the not yet executed commands of one `clc_Move` (backups first, so a lost
    /// packet's commands are recovered).
    fn run_commands(&mut self, game: &GameInfo, tick: i32, cmds: &[UserCmd]) {
        let Some(pawn) = self.pawn.as_mut() else {
            return;
        };
        if self.signon < netmsg::SIGNONSTATE_SPAWN {
            return;
        }
        let dead = pawn.health <= 0;
        let dt = game.tick_interval;
        for c in cmds {
            if self
                .last_command
                .is_some_and(|last| c.command_number.wrapping_sub(last) <= 0)
            {
                continue;
            }
            self.last_command = Some(c.command_number);
            if c.gmod_raw48 != self.last_raw48 || c.gmod_flag != self.last_flag {
                debug!(target: "usercmd_gmod", raw48 = format!("{:#014x}", c.gmod_raw48), flag = c.gmod_flag, "usercmd GMod fields");
                self.last_raw48 = c.gmod_raw48;
                self.last_flag = c.gmod_flag;
            }
            for &k in c.held_keys.iter().filter(|k| !self.held_keys.contains(k)) {
                self.button_events.push((k, true));
            }
            for &k in self.held_keys.iter().filter(|k| !c.held_keys.contains(k)) {
                self.button_events.push((k, false));
            }
            self.held_keys.clone_from(&c.held_keys);
            self.world_click = c.world_clicking.then(|| srcds_protocol::usercmd::world_click_direction(c.gmod_raw48)).flatten();
            // [факт] the GMod client sends noclip as impulse 154 (it predicts the
            // switch itself); impulse 100 is the flashlight
            match c.impulse {
                154 => {
                    pawn.move_type = if pawn.move_type == entities::MOVETYPE_NOCLIP {
                        entities::MOVETYPE_WALK
                    } else {
                        entities::MOVETYPE_NOCLIP
                    };
                    pawn.mv.on_ground = false;
                    if pawn.move_type == entities::MOVETYPE_NOCLIP {
                        // [факт] the client is never crouched in noclip
                        pawn.mv.stand_up();
                    }
                    self.noclip_events.push(pawn.move_type == entities::MOVETYPE_NOCLIP);
                }
                100 => self.flashlight_requests += 1,
                _ => {}
            }
            self.buttons = c.buttons as i32;
            self.mouse_wheel += c.mouse_wheel as i32;
            self.mouse_delta.0 += c.mouse_dx as i32;
            self.mouse_delta.1 += c.mouse_dy as i32;
            let mut a = c.view_angles;
            a[0] = a[0].clamp(-89.0, 89.0);
            pawn.angles = a;
            {
                let view = if pawn.mv.ducked { entities::VIEW_HEIGHT_DUCKED } else { entities::VIEW_HEIGHT };
                let o = pawn.mv.origin;
                if self.weapon_cmds.len() < 64 {
                    self.weapon_cmds.push(crate::weapon_sim::WeaponCmd {
                        time: crate::weapon_sim::command_time(pawn.tick_base, dt),
                        buttons: c.buttons as i32,
                        old_buttons: self.last_cmd_buttons,
                        eye: [o[0], o[1], o[2] + view],
                        angles: pawn.angles,
                        select: c.weapon_select,
                    });
                } else if c.weapon_select != 0 {
                    self.weapon_select = Some(c.weapon_select);
                }
                self.last_cmd_buttons = c.buttons as i32;
            }
            if dead {
                // the dead do not move (the view still turns)
                pawn.tick_base = pawn.tick_base.wrapping_add(1);
                pawn.sim_tick = tick;
                continue;
            }
            if pawn.move_type == entities::MOVETYPE_NOCLIP {
                let mv = &mut pawn.mv;
                movement::begin_command(mv, dt);
                movement::update_speed_state(mv, c.buttons, &pawn.params);
                // [факт] (live client, cl_showerror) in noclip the client's fall velocity
                // (-vz) and ground state are those from before the move: its values
                // lagged ours by exactly one tick when taken after it
                mv.fall_velocity = -mv.velocity[2];
                movement::categorize_state(&game.world, &game.solids.borrow(), mv, &pawn.params);
                movement::noclip_move(&mut mv.origin, &mut mv.velocity, c, &pawn.params, dt);
                // no crouching in noclip: a duck key held when noclip ends is a fresh
                // press ([факт] the client crouched at once, in the air)
                mv.old_buttons = c.buttons & !srcds_protocol::usercmd::IN_DUCK;
            } else {
                let params = pawn.params;
                // the command's time, as the client's prediction uses it
                let now = pawn.tick_base as f64 * dt as f64;
                let ladders = game.ladders.borrow();
                let solids = game.solids.borrow();
                match movement::ladder_move(&game.world, &ladders, &mut pawn.mv, c, &params, now) {
                    movement::LadderOutcome::Frozen => {}
                    movement::LadderOutcome::Ladder => {
                        movement::ladder_fly(&game.world, &solids, &ladders, &mut pawn.mv, c, &params, dt, now);
                    }
                    movement::LadderOutcome::Walk => {
                        movement::walk_move_with(&game.world, &solids, &mut pawn.mv, c, &params, dt);
                    }
                }
                pawn.move_type = if pawn.mv.forced.is_some() {
                    entities::MOVETYPE_NONE
                } else if pawn.mv.ladder.is_some() {
                    entities::MOVETYPE_LADDER
                } else {
                    entities::MOVETYPE_WALK
                };
            }
            // Fell out of the collidable world (e.g. through terrain that has no
            // collision yet): put the player back at the spawn point.
            if pawn.mv.origin[2] < game.map.world_mins[2] - 1024.0 {
                warn!(addr = %self.addr, origin = ?pawn.mv.origin, "player fell out of the world; respawning");
                let sp = &game.map.spawn_points;
                if let Some(p) = sp.get(self.slot as usize % sp.len().max(1)) {
                    pawn.mv.origin = p.origin;
                }
                pawn.mv.velocity = [0.0; 3];
            }
            pawn.tick_base = pawn.tick_base.wrapping_add(1);
            pawn.sim_tick = tick;
            tracing::trace!(
                cmd = c.command_number,
                fwd = c.forward_move,
                side = c.side_move,
                buttons = c.buttons,
                origin = ?pawn.mv.origin,
                "command executed"
            );
        }
    }

    /// This client's player entity, if spawned.
    pub fn player_entity(&self, game: &GameInfo) -> Option<EntityState> {
        self.pawn
            .as_ref()
            .map(|p| entities::player(&game.net, self.slot, self.serial, p, &p.params))
    }

    pub fn player_info(&self) -> Vec<u8> {
        let guid = self
            .claimed_steam_id
            .map(signon::steam2_id)
            .unwrap_or_else(|| "STEAM_ID_LAN".to_string());
        let friends_id = self
            .claimed_steam_id
            .map_or(0, |id| (id & 0xFFFF_FFFF) as u32);
        signon::player_info(&self.name, self.user_id, &guid, friends_id, false)
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
        if let Some(h) = &inc.header {
            self.note_ack(h.ack, now);
        }
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

    /// Sends dynamic string-table entries the client is missing.
    pub fn sync_tables(&mut self, game: &GameInfo) {
        let Some(known) = self.tables_known else { return };
        let mut w = BitWriter::with_capacity(4096);
        match signon::table_updates(game, known, &mut w) {
            Ok(now) if now != known => {
                self.tables_known = Some(now);
                self.queue(Ok(w));
            }
            Ok(_) => {}
            Err(e) => warn!(addr = %self.addr, error = %e, "string table update failed"),
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
                    if self.user_info.len() < 4096 && k.len() < 128 && v.len() < 1024 {
                        self.user_info.insert(k.to_ascii_lowercase(), v.clone());
                        self.user_info_dirty = true;
                    }
                    if k == "name" && !v.is_empty() {
                        self.name = v.chars().take(127).collect();
                    }
                    if k == "fov_desired" {
                        if let Ok(f) = v.trim().parse::<f32>() {
                            self.fov_desired = Some(f.clamp(75.0, 100.0));
                        }
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
                            // World signon: string tables now, send tables / class
                            // info at PRESPAWN, entities from SPAWN on.
                            self.queue(signon::tables_batch(game, tick, players));
                            let d = game.dynamic.borrow();
                            self.tables_known = Some((d.models.len(), d.network_strings.len()));
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
                        self.spawn(game, tick);
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
                match cmd.split_whitespace().next().unwrap_or("") {
                    "noclip" => self.toggle_noclip(),
                    // Developer teleport (LAN-only server; a cheat command in Source).
                    "setpos" => self.setpos(&cmd),
                    "status" | "ping" if self.pending_commands.len() < 8 => {
                        self.pending_commands.push(cmd);
                    }
                    _ if self.lua_commands.len() < 64 => self.lua_commands.push(cmd),
                    _ => {}
                }
            }
            ClientMessage::GModClientToServer { bits, data } => {
                if self.gmod_packets.len() < 256 {
                    self.gmod_packets.push((data, bits));
                }
            }
            ClientMessage::Tick { tick } => {
                if self.snap_log < 400 {
                    info!(target: "snapdbg", addr = %self.addr, server_tick = self.last_snapshot_tick, client_delta = tick, signon = self.signon, "net_Tick from client");
                }
                self.delta_tick = tick;
            }
            ClientMessage::Move {
                new_commands,
                backup_commands,
                bits,
                data,
            } => {
                let (cmds, err) = usercmd::read_move(&data, bits, new_commands + backup_commands);
                if let Some(e) = err {
                    debug!(addr = %self.addr, error = %e, decoded = cmds.len(), "usercmd decode stopped");
                    // Raw dump for format research (enable with RUST_LOG=usercmd_raw=debug).
                    let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
                    debug!(target: "usercmd_raw", new_commands, backup_commands, bits, %hex, "undecoded usercmds");
                }
                self.run_commands(game, tick, &cmds);
            }
            _ => {}
        }
    }

    /// Unreliable messages for this tick.
    fn unreliable_for_tick(
        &mut self,
        game: &GameInfo,
        tick: i32,
        world: &[EntityState],
    ) -> srcds_protocol::error::Result<Option<BitWriter>> {
        if self.signon < netmsg::SIGNONSTATE_SPAWN {
            return Ok(None);
        }
        let snap = Snapshot {
            tick,
            entities: world.to_vec(),
        };
        // Delta against the snapshot the client last reported, if we still have it;
        // otherwise a full update (also the first snapshot, which moves the client
        // from SPAWN to FULL).
        //
        // [факт] (live client, slow first load) a full update makes the client drop
        // all older frames: with a full update sent every tick while the client was
        // still loading, its late report of tick 2983 arrived after it had parsed
        // full updates 2984..3002, the delta from 2983 hit "Update delta not found."
        // and the client crashed. So only one full update is in flight: a delta
        // base must not be older than the last full update, and after a full update
        // the server waits for the client to confirm it (resending after a while).
        let base_ok = self.delta_tick >= 0 && self.delta_tick >= self.last_full_tick;
        let from = base_ok
            .then(|| self.snapshots.iter().find(|s| s.tick == self.delta_tick))
            .flatten();
        if from.is_none() {
            let waiting = self.last_full_tick >= 0 && tick.wrapping_sub(self.last_full_tick) < FULL_UPDATE_RETRY_TICKS;
            if waiting {
                return Ok(None);
            }
            self.last_full_tick = tick;
        }
        let enc = entities::encode(&game.net, &snap, from)?;
        if self.snap_log < 400 {
            self.snap_log += 1;
            info!(
                target: "snapdbg",
                addr = %self.addr,
                tick,
                client_delta = self.delta_tick,
                delta_from = from.map_or(-1, |f| f.tick),
                found = from.is_some(),
                history = self.snapshots.len(),
                oldest = self.snapshots.front().map_or(-1, |s| s.tick),
                entities = snap.entities.len(),
                updated = enc.updated,
                bits = enc.data.bit_len(),
                "snapshot"
            );
        }
        self.last_snapshot_tick = tick;
        let mut w = BitWriter::with_capacity(512 + enc.data.byte_len());
        netmsg::write_tick(&mut w, tick, game.tick_interval, 0.0)?;
        netmsg::write_packet_entities(
            &mut w,
            &PacketEntities {
                max_entries: enc.max_entries,
                delta_from: from.map(|f| f.tick),
                baseline: 0,
                updated_entries: enc.updated,
                update_baseline: false,
                data: enc.data.as_bytes(),
                data_bits: enc.data.bit_len(),
            },
        )?;
        self.snapshots.push_back(snap);
        while self.snapshots.len() > SNAPSHOT_HISTORY {
            self.snapshots.pop_front();
        }
        if !self.sent_first_snapshot {
            let angles = self.pawn.as_ref().map_or([0.0; 3], |p| p.angles);
            netmsg::write_fix_angle(&mut w, false, angles)?;
            self.sent_first_snapshot = true;
        }
        Ok(Some(w))
    }

    /// Builds this tick's datagrams (header + pending reliable data + unreliable).
    pub fn transmit(&mut self, game: &GameInfo, tick: i32, world: &[EntityState]) -> Vec<Vec<u8>> {
        // entities whose model the client may not have precached yet wait until the
        // string-table update is acknowledged
        if !self.chan.has_pending_reliable() {
            self.models_acked = self.tables_known.map_or(0, |k| k.0);
        }
        let limit = game.player_model_index() + self.models_acked as i32;
        let filtered: Vec<EntityState>;
        let model_pending = |e: &EntityState| e.model_index(&game.net).is_some_and(|m| m > limit);
        // a rope waits for both end entities: the client resolves them once, when the
        // rope is created
        let held: std::collections::HashSet<u32> = world.iter().filter(|e| model_pending(e)).map(|e| e.index).collect();
        // (rope materials are precached before signon, see LuaGame::start)
        let pending = |e: &EntityState| {
            model_pending(e)
                || e.rope_refs(&game.net)
                    .is_some_and(|(_, a, b)| held.contains(&((a & 0x1FFF) as u32)) || held.contains(&((b & 0x1FFF) as u32)))
        };
        let world = if world.iter().any(pending) {
            filtered = world
                .iter()
                .filter_map(|e| {
                    if e.class == entities::CLASS_PLAYER {
                        // players never leave the snapshot: until the client has their
                        // model, they keep the default one
                        let mut e = e.clone();
                        if model_pending(&e) {
                            e.set(&game.net, "DT_BaseEntity", "m_nModelIndex", srcds_protocol::sendtable::PropValue::Int(game.player_model_index()));
                        }
                        return Some(e);
                    }
                    (!pending(e)).then(|| e.clone())
                })
                .collect();
            &filtered[..]
        } else {
            world
        };
        let unrel = match self.unreliable_for_tick(game, tick, world) {
            Ok(u) => u,
            Err(e) => {
                warn!(addr = %self.addr, error = %e, "failed to build snapshot");
                None
            }
        };
        let d = match self.chan.transmit(unrel.as_ref()) {
            Ok(d) => {
                self.sent_at
                    .push_back((self.chan.out_sequence(), Instant::now()));
                if self.sent_at.len() > 256 {
                    self.sent_at.pop_front();
                }
                d
            }
            Err(e) => {
                warn!(addr = %self.addr, error = %e, "netchannel transmit failed");
                return Vec::new();
            }
        };
        if self.snap_log < 400 && (d.len() > MAX_UNSPLIT || self.chan.has_pending_reliable()) {
            info!(target: "snapdbg", addr = %self.addr, tick, bytes = d.len(), split = d.len() > MAX_UNSPLIT, reliable_pending = self.chan.has_pending_reliable(), "datagram");
        }
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
