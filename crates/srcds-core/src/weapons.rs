//! Player weapons on the server: switching (`weaponselect`), the physics gun (a
//! C++ weapon in GMod, implemented here), and networking of weapon / view-model
//! entities.
//!
//! Physgun behaviour follows the sandbox hooks: `PhysgunPickup` decides whether an
//! entity may be grabbed, `OnPhysgunPickup` / `PhysgunDrop` are notified,
//! `OnPhysgunFreeze` handles right click while holding and `OnPhysgunReload` the
//! reload key. The held entity follows the aim point kinematically and keeps its
//! velocity when released (so it can be thrown).

use std::collections::HashMap;
use std::sync::Arc;

use srcds_lua::mlua::{self, AnyUserData, ObjectLike, Table, Value};
use srcds_lua::entity::{self, store, with, EntRef, EntUd};
use srcds_lua::vecang::{angle_vectors, rotate, Vector};
use srcds_lua::GLua;

use crate::entities::{self, ViewModelState, WeaponState};
use srcds_protocol::sendtable::PropValue;
use crate::signon::GameInfo;
use crate::vpk::Content;

pub const IN_ATTACK: i32 = 1 << 0;
pub const IN_USE: i32 = 1 << 5;
pub const IN_ATTACK2: i32 = 1 << 11;
pub const IN_RELOAD: i32 = 1 << 13;

const PHYSGUN_RANGE: f32 = 4096.0;
const PHYSGUN_MIN_DIST: f32 = 32.0;
const WHEEL_STEP: f32 = 20.0;
const ROTATE_SCALE: f32 = 0.1;

/// View / world models of engine weapons (from `scripts/weapons/<class>.txt`).
pub struct WeaponModels {
    content: Arc<Content>,
    cache: HashMap<String, (String, String)>,
    sequences: HashMap<String, (i32, f32)>,
}

impl WeaponModels {
    pub fn new(content: Arc<Content>) -> Self {
        Self {
            content,
            cache: HashMap::new(),
            sequences: HashMap::new(),
        }
    }

    /// (view model, world model) of an engine weapon class.
    pub fn get(&mut self, class: &str) -> (String, String) {
        if let Some(m) = self.cache.get(class) {
            return m.clone();
        }
        let text = self
            .content
            .read(&format!("scripts/weapons/{class}.txt"), 64 * 1024)
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_default();
        let key = |k: &str| -> String {
            // "key" "value" pairs, first occurrence
            let mut it = text.split('"').skip(1).step_by(2);
            while let Some(t) = it.next() {
                if t.eq_ignore_ascii_case(k) {
                    return it.next().unwrap_or("").to_string();
                }
            }
            String::new()
        };
        let m = (key("viewmodel"), key("playermodel"));
        self.cache.insert(class.to_string(), m.clone());
        m
    }

    /// First sequence of `model` with `activity` and its duration (-1 when none).
    pub fn sequence_info(&mut self, model: &str, activity: &str) -> (i32, f32) {
        let key = format!("{model}|{activity}");
        if let Some(i) = self.sequences.get(&key) {
            return *i;
        }
        let seqs = self.content.model_sequences(model);
        let i = seqs
            .iter()
            .position(|s| s.activity.eq_ignore_ascii_case(activity))
            .map_or((-1, 0.0), |i| (i as i32, seqs[i].duration));
        self.sequences.insert(key, i);
        i
    }
}

/// Physgun state of one player.
#[derive(Debug, Clone, Copy)]
pub struct Grab {
    pub ent: EntRef,
    pub dist: f32,
    /// Grab point in the entity's local space.
    pub local: [f32; 3],
    /// The entity's forward and up axes in the player's yaw frame (x = where the
    /// player faces horizontally, z = up): the object turns with the player.
    pub rel_f: [f32; 3],
    pub rel_u: [f32; 3],
}

/// `v` turned by `deg` around the z axis.
fn rot_z(v: [f32; 3], deg: f32) -> [f32; 3] {
    let (s, c) = deg.to_radians().sin_cos();
    [v[0] * c - v[1] * s, v[0] * s + v[1] * c, v[2]]
}

/// `v` turned by `deg` around the y axis (the player's left in the yaw frame);
/// positive lifts the side nearest the player.
fn rot_y(v: [f32; 3], deg: f32) -> [f32; 3] {
    let (s, c) = deg.to_radians().sin_cos();
    [v[0] * c + v[2] * s, v[1], -v[0] * s + v[2] * c]
}

fn norm(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
    [v[0] / l, v[1] / l, v[2] / l]
}

/// Keeps `u` perpendicular to `f`, both unit length.
fn orthonormal(f: [f32; 3], u: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    let f = norm(f);
    let d = f[0] * u[0] + f[1] * u[1] + f[2] * u[2];
    (f, norm([u[0] - f[0] * d, u[1] - f[1] * d, u[2] - f[2] * d]))
}

/// Physgun rotation for a mouse move while `use` is held: horizontal movement
/// turns the object around the vertical axis, vertical movement around the
/// player's right axis ([факт] live client: mouse up lifts the near side).
pub fn rotate_held(rel_f: [f32; 3], rel_u: [f32; 3], mouse: (i32, i32)) -> ([f32; 3], [f32; 3]) {
    let yaw = mouse.0 as f32 * ROTATE_SCALE;
    let pitch = -(mouse.1 as f32) * ROTATE_SCALE;
    let f = rot_y(rot_z(rel_f, yaw), pitch);
    let u = rot_y(rot_z(rel_u, yaw), pitch);
    orthonormal(f, u)
}

#[derive(Default)]
pub struct WeaponSystem {
    pub grabs: HashMap<EntRef, Grab>,
    /// View-model animation parity per player (bumped on weapon switch).
    pub parity: HashMap<EntRef, i32>,
    /// Earliest next shot of engine guns per player.
    pub next_fire: HashMap<EntRef, f64>,
    /// Physgun beam entity per player.
    pub beams: HashMap<EntRef, EntRef>,
    /// Object each player holds with the gravity gun.
    pub cannon_holds: HashMap<EntRef, CannonHold>,
    /// Flying bolts and grenades.
    pub projectiles: Vec<crate::weapon_sim::Projectile>,
}

/// An object held by the gravity gun.
#[derive(Debug, Clone, Copy)]
pub struct CannonHold {
    pub ent: EntRef,
    /// Distance of the hold point from the eye.
    pub dist: f32,
    /// The object's bounds centre relative to its origin.
    pub center_offset: [f32; 3],
}

pub(crate) fn ud(glua: &GLua, r: EntRef) -> mlua::Result<Value> {
    mlua::IntoLua::into_lua(r, &glua.lua)
}

fn hook_true(glua: &GLua, name: &str, args: impl mlua::IntoLuaMulti) -> bool {
    let out = glua.hook_call(name, args);
    matches!(out.into_iter().next(), Some(Value::Boolean(true)))
}

pub(crate) fn hook_is_false(glua: &GLua, name: &str, args: impl mlua::IntoLuaMulti) -> bool {
    let out = glua.hook_call(name, args);
    matches!(out.into_iter().next(), Some(Value::Boolean(false)))
}

fn class_of(glua: &GLua, r: EntRef) -> String {
    store(&glua.lua).ok().and_then(|s| s.get(r).map(|e| e.class.clone())).unwrap_or_default()
}

/// Calls `SWEP:<name>(args)` when the weapon is scripted; returns the first result.
fn swep_call(glua: &GLua, w: EntRef, name: &str) -> Option<Value> {
    let t = entity::ent_table(&glua.lua, w, false).ok().flatten()?;
    let f = match t.get::<Value>(name) {
        Ok(Value::Function(f)) => f,
        _ => return None,
    };
    match f.call::<Value>(ud(glua, w).ok()?) {
        Ok(v) => Some(v),
        Err(e) => {
            srcds_lua::runtime::report(&glua.state, &e);
            None
        }
    }
}

impl WeaponSystem {
    /// Switches `ply` to the weapon entity `index` if it owns it. Returns false when it
    /// does not own it (yet: the selection may arrive before the weapon is given).
    pub fn select(&mut self, glua: &GLua, models: &mut WeaponModels, ply: EntRef, index: u32, now: f64) -> bool {
        let Ok((weapons, active)) = entity::with_player(&glua.lua, ply, |p, _| (p.weapons.clone(), p.active_weapon)) else {
            return true;
        };
        let Some(new) = weapons.into_iter().find(|w| w.id == index) else { return false };
        if new == active {
            return true;
        }
        let (Ok(p), Ok(o), Ok(n)) = (ud(glua, ply), ud(glua, active), ud(glua, new)) else { return true };
        if hook_true(glua, "PlayerSwitchWeapon", (p, o, n)) {
            return true; // blocked
        }
        if !active.is_null() {
            if let Some(Value::Boolean(false)) = swep_call(glua, active, "Holster") {
                return true;
            }
            self.release(glua, ply);
            // the base holster cancels a reload in progress
            let _ = with(&glua.lua, active, |e| {
                e.nw.remove("__srcds_in_reload");
            });
            // [факт] live client: it predicts `m_hLastWeapon` = the weapon put away
            let h = entities::ehandle(active.id, active.serial);
            let _ = with(&glua.lua, ply, |e| {
                e.nw.insert("__srcds_last_weapon".into(), srcds_lua::entity::NetValue::Int(h as i64));
            });
        }
        let _ = entity::with_player(&glua.lua, ply, |p, _| p.active_weapon = new);
        // the new view model draws (Deploy may choose another activity); attacks wait
        // for the draw to end
        let vm = entity::with_player(&glua.lua, ply, |p, _| p.view_model).unwrap_or(EntRef::NULL);
        entity::set_view_model_activity(&glua.lua, vm, "ACT_VM_DRAW");
        swep_call(glua, new, "Deploy");
        let path = crate::weapon_sim::view_model_path(glua, models, new);
        let pending = entity::with(&glua.lua, vm, |e| e.nw.contains_key("__srcds_vm_pending")).unwrap_or(false);
        crate::weapon_sim::resolve_lua_anims(glua, models, new, vm, &path, now);
        if pending {
            // [факт] live client (`cl_showerror 2`): after a draw it predicts both next
            // attacks and the owner's `m_flNextAttack` at the draw's end (= the idle time)
            let idle = store(&glua.lua).ok().and_then(|s| s.get(new).and_then(|e| match e.nw.get("__srcds_idle") {
                Some(srcds_lua::entity::NetValue::Float(f)) => Some(*f),
                _ => None,
            }));
            if let Some(end) = idle {
                let _ = with(&glua.lua, new, |e| {
                    e.nw.insert("__srcds_npf".into(), srcds_lua::entity::NetValue::Float(end));
                    e.nw.insert("__srcds_nsf".into(), srcds_lua::entity::NetValue::Float(end));
                });
                let _ = with(&glua.lua, ply, |e| {
                    e.nw.insert("__srcds_next_attack".into(), srcds_lua::entity::NetValue::Float(end));
                });
            }
        }
        true
    }

    /// Drops whatever `ply` holds with the physgun.
    pub fn release(&mut self, glua: &GLua, ply: EntRef) {
        if let Some(g) = self.grabs.remove(&ply) {
            let _ = with(&glua.lua, g.ent, |d| {
                let mut frozen = false;
                if let Some(p) = &mut d.phys {
                    p.held = false;
                    p.asleep = false;
                    frozen = !p.motion;
                }
                // a frozen prop let go stays where it was put
                if frozen {
                    d.vel = [0.0; 3];
                }
            });
            if entity::valid(&glua.lua, g.ent) {
                if let (Ok(p), Ok(e)) = (ud(glua, ply), ud(glua, g.ent)) {
                    glua.hook_call("PhysgunDrop", (p, e));
                }
            }
        }
    }

    /// One tick of the physics gun for `ply` (`eye` = shoot position, `angles` = view).
    #[allow(clippy::too_many_arguments)]
    pub fn physgun(
        &mut self,
        glua: &GLua,
        ply: EntRef,
        eye: [f32; 3],
        angles: [f32; 3],
        buttons: i32,
        old_buttons: i32,
        wheel: i32,
        mouse: (i32, i32),
        dt: f32,
    ) {
        let weapon = entity::with_player(&glua.lua, ply, |p, _| p.active_weapon).unwrap_or(EntRef::NULL);
        if weapon.is_null() || class_of(glua, weapon) != "weapon_physgun" {
            self.release(glua, ply);
            return;
        }
        let pressed = |b: i32| buttons & b != 0 && old_buttons & b == 0;
        // reload: sandbox unfreezes the player's props
        if pressed(IN_RELOAD) {
            if let (Ok(w), Ok(p)) = (ud(glua, weapon), ud(glua, ply)) {
                glua.hook_call("OnPhysgunReload", (w, p));
            }
        }
        // right click while holding: freeze and let go
        if pressed(IN_ATTACK2) {
            if let Some(g) = self.grabs.get(&ply).copied() {
                if entity::valid(&glua.lua, g.ent) {
                    self.freeze(glua, ply, weapon, g.ent);
                }
                self.grabs.remove(&ply);
                return;
            }
        }
        // [факт] reference `CWeaponPhysGun::ItemPostFrame`: the beam works only while
        // primary fire is held without alt-fire (`buttons & 0x801 == IN_ATTACK`),
        // otherwise it lets go
        if buttons & (IN_ATTACK | IN_ATTACK2) != IN_ATTACK {
            self.release(glua, ply);
            return;
        }
        let fwd = angle_vectors(angles).0;
        if let Some(g) = self.grabs.get(&ply).copied() {
            if !entity::valid(&glua.lua, g.ent) {
                self.grabs.remove(&ply);
                return;
            }
            let mut g = g;
            g.dist = (g.dist + wheel as f32 * WHEEL_STEP).max(PHYSGUN_MIN_DIST);
            if buttons & IN_USE != 0 && mouse != (0, 0) {
                (g.rel_f, g.rel_u) = rotate_held(g.rel_f, g.rel_u, mouse);
            }
            let target = [eye[0] + fwd[0] * g.dist, eye[1] + fwd[1] * g.dist, eye[2] + fwd[2] * g.dist];
            let ang = entity::angles_from_basis(rot_z(g.rel_f, angles[1]), rot_z(g.rel_u, angles[1]));
            let _ = with(&glua.lua, g.ent, |e| {
                let off = rotate(g.local, ang);
                let desired = [target[0] - off[0], target[1] - off[1], target[2] - off[2]];
                for k in 0..3 {
                    e.vel[k] = ((desired[k] - e.pos[k]) / dt).clamp(-3000.0, 3000.0);
                }
                e.pos = desired;
                e.ang = ang;
                // a frozen prop is moved but stays frozen
                if let Some(p) = &mut e.phys {
                    p.asleep = false;
                    p.held = true;
                    p.ang_vel = [0.0; 3];
                }
            });
            self.grabs.insert(ply, g);
            return;
        }
        // pick up: while the trigger is held, the beam catches what it sweeps onto
        // ([факт] live client: it predicted a grab the server had not made —
        // `m_hGrabbedEntity net NULL pred <entity>`)
        // pick up: trace along the aim
        let hit = (|| -> mlua::Result<Option<(EntRef, [f32; 3])>> {
            let util: Table = glua.lua.globals().get("util")?;
            let tl: mlua::Function = util.get("TraceLine")?;
            let t = glua.lua.create_table()?;
            t.set("start", Vector(eye))?;
            t.set(
                "endpos",
                Vector([eye[0] + fwd[0] * PHYSGUN_RANGE, eye[1] + fwd[1] * PHYSGUN_RANGE, eye[2] + fwd[2] * PHYSGUN_RANGE]),
            )?;
            t.set("filter", ud(glua, ply)?)?;
            let tr: Table = tl.call(t)?;
            let ent = match tr.get::<Value>("Entity")? {
                Value::UserData(u) => u.borrow::<EntUd>().map(|e| e.0).unwrap_or(EntRef::NULL),
                _ => EntRef::NULL,
            };
            let pos: Vector = tr.get("HitPos")?;
            Ok((!ent.is_null() && ent.id != 0).then_some((ent, pos.0)))
        })();
        let Ok(Some((ent, hit_pos))) = hit else { return };
        // [факт] reference: any non-world entity the beam hits is grabbed unless the
        // gamemode's `PhysgunPickup` says no (the client predicted grabs of entities
        // without physics that this server refused); those move without physics
        let movable = store(&glua.lua).ok().and_then(|s| s.get(ent).map(|e| e.player.is_none() && !e.removing)).unwrap_or(false);
        if !movable {
            return;
        }
        let (Ok(p), Ok(e)) = (ud(glua, ply), ud(glua, ent)) else { return };
        if hook_is_false(glua, "PhysgunPickup", (p.clone(), e.clone())) {
            return;
        }
        let Ok((pos, ang)) = with(&glua.lua, ent, |d| (d.pos, d.ang)) else { return };
        let local = entity::unrotate([hit_pos[0] - pos[0], hit_pos[1] - pos[1], hit_pos[2] - pos[2]], ang);
        let dist = {
            let d = [hit_pos[0] - eye[0], hit_pos[1] - eye[1], hit_pos[2] - eye[2]];
            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(PHYSGUN_MIN_DIST)
        };
        let (f, _, u) = angle_vectors(ang);
        let (rel_f, rel_u) = (rot_z(f, -angles[1]), rot_z(u, -angles[1]));
        self.grabs.insert(ply, Grab { ent, dist, local, rel_f, rel_u });
        let _ = with(&glua.lua, ent, |d| {
            if let Some(ph) = &mut d.phys {
                ph.asleep = false;
            }
        });
        glua.hook_call("OnPhysgunPickup", (p, e));
    }

    /// One shot of an engine (HL2) weapon for a command: spends a round, plays the
    /// attack animation and applies hitscan damage. Returns the refire delay, or
    /// `None` for a weapon this server does not fire.
    /// [гипотеза] refire delays / damage are approximations, not the game's values.
    #[allow(clippy::too_many_arguments)]
    pub fn engine_fire(
        &mut self,
        glua: &GLua,
        models: &mut WeaponModels,
        ply: EntRef,
        weapon: EntRef,
        vm: EntRef,
        path: &str,
        class: &str,
        cmd: &crate::weapon_sim::WeaponCmd,
        killed: &mut Vec<EntRef>,
    ) -> Option<f64> {
        // (refire s, automatic, damage, pellets, range, spread)
        let spec = match class {
            "weapon_pistol" => (0.12, false, 8.0, 1, 8192.0, 0.01),
            "weapon_smg1" => (0.075, true, 5.0, 1, 8192.0, 0.04),
            "weapon_ar2" => (0.1, true, 11.0, 1, 8192.0, 0.02),
            "weapon_357" => (0.75, false, 40.0, 1, 8192.0, 0.0),
            "weapon_shotgun" => (0.9, false, 8.0, 7, 4096.0, 0.08),
            "weapon_crossbow" => (1.6, false, 100.0, 1, 8192.0, 0.0),
            "weapon_crowbar" => (0.4f32 as f64, true, 10.0, 1, 75.0, 0.0),
            "weapon_stunstick" => (0.8f32 as f64, true, 10.0, 1, 75.0, 0.0),
            _ => return None,
        };
        let (interval, auto, damage, pellets, range, spread) = spec;
        if !auto && cmd.old_buttons & IN_ATTACK != 0 {
            return None;
        }
        let max_clip = entity::with(&glua.lua, weapon, |e| e.max_clip1).unwrap_or(-1);
        if max_clip > 0 {
            let _ = entity::with(&glua.lua, weapon, |e| e.clip1 = (e.clip1 - 1).max(0));
        }
        let any_hit = fire_bullets(glua, ply, cmd, pellets, damage, range, spread, killed);
        let act = match class {
            "weapon_crowbar" | "weapon_stunstick" => if any_hit { "ACT_VM_HITCENTER" } else { "ACT_VM_MISSCENTER" },
            _ => "ACT_VM_PRIMARYATTACK",
        };
        crate::weapon_sim::send_anim(glua, models, weapon, vm, path, act, cmd.time);
        Some(interval)
    }

    fn freeze(&mut self, glua: &GLua, ply: EntRef, weapon: EntRef, ent: EntRef) {
        self.grabs.remove(&ply);
        let _ = with(&glua.lua, ent, |d| {
            if let Some(p) = &mut d.phys {
                p.held = false;
            }
        });
        let run = || -> mlua::Result<()> {
            let e = ud(glua, ent)?;
            let phys: Value = match &e {
                Value::UserData(u) => AnyUserData::call_method(u, "GetPhysicsObject", ())?,
                _ => Value::Nil,
            };
            let handled = hook_true(glua, "OnPhysgunFreeze", (ud(glua, weapon)?, phys, e.clone(), ud(glua, ply)?));
            if !handled {
                // default: stop the object where it is
                let _ = with(&glua.lua, ent, |d| {
                    d.vel = [0.0; 3];
                    if let Some(p) = &mut d.phys {
                        p.motion = false;
                    }
                });
            }
            glua.hook_call("PhysgunDrop", (ud(glua, ply)?, e));
            Ok(())
        };
        if let Err(e) = run() {
            srcds_lua::runtime::report(&glua.state, &e);
        }
    }

    /// Weapon and view-model entities of all players, plus each player's inventory.
    pub fn network(
        &mut self,
        glua: &GLua,
        game: &GameInfo,
        models: &mut WeaponModels,
        tick: i32,
    ) -> (Vec<entities::EntityState>, HashMap<EntRef, entities::Inventory>) {
        let mut out = Vec::new();
        let mut inv = HashMap::new();
        // a beam entity for every player holding a physgun
        if let Ok(mut st) = store(&glua.lua) {
            let holders: Vec<EntRef> = st
                .players()
                .filter(|e| e.player.as_ref().is_some_and(|p| p.weapons.iter().any(|w| st.get(*w).is_some_and(|we| we.class == "weapon_physgun"))))
                .map(|e| e.r)
                .collect();
            for ply in holders {
                if !self.beams.get(&ply).is_some_and(|b| st.get(*b).is_some()) {
                    if let Some(b) = st.create("physgun_beam", None) {
                        if let Some(e) = st.get_mut(b) {
                            e.spawned = true;
                            e.owner = ply;
                        }
                        self.beams.insert(ply, b);
                    }
                }
            }
        }
        let beam_ends: HashMap<EntRef, [f32; 3]> = self
            .beams
            .keys()
            .filter_map(|ply| {
                let (pos, ang, crouch, on) = entity::with_player(&glua.lua, *ply, |p, pos| (*pos, p.eye_angles, p.crouching, p.buttons & IN_ATTACK != 0)).ok()?;
                if !on {
                    return None;
                }
                let eye = [pos[0], pos[1], pos[2] + if crouch { 28.0 } else { 64.0 }];
                let f = angle_vectors(ang).0;
                let end = [eye[0] + f[0] * PHYSGUN_RANGE, eye[1] + f[1] * PHYSGUN_RANGE, eye[2] + f[2] * PHYSGUN_RANGE];
                let util: Table = glua.lua.globals().get("util").ok()?;
                let tl: mlua::Function = util.get("TraceLine").ok()?;
                let t = glua.lua.create_table().ok()?;
                t.set("start", Vector(eye)).ok()?;
                t.set("endpos", Vector(end)).ok()?;
                t.set("filter", ud(glua, *ply).ok()?).ok()?;
                let tr: Table = tl.call(t).ok()?;
                let hit: Vector = tr.get("HitPos").ok()?;
                Some((*ply, hit.0))
            })
            .collect();
        let Ok(s) = store(&glua.lua) else { return (out, inv) };
        let handle = |r: EntRef| if r.is_null() { entities::INVALID_EHANDLE } else { entities::ehandle(r.id, r.serial) };
        for pe in s.players() {
            let Some(p) = &pe.player else { continue };
            let owner = handle(pe.r);
            let mut active_view_model = 0;
            let mut weapons = Vec::new();
            // the view model's sequence and animation parity (weapon logic)
            let (vm_seq, vm_parity, vm_flash, vm_rate) = s.get(p.view_model).map_or((0, 0, 0, 1.0), |v| {
                let int = |k: &str| match v.nw.get(k) {
                    Some(srcds_lua::entity::NetValue::Int(n)) => *n as i32,
                    _ => 0,
                };
                let rate = match v.nw.get("__srcds_vm_rate") {
                    Some(srcds_lua::entity::NetValue::Float(f)) => *f as f32,
                    _ => 1.0,
                };
                (int("__srcds_vm_seq"), int("__srcds_vm_parity"), int("__srcds_mflash"), rate)
            });
            for w in &p.weapons {
                let Some(we) = s.get(*w) else { continue };
                let (view, world) = if we.scripted {
                    let t = we.table.as_ref().and_then(|k| glua.lua.registry_value::<Table>(k).ok());
                    let get = |k: &str| t.as_ref().and_then(|t| t.get::<Option<String>>(k).ok().flatten()).unwrap_or_default();
                    (get("ViewModel"), get("WorldModel"))
                } else {
                    models.get(&we.class)
                };
                let vm = game.precache_model(&view).unwrap_or(0);
                let wm = game.precache_model(&world).unwrap_or(0);
                let active = *w == p.active_weapon;
                if active {
                    active_view_model = vm;
                }
                let grab = self.grabs.get(&pe.r).filter(|_| active);
                let hold_type = if we.scripted {
                    we.table
                        .as_ref()
                        .and_then(|k| glua.lua.registry_value::<Table>(k).ok())
                        .and_then(|t| t.get::<Option<String>>("HoldType").ok().flatten())
                        .unwrap_or_else(|| "pistol".into())
                } else {
                    String::new()
                };
                let time = |k: &str| match we.nw.get(k) {
                    Some(srcds_lua::entity::NetValue::Float(f)) => *f,
                    _ => 0.0,
                };
                let mut state = entities::weapon(
                    &game.net,
                    &WeaponState {
                        index: w.id,
                        serial: w.serial,
                        class: entities::weapon_class(&we.class, we.scripted),
                        // carried: follows the owner (local origin 0)
                        origin: [0.0; 3],
                        angles: [0.0; 3],
                        next_primary: time("__srcds_npf"),
                        next_secondary: time("__srcds_nsf"),
                        idle_time: time("__srcds_idle"),
                        world_model: wm,
                        view_model: vm,
                        state: if active { entities::WEAPON_IS_ACTIVE } else { entities::WEAPON_IS_CARRIED },
                        owner,
                        clip1: we.clip1,
                        ammo1: if we.ammo1.is_empty() { -1 } else { srcds_lua::ammo::ammo_id(&glua.lua, &we.ammo1) },
                        ammo2: if we.ammo2.is_empty() { -1 } else { srcds_lua::ammo::ammo_id(&glua.lua, &we.ammo2) },
                        clip2: we.clip2,
                        sim_tick: tick,
                        script_name: if we.scripted { we.class.clone() } else { String::new() },
                        hold_type,
                        // [факт] reference: letting go writes handle 0 (the world) — the
                        // client predicted the world where this server sent "none"
                        grabbed: grab.map_or(0, |g| handle(g.ent)),
                        hit_local: grab.map_or([0.0; 3], |g| g.local),
                        beam: self.beams.get(&pe.r).map_or(entities::INVALID_EHANDLE, |b| handle(*b)),
                        cannon_effect: match we.nw.get("__srcds_pc_effect") {
                            Some(srcds_lua::entity::NetValue::Int(n)) => *n as i32,
                            _ => 2,
                        },
                        cannon_held: self
                            .cannon_holds
                            .get(&pe.r)
                            .filter(|_| we.class == "weapon_physcannon")
                            .map_or(entities::INVALID_EHANDLE, |h| handle(h.ent)),
                        cannon_open: matches!(we.nw.get("__srcds_pc_open"), Some(srcds_lua::entity::NetValue::Int(n)) if *n != 0),
                    },
                );
                entities::set_dtvars(&game.net, &mut state, &crate::game_lua::dtvars(we));
                // per-weapon predicted state (weapon logic)
                let int = |k: &str| match we.nw.get(k) {
                    Some(srcds_lua::entity::NetValue::Int(n)) => *n as i32,
                    _ => 0,
                };
                state.set(&game.net, "DT_BaseAnimating", "m_nMuzzleFlashParity", PropValue::Int(int("__srcds_mflash") & 3));
                if !we.scripted && crate::weapon_sim::is_machine_gun(&we.class) {
                    state.set(&game.net, "DT_HL2MPMachineGun", "m_nShotsFired", PropValue::Int(int("__srcds_shots")));
                }
                if !we.scripted && we.class == "weapon_shotgun" {
                    for (prop, key) in [
                        ("m_bNeedPump", "__srcds_sg_pump"),
                        ("m_bDelayedFire1", "__srcds_sg_fire1"),
                        ("m_bDelayedFire2", "__srcds_sg_fire2"),
                        ("m_bDelayedReload", "__srcds_sg_reload"),
                    ] {
                        state.set(&game.net, "DT_WeaponShotgun", prop, PropValue::Int((int(key) != 0) as i32));
                    }
                }
                if !we.scripted && we.class == "weapon_crossbow" {
                    state
                        .set(&game.net, "DT_WeaponCrossbow", "m_bInZoom", PropValue::Int((int("__srcds_xb_zoom") != 0) as i32))
                        .set(&game.net, "DT_WeaponCrossbow", "m_bMustReload", PropValue::Int((int("__srcds_xb_reload") != 0) as i32));
                }
                if !we.scripted && we.class == "weapon_pistol" {
                    state
                        .set(&game.net, "DT_WeaponPistol", "m_flSoonestPrimaryAttack", PropValue::Double(time("__srcds_soonest")))
                        .set(&game.net, "DT_WeaponPistol", "m_flLastAttackTime", PropValue::Double(time("__srcds_lastattack")))
                        .set(&game.net, "DT_WeaponPistol", "m_flAccuracyPenalty", PropValue::Float(time("__srcds_penalty") as f32))
                        .set(&game.net, "DT_WeaponPistol", "m_nNumShotsFired", PropValue::Int(int("__srcds_numshots")));
                }
                out.push(state);
                if we.class == "weapon_physgun" {
                    if let Some(b) = self.beams.get(&pe.r) {
                        let on = active && p.buttons & IN_ATTACK != 0;
                        let (target, hold) = match grab {
                            Some(g) => (handle(g.ent), g.local),
                            None => (entities::INVALID_EHANDLE, beam_ends.get(&pe.r).copied().unwrap_or(pe.pos)),
                        };
                        out.push(entities::phys_beam(&game.net, b.id, b.serial, pe.pos, handle(*w), on, target, hold));
                    }
                }
                weapons.push(handle(*w));
            }
            if !p.view_model.is_null() {
                out.push(entities::view_model(
                    &game.net,
                    &ViewModelState {
                        index: p.view_model.id,
                        serial: p.view_model.serial,
                        model: active_view_model,
                        weapon: handle(p.active_weapon),
                        owner,
                        sequence: vm_seq,
                        parity: vm_parity & 7,
                        hidden: p.active_weapon.is_null(),
                        rate: vm_rate,
                        muzzle_flash: vm_flash,
                    },
                ));
            }
            inv.insert(
                pe.r,
                entities::Inventory {
                    active: handle(p.active_weapon),
                    weapons,
                    view_model: handle(p.view_model),
                    ..Default::default()
                },
            );
        }
        (out, inv)
    }
}

#[cfg(test)]
mod rotate_tests {
    use super::*;

    /// Mouse up lifts the near side (far side goes down); mouse right turns the
    /// object counter-clockwise seen from above (its near side follows the mouse);
    /// the axes stay orthonormal.
    #[test]
    fn held_object_rotates_about_view_axes() {
        let (f, u) = rotate_held([1.0, 0.0, 0.0], [0.0, 0.0, 1.0], (0, -900));
        assert!((f[2] + 1.0).abs() < 1e-4 && f[0].abs() < 1e-4, "{f:?}");
        assert!((u[0] - 1.0).abs() < 1e-4, "{u:?}");
        let (f, _) = rotate_held([1.0, 0.0, 0.0], [0.0, 0.0, 1.0], (900, 0));
        assert!((f[1] - 1.0).abs() < 1e-4, "{f:?}");
        // after a yaw turn, pitch is still about the player's right axis
        let (f, u) = rotate_held([0.0, 1.0, 0.0], [0.0, 0.0, 1.0], (0, -900));
        assert!((f[1] - 1.0).abs() < 1e-4, "side axis unchanged {f:?}");
        assert!((u[0] - 1.0).abs() < 1e-4, "{u:?}");
    }
}

/// Fires `pellets` bullets from the command's eye along its view (a fixed spread
/// pattern), pushing physics objects and damaging what they hit; returns whether any
/// hit something.
#[allow(clippy::too_many_arguments)]
pub fn fire_bullets(
    glua: &GLua,
    ply: EntRef,
    cmd: &crate::weapon_sim::WeaponCmd,
    pellets: i32,
    damage: f32,
    range: f32,
    spread: f32,
    killed: &mut Vec<EntRef>,
) -> bool {
    let (eye, angles) = (cmd.eye, cmd.angles);
    let (fwd, right, up) = angle_vectors(angles);
    let mut any_hit = false;
    for i in 0..pellets {
            // deterministic spread pattern
            let a = i as f32 * 2.399;
            let s = if pellets > 1 { spread } else { spread * 0.5 };
            let dir = [
                fwd[0] + (right[0] * a.cos() + up[0] * a.sin()) * s,
                fwd[1] + (right[1] * a.cos() + up[1] * a.sin()) * s,
                fwd[2] + (right[2] * a.cos() + up[2] * a.sin()) * s,
            ];
            let hit = (|| -> mlua::Result<(bool, Option<EntRef>)> {
                let util: Table = glua.lua.globals().get("util")?;
                let tl: mlua::Function = util.get("TraceLine")?;
                let t = glua.lua.create_table()?;
                t.set("start", Vector(eye))?;
                t.set("endpos", Vector([eye[0] + dir[0] * range, eye[1] + dir[1] * range, eye[2] + dir[2] * range]))?;
                t.set("filter", ud(glua, ply)?)?;
                let tr: Table = tl.call(t)?;
                let ent = match tr.get::<Value>("Entity")? {
                    Value::UserData(u) => u.borrow::<EntUd>().map(|e| e.0).unwrap_or(EntRef::NULL),
                    _ => EntRef::NULL,
                };
                let did: bool = tr.get("Hit").unwrap_or(false);
                Ok((did, (!ent.is_null() && ent.id != 0).then_some(ent)))
            })();
            let Ok((did, ent)) = hit else { continue };
            any_hit |= did;
            let Some(ent) = ent else { continue };
            apply_hit(glua, ent, dir, damage, killed);
        }
    any_hit
}

/// Damage `damage` to `ent` coming along `dir`: pushes a movable physics object,
/// takes health (a player brought to 0 goes to `killed`).
pub fn apply_hit(glua: &GLua, ent: EntRef, dir: [f32; 3], damage: f32, killed: &mut Vec<EntRef>) {
    let _ = with(&glua.lua, ent, |e| {
        if let Some(p) = &mut e.phys {
            if p.motion {
                let k = damage * 30.0 / p.mass.max(1.0);
                for j in 0..3 {
                    e.vel[j] += dir[j] * k;
                }
                p.asleep = false;
            }
        }
        if e.health > 0 && (e.player.is_some() || e.max_health > 0) {
            e.health -= damage as i32;
            if e.health <= 0 && e.player.is_some() {
                killed.push(ent);
            }
        }
    });
}
