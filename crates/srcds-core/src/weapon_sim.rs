//! Per-command weapon logic, run with the same inputs and times the client uses to
//! predict it, so the predicted weapon and view-model state match what the server
//! sends.
//!
//! [факт] live client (`cl_showerror 2`): with weapon times / view-model animation
//! not simulated per command, `m_flTimeWeaponIdle`, `m_nSequence` and
//! `m_nAnimationParity` disagreed every tick and the client replayed its animation.
//!
//! Behaviour (own implementation of Source's base weapon rules): each command, a
//! finished reload fills the clip; the secondary or primary attack fires when its next
//! time has come; the reload key starts a reload; with no attack keys the weapon idles
//! once its idle time passes. Every weapon animation selects the view model's sequence
//! for the activity, bumps the animation parity and sets the idle time to now + the
//! sequence's duration.

use srcds_lua::entity::{self, store, with, EntRef, NetValue};
use srcds_lua::mlua::{Table, Value};
use srcds_lua::GLua;

use crate::weapons::{WeaponModels, WeaponSystem, IN_ATTACK, IN_ATTACK2, IN_RELOAD};

/// One user command's weapon input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeaponCmd {
    /// The command's time: tick base × tick interval (the client's prediction time).
    pub time: f64,
    pub buttons: i32,
    pub old_buttons: i32,
    pub eye: [f32; 3],
    pub angles: [f32; 3],
}

/// Command time for a tick base, as the client computes it (float interval).
pub fn command_time(tick_base: i32, interval: f32) -> f64 {
    tick_base as f64 * interval as f64
}

fn nw_time(glua: &GLua, r: EntRef, key: &str) -> f64 {
    store(&glua.lua)
        .ok()
        .and_then(|s| s.get(r).and_then(|e| match e.nw.get(key) {
            Some(NetValue::Float(f)) => Some(*f),
            _ => None,
        }))
        .unwrap_or(0.0)
}

fn set_nw(glua: &GLua, r: EntRef, key: &str, v: NetValue) {
    let _ = with(&glua.lua, r, |e| {
        e.nw.insert(key.into(), v);
    });
}

fn has_nw(glua: &GLua, r: EntRef, key: &str) -> bool {
    store(&glua.lua).ok().is_some_and(|s| s.get(r).is_some_and(|e| e.nw.contains_key(key)))
}

/// The view model a weapon uses (SWEP `ViewModel` or the weapon script's).
pub fn view_model_path(glua: &GLua, models: &mut WeaponModels, w: EntRef) -> String {
    let Ok(s) = store(&glua.lua) else { return String::new() };
    let Some(e) = s.get(w) else { return String::new() };
    if e.scripted {
        let t = e.table.as_ref().and_then(|k| glua.lua.registry_value::<Table>(k).ok());
        t.and_then(|t| t.get::<Option<String>>("ViewModel").ok().flatten()).unwrap_or_default()
    } else {
        let class = e.class.clone();
        drop(s);
        models.get(&class).0
    }
}

/// Plays `activity` on the owner's view model; returns the sequence's duration.
pub fn send_anim(glua: &GLua, models: &mut WeaponModels, w: EntRef, vm: EntRef, path: &str, activity: &str, t: f64) -> f32 {
    let (seq, dur) = models.sequence_info(path, activity);
    if seq < 0 {
        return 0.0;
    }
    // [факт] reference SendWeaponAnim: after a draw the idle time is the sequence's
    // length divided by the deploy speed (`sv_defaultdeployspeed`), and the view model
    // plays it that much faster
    let rate = if activity.eq_ignore_ascii_case("ACT_VM_DRAW") { DEPLOY_SPEED } else { 1.0 };
    let dur = dur / rate;
    let _ = with(&glua.lua, vm, |e| {
        let n = match e.nw.get("__srcds_vm_parity") {
            Some(NetValue::Int(n)) => *n + 1,
            _ => 1,
        };
        e.nw.insert("__srcds_vm_parity".into(), NetValue::Int(n));
        e.nw.insert("__srcds_vm_seq".into(), NetValue::Int(seq as i64));
        e.nw.insert("__srcds_vm_rate".into(), NetValue::Float(rate as f64));
    });
    set_nw(glua, w, "__srcds_idle", NetValue::Float(t + dur as f64));
    dur
}

/// `sv_defaultdeployspeed` default ([факт] reference server.dll).
pub const DEPLOY_SPEED: f32 = 4.0;

/// Seconds per command (the tick interval), for per-frame decays.
pub const FRAME_TIME: f64 = 0.015f32 as f64;

/// Applies animations Lua asked for (`SendWeaponAnim`, `DefaultReload`) at time `t`.
pub fn resolve_lua_anims(glua: &GLua, models: &mut WeaponModels, w: EntRef, vm: EntRef, path: &str, t: f64) {
    let pending = with(&glua.lua, vm, |e| match e.nw.remove("__srcds_vm_pending") {
        Some(NetValue::Str(a)) => Some(a),
        _ => None,
    })
    .ok()
    .flatten();
    let Some(act) = pending else { return };
    let dur = send_anim(glua, models, w, vm, path, &act, t);
    // a reload: the next attacks wait for its end, then the clip fills
    if with(&glua.lua, w, |e| e.nw.remove("__srcds_reload_start").is_some()).unwrap_or(false) {
        let end = t + dur as f64;
        set_nw(glua, w, "__srcds_npf", NetValue::Float(end));
        set_nw(glua, w, "__srcds_nsf", NetValue::Float(end));
    }
}

fn swep_call(glua: &GLua, w: EntRef, name: &str) {
    let Ok(Some(t)) = entity::ent_table(&glua.lua, w, false) else { return };
    if let Ok(Value::Function(f)) = t.get::<Value>(name) {
        if let Err(e) = f.call::<()>(w) {
            srcds_lua::runtime::report(&glua.state, &e);
        }
    }
}

fn swep_automatic(glua: &GLua, w: EntRef, key: &str) -> bool {
    entity::ent_table(&glua.lua, w, false)
        .ok()
        .flatten()
        .and_then(|t| t.get::<Table>(key).ok())
        .and_then(|p| p.get::<Option<bool>>("Automatic").ok().flatten())
        .unwrap_or(false)
}

fn set_lua_time(glua: &GLua, t: f64) -> Option<f64> {
    let mut e = glua.lua.app_data_mut::<srcds_lua::lib_misc::Engine>()?;
    let old = e.cur_time;
    e.cur_time = t;
    Some(old)
}

fn nw_int(glua: &GLua, r: EntRef, key: &str) -> i64 {
    store(&glua.lua)
        .ok()
        .and_then(|s| s.get(r).and_then(|e| match e.nw.get(key) {
            Some(NetValue::Int(n)) => Some(*n),
            _ => None,
        }))
        .unwrap_or(0)
}

/// Bumps an entity's 2-bit muzzle-flash counter.
fn muzzle_flash(glua: &GLua, r: EntRef) {
    let n = nw_int(glua, r, "__srcds_mflash") + 1;
    set_nw(glua, r, "__srcds_mflash", NetValue::Int(n & 3));
}

/// The HL2 weapons built on the machine-gun base (shots counted while held).
pub fn is_machine_gun(class: &str) -> bool {
    matches!(class, "weapon_pistol" | "weapon_smg1" | "weapon_ar2")
}

/// Reserve ammo of the weapon's primary type held by its owner.
fn reserve_ammo(glua: &GLua, w: EntRef) -> i32 {
    reserve(glua, w, false)
}

/// Reserve ammo of the weapon's primary or secondary type held by its owner.
fn reserve(glua: &GLua, w: EntRef, secondary: bool) -> i32 {
    with(&glua.lua, w, |e| (e.weapon_owner, if secondary { e.ammo2.clone() } else { e.ammo1.clone() }))
        .ok()
        .and_then(|(o, a)| entity::with_player(&glua.lua, o, |p, _| p.ammo.get(&a).copied().unwrap_or(0)).ok())
        .unwrap_or(0)
}

/// Takes one round of the weapon's secondary ammo from its owner.
fn take_secondary(glua: &GLua, w: EntRef) {
    let Ok((o, a)) = with(&glua.lua, w, |e| (e.weapon_owner, e.ammo2.clone())) else { return };
    let _ = entity::with_player(&glua.lua, o, |p, _| {
        if let Some(n) = p.ammo.get_mut(&a) {
            *n = (*n - 1).max(0);
        }
    });
}

impl WeaponSystem {
    /// Runs one command for `ply`'s active weapon; returns players killed by it.
    pub fn run_command(&mut self, glua: &GLua, models: &mut WeaponModels, ply: EntRef, cmd: &WeaponCmd) -> Vec<EntRef> {
        let Ok((w, vm, alive)) = entity::with_player(&glua.lua, ply, |p, _| (p.active_weapon, p.view_model, p.alive)) else {
            return Vec::new();
        };
        let class = with(&glua.lua, w, |e| e.class.clone()).unwrap_or_default();
        // the gravity gun lets go when put away or when its owner dies
        if self.cannon_holds.contains_key(&ply) && (!alive || class != "weapon_physcannon") {
            self.cannon_detach(glua, ply, w);
        }
        if w.is_null() || !alive {
            return Vec::new();
        }
        let Ok((scripted, class)) = with(&glua.lua, w, |e| (e.scripted, e.class.clone())) else {
            return Vec::new();
        };
        let path = view_model_path(glua, models, w);
        let saved = set_lua_time(glua, cmd.time);
        let mut killed = Vec::new();
        if scripted {
            swep_command(glua, models, w, vm, &path, cmd);
        } else {
            self.engine_command(glua, models, ply, w, vm, &path, &class, cmd, &mut killed);
        }
        if let Some(old_t) = saved {
            set_lua_time(glua, old_t);
        }
        killed
    }

    /// An HL2 (engine) weapon: the base weapon's per-command order with each weapon's
    /// own parts. [факт] order and numbers read from the reference server.dll (base
    /// `ItemPostFrame`, the machine-gun `PrimaryAttack`, the pistol's overrides); this
    /// is an own implementation of that behaviour.
    #[allow(clippy::too_many_arguments)]
    fn engine_command(
        &mut self,
        glua: &GLua,
        models: &mut WeaponModels,
        ply: EntRef,
        w: EntRef,
        vm: EntRef,
        path: &str,
        class: &str,
        cmd: &WeaponCmd,
        killed: &mut Vec<EntRef>,
    ) {
        let t = cmd.time;
        let (b, old) = (cmd.buttons, cmd.old_buttons);
        let pressed = |x: i32| b & x != 0 && old & x == 0;
        let released = |x: i32| b & x == 0 && old & x != 0;
        let pistol = class == "weapon_pistol";
        if class == "weapon_physcannon" {
            self.physcannon_command(glua, models, ply, w, vm, path, cmd);
            return;
        }
        if class == "weapon_shotgun" {
            self.shotgun_command(glua, models, ply, w, vm, path, cmd, killed);
            return;
        }
        if class == "weapon_crowbar" || class == "weapon_stunstick" {
            // melee: swing while held, else idle (no reload, no alt-fire)
            if b & IN_ATTACK != 0 && t >= nw_time(glua, w, "__srcds_npf") {
                self.primary_attack(glua, models, ply, w, vm, path, class, cmd, killed);
            } else if b & IN_ATTACK2 == 0 && t > nw_time(glua, w, "__srcds_idle") {
                send_anim(glua, models, w, vm, path, "ACT_VM_IDLE", t);
            }
            return;
        }
        if class == "weapon_crossbow" {
            // alt-fire toggles the scope; after a shot the bolt is reloaded as soon as
            // the idle time passes
            if pressed(IN_ATTACK2) {
                let z = nw_int(glua, w, "__srcds_xb_zoom") == 0;
                set_nw(glua, w, "__srcds_xb_zoom", NetValue::Int(z as i64));
            }
            if nw_int(glua, w, "__srcds_xb_reload") != 0 && t > nw_time(glua, w, "__srcds_idle") && self.reload(glua, models, w, vm, path, class, t) {
                set_nw(glua, w, "__srcds_xb_reload", NetValue::Int(0));
            }
        }
        if class == "weapon_physgun" {
            // [факт] reference `CWeaponPhysGun::ItemPostFrame` never idles (the client
            // predicted one animation fewer every idle cycle while it did)
            return;
        }
        // the pistol's accuracy recovers while the trigger is up (its pre-frame)
        if pistol && b & IN_ATTACK == 0 && t > nw_time(glua, w, "__srcds_soonest") {
            let p = (nw_time(glua, w, "__srcds_penalty") - FRAME_TIME).clamp(0.0, 1.5);
            set_nw(glua, w, "__srcds_penalty", NetValue::Float(p));
        }
        // machine guns count consecutive shots only while held
        if is_machine_gun(class) && b & IN_ATTACK == 0 {
            set_nw(glua, w, "__srcds_shots", NetValue::Int(0));
        }
        // the AR2's charged alt-fire goes off half a second after the press
        if class == "weapon_ar2" && has_nw(glua, w, "__srcds_ar2_delayed") && nw_time(glua, w, "__srcds_ar2_delayed") < t {
            let _ = with(&glua.lua, w, |e| e.nw.remove("__srcds_ar2_delayed"));
            let dur = send_anim(glua, models, w, vm, path, "ACT_VM_SECONDARYATTACK", t);
            set_nw(glua, w, "__srcds_nsf", NetValue::Float(t + dur as f64));
            take_secondary(glua, w);
            // [не реализовано] the energy ball itself
            set_nw(glua, w, "__srcds_npf", NetValue::Float(t + 1.0));
            set_nw(glua, w, "__srcds_nsf", NetValue::Float(t + 1.0));
        }
        let uses_clip = with(&glua.lua, w, |e| e.max_clip1 > 0).unwrap_or(false);
        // a reload that has run its course fills the clip
        if uses_clip && has_nw(glua, w, "__srcds_in_reload") && nw_time(glua, w, "__srcds_npf") <= t {
            entity::finish_reload(&glua.lua, w);
        }
        let clip = || with(&glua.lua, w, |e| e.clip1).unwrap_or(0);
        // alt-fire: with secondary ammo (or none needed) the weapon's own, else the
        // empty click every half second
        if b & IN_ATTACK2 != 0 && t >= nw_time(glua, w, "__srcds_nsf") {
            let needs = with(&glua.lua, w, |e| !e.ammo2.is_empty()).unwrap_or(false);
            if !needs || reserve(glua, w, true) > 0 {
                self.secondary_attack(glua, models, ply, w, vm, path, class, cmd);
            } else if t > nw_time(glua, w, "__srcds_empty_sound") {
                set_nw(glua, w, "__srcds_empty_sound", NetValue::Float(t + 0.5));
                set_nw(glua, w, "__srcds_nsf", NetValue::Float(t + 0.5));
            }
        }
        if b & IN_ATTACK != 0 && nw_time(glua, w, "__srcds_npf") <= t {
            if uses_clip && clip() <= 0 {
                self.fire_on_empty(glua, models, w, vm, path, t);
            } else {
                // a fresh click (or letting go of alt-fire) fires at once
                if pressed(IN_ATTACK) || released(IN_ATTACK2) {
                    set_nw(glua, w, "__srcds_npf", NetValue::Float(t));
                }
                self.primary_attack(glua, models, ply, w, vm, path, class, cmd, killed);
            }
        }
        if b & IN_RELOAD != 0 && nw_time(glua, w, "__srcds_npf") <= t && uses_clip && !has_nw(glua, w, "__srcds_in_reload") {
            self.reload(glua, models, w, vm, path, class, t);
        }
        if b & (IN_ATTACK | IN_ATTACK2) == 0 && !(uses_clip && b & IN_RELOAD != 0) {
            // nothing pressed: an empty clip reloads by itself, else the weapon idles
            set_nw(glua, w, "__srcds_fire_on_empty", NetValue::Int(0));
            if !self.reload_if_empty(glua, models, w, vm, path, class, t) && !has_nw(glua, w, "__srcds_in_reload") && t > nw_time(glua, w, "__srcds_idle") {
                let has_ammo = !uses_clip || clip() > 0 || reserve_ammo(glua, w) > 0;
                let act = if pistol && !has_ammo { "ACT_VM_IDLE_EMPTY" } else { "ACT_VM_IDLE" };
                send_anim(glua, models, w, vm, path, act, t);
            }
        }
        // the pistol's own frame: alt-fire holds it back, dry fire on an empty clip,
        // and refire as fast as the player clicks
        if pistol && !has_nw(glua, w, "__srcds_in_reload") {
            if b & IN_ATTACK2 != 0 {
                for k in ["__srcds_lastattack", "__srcds_soonest", "__srcds_npf"] {
                    set_nw(glua, w, k, NetValue::Float(t + 0.1f32 as f64));
                }
            }
            if b & IN_ATTACK != 0 {
                if t > nw_time(glua, w, "__srcds_npf") && clip() <= 0 {
                    let dur = send_anim(glua, models, w, vm, path, "ACT_VM_DRYFIRE", t);
                    set_nw(glua, w, "__srcds_soonest", NetValue::Float(t + 0.2f32 as f64));
                    set_nw(glua, w, "__srcds_npf", NetValue::Float(t + dur as f64));
                }
            } else if t > nw_time(glua, w, "__srcds_soonest") && (t - 0.1f32 as f64) < nw_time(glua, w, "__srcds_npf") {
                set_nw(glua, w, "__srcds_npf", NetValue::Float(t - 0.1f32 as f64));
            }
        }
    }

    /// The trigger on an empty clip: the first time only marks it (the empty click
    /// sound), held on it reloads when it can.
    fn fire_on_empty(&mut self, glua: &GLua, models: &mut WeaponModels, w: EntRef, vm: EntRef, path: &str, t: f64) {
        if nw_int(glua, w, "__srcds_fire_on_empty") != 0 {
            let class = with(&glua.lua, w, |e| e.class.clone()).unwrap_or_default();
            self.reload_if_empty(glua, models, w, vm, path, &class, t);
        } else {
            set_nw(glua, w, "__srcds_fire_on_empty", NetValue::Int(1));
        }
    }

    /// Reloads an empty clip when the owner has reserve ammo and both attacks are
    /// ready; returns whether it did.
    #[allow(clippy::too_many_arguments)]
    fn reload_if_empty(&mut self, glua: &GLua, models: &mut WeaponModels, w: EntRef, vm: EntRef, path: &str, class: &str, t: f64) -> bool {
        let (uses_clip, clip) = with(&glua.lua, w, |e| (e.max_clip1 > 0, e.clip1)).unwrap_or((false, 0));
        if !uses_clip || clip != 0 || reserve_ammo(glua, w) <= 0 || has_nw(glua, w, "__srcds_in_reload") {
            return false;
        }
        if nw_time(glua, w, "__srcds_npf") >= t || nw_time(glua, w, "__srcds_nsf") >= t {
            return false;
        }
        self.reload(glua, models, w, vm, path, class, t)
    }

    /// The weapon's reload (the default clip reload; the pistol also steadies).
    #[allow(clippy::too_many_arguments)]
    fn reload(&mut self, glua: &GLua, models: &mut WeaponModels, w: EntRef, vm: EntRef, path: &str, class: &str, t: f64) -> bool {
        if class == "weapon_ar2" && has_nw(glua, w, "__srcds_ar2_delayed") {
            return false;
        }
        let nsf = nw_time(glua, w, "__srcds_nsf");
        let started = entity::default_reload(&glua.lua, w, "ACT_VM_RELOAD");
        if started && class == "weapon_pistol" {
            set_nw(glua, w, "__srcds_penalty", NetValue::Float(0.0));
        }
        resolve_lua_anims(glua, models, w, vm, path, t);
        // the SMG1's grenade stays ready through a reload
        if started && class == "weapon_smg1" {
            set_nw(glua, w, "__srcds_nsf", NetValue::Float(nsf));
        }
        started
    }

    /// The weapon's alt-fire (secondary ammo checked by the caller).
    #[allow(clippy::too_many_arguments)]
    fn secondary_attack(&mut self, glua: &GLua, models: &mut WeaponModels, ply: EntRef, w: EntRef, vm: EntRef, path: &str, class: &str, cmd: &WeaponCmd) {
        let t = cmd.time;
        match class {
            "weapon_smg1" => {
                // a grenade: cancels a reload
                let _ = with(&glua.lua, w, |e| e.nw.remove("__srcds_in_reload"));
                self.launch(glua, ply, cmd, ProjectileKind::Grenade);
                send_anim(glua, models, w, vm, path, "ACT_VM_SECONDARYATTACK", t);
                take_secondary(glua, w);
                set_nw(glua, w, "__srcds_npf", NetValue::Float(t + 0.5));
                set_nw(glua, w, "__srcds_nsf", NetValue::Float(t + 1.0));
            }
            "weapon_ar2" => {
                if !has_nw(glua, w, "__srcds_ar2_delayed") {
                    // (kept as a float, as the reference does)
                    set_nw(glua, w, "__srcds_ar2_delayed", NetValue::Float((t + 0.5) as f32 as f64));
                    set_nw(glua, w, "__srcds_nsf", NetValue::Float(t + 0.5));
                    set_nw(glua, w, "__srcds_npf", NetValue::Float(t + 0.5));
                    send_anim(glua, models, w, vm, path, "ACT_VM_FIDGET", t);
                }
            }
            _ => {}
        }
    }

    /// The weapon's primary attack at the command's time.
    #[allow(clippy::too_many_arguments)]
    fn primary_attack(
        &mut self,
        glua: &GLua,
        models: &mut WeaponModels,
        ply: EntRef,
        w: EntRef,
        vm: EntRef,
        path: &str,
        class: &str,
        cmd: &WeaponCmd,
        killed: &mut Vec<EntRef>,
    ) {
        let t = cmd.time;
        let pistol = class == "weapon_pistol";
        if pistol {
            // shots within half a second of each other count up the recoil
            let last = nw_time(glua, w, "__srcds_lastattack");
            let n = if t - last <= 0.5 { nw_int(glua, w, "__srcds_numshots") + 1 } else { 0 };
            set_nw(glua, w, "__srcds_numshots", NetValue::Int(n));
            set_nw(glua, w, "__srcds_lastattack", NetValue::Float(t));
            set_nw(glua, w, "__srcds_soonest", NetValue::Float(t + 0.1f32 as f64));
        }
        if class == "weapon_crossbow" {
            // the bolt flies as its own entity
            let _ = with(&glua.lua, w, |e| e.clip1 = (e.clip1 - 1).max(0));
            self.launch(glua, ply, cmd, ProjectileKind::Bolt);
            send_anim(glua, models, w, vm, path, "ACT_VM_PRIMARYATTACK", t);
            set_nw(glua, w, "__srcds_nsf", NetValue::Float(t + 0.75));
            set_nw(glua, w, "__srcds_npf", NetValue::Float(t + 0.75));
            set_nw(glua, w, "__srcds_xb_reload", NetValue::Int(1));
            // [факт] reference: the idle time comes from "sequence 181" (the activity
            // number used as a sequence index); [гипотеза] an out-of-range sequence
            // lasts 0.1 s, so the reload follows at once
            set_nw(glua, w, "__srcds_idle", NetValue::Float(t + 0.1f32 as f64));
            return;
        }
        if class == "weapon_crowbar" || class == "weapon_stunstick" {
            // [факт] reference swing: next swing after the fire rate, alt-fire after
            // the swing animation
            if let Some(rate) = self.engine_fire(glua, models, ply, w, vm, path, class, cmd, killed) {
                set_nw(glua, w, "__srcds_npf", NetValue::Float(t + rate));
                let idle = nw_time(glua, w, "__srcds_idle");
                set_nw(glua, w, "__srcds_nsf", NetValue::Float(idle));
            }
            return;
        }
        if class == "weapon_357" {
            muzzle_flash(glua, w);
            muzzle_flash(glua, vm);
            send_anim(glua, models, w, vm, path, "ACT_VM_PRIMARYATTACK", t);
            set_nw(glua, w, "__srcds_npf", NetValue::Float(t + 0.75));
            set_nw(glua, w, "__srcds_nsf", NetValue::Float(t + 0.75));
            let _ = with(&glua.lua, w, |e| e.clip1 = (e.clip1 - 1).max(0));
            crate::weapons::fire_bullets(glua, ply, cmd, 1, 40.0, 56756.0, 0.0, killed);
            return;
        }
        if !is_machine_gun(class) {
            // other HL2 weapons: the approximate generic shot
            if let Some(rate) = self.engine_fire(glua, models, ply, w, vm, path, class, cmd, killed) {
                set_nw(glua, w, "__srcds_npf", NetValue::Float(t + rate));
            }
            return;
        }
        // machine-gun base: as many shots as the refire time allows since the last
        // one, paid from the clip
        let has_ammo = with(&glua.lua, w, |e| e.max_clip1 <= 0 || e.clip1 > 0).unwrap_or(false) || reserve_ammo(glua, w) > 0;
        if !has_ammo {
            return;
        }
        set_nw(glua, w, "__srcds_shots", NetValue::Int(nw_int(glua, w, "__srcds_shots") + 1));
        muzzle_flash(glua, w);
        muzzle_flash(glua, vm);
        // (refire s, damage, spread)
        let (rate, damage, spread) = match class {
            "weapon_pistol" => (0.5, 8.0, 0.01),
            "weapon_smg1" => (0.075f32 as f64, 5.0, 0.04),
            _ => (0.1f32 as f64, 11.0, 0.02),
        };
        let mut npf = nw_time(glua, w, "__srcds_npf");
        let mut shots = 0;
        while npf <= t {
            npf += rate;
            shots += 1;
        }
        set_nw(glua, w, "__srcds_npf", NetValue::Float(npf));
        let _ = with(&glua.lua, w, |e| {
            if e.max_clip1 > 0 {
                e.clip1 -= shots.min(e.clip1.max(0));
            }
        });
        crate::weapons::fire_bullets(glua, ply, cmd, shots, damage, 56756.0, spread, killed);
        let act = if pistol {
            match nw_int(glua, w, "__srcds_numshots") {
                n if n < 1 => "ACT_VM_PRIMARYATTACK",
                1 => "ACT_VM_RECOIL1",
                2 => "ACT_VM_RECOIL2",
                _ => "ACT_VM_RECOIL3",
            }
        } else {
            // [факт] reference: the SMG1 and AR2 pick recoil animations by the shots
            // fired in the burst
            let n = nw_int(glua, w, "__srcds_shots");
            match (class, n) {
                (_, n) if n < 2 => "ACT_VM_PRIMARYATTACK",
                (_, 2) => "ACT_VM_RECOIL1",
                ("weapon_smg1", _) => "ACT_VM_RECOIL3",
                (_, 3) => "ACT_VM_RECOIL2",
                _ => "ACT_VM_RECOIL3",
            }
        };
        send_anim(glua, models, w, vm, path, act, t);
        if pistol {
            set_nw(glua, w, "__srcds_penalty", NetValue::Float(nw_time(glua, w, "__srcds_penalty") + 0.2));
        }
    }
}

/// A Lua weapon: the engine's base order around the SWEP's Lua callbacks.
fn swep_command(glua: &GLua, models: &mut WeaponModels, w: EntRef, vm: EntRef, path: &str, cmd: &WeaponCmd) {
    let t = cmd.time;
    let (b, old) = (cmd.buttons, cmd.old_buttons);
    let pressed = |x: i32| b & x != 0 && old & x == 0;
    if has_nw(glua, w, "__srcds_in_reload") && nw_time(glua, w, "__srcds_npf") <= t {
        entity::finish_reload(&glua.lua, w);
    }
    if b & IN_ATTACK2 != 0 && nw_time(glua, w, "__srcds_nsf") <= t {
        if swep_automatic(glua, w, "Secondary") || pressed(IN_ATTACK2) {
            swep_call(glua, w, "SecondaryAttack");
        }
    } else if b & IN_ATTACK != 0 && nw_time(glua, w, "__srcds_npf") <= t && !has_nw(glua, w, "__srcds_in_reload") && (swep_automatic(glua, w, "Primary") || pressed(IN_ATTACK)) {
        swep_call(glua, w, "PrimaryAttack");
    }
    if pressed(IN_RELOAD) && !has_nw(glua, w, "__srcds_in_reload") {
        swep_call(glua, w, "Reload");
    }
    resolve_lua_anims(glua, models, w, vm, path, t);
    if b & (IN_ATTACK | IN_ATTACK2 | IN_RELOAD) == 0 && !has_nw(glua, w, "__srcds_in_reload") && t > nw_time(glua, w, "__srcds_idle") {
        send_anim(glua, models, w, vm, path, "ACT_VM_IDLE", t);
    }
}

/// `physcannon_tracelength` default ([факт] reference server.dll, replicated).
pub const PHYSCANNON_RANGE: f32 = 250.0;
/// `physcannon_maxforce` default ([факт] reference).
pub const PHYSCANNON_MAX_FORCE: f32 = 1500.0;
/// `physcannon_maxmass` default ([факт] reference).
pub const PHYSCANNON_MAX_MASS: f32 = 250.0;
/// `physcannon_pullforce` default ([факт] reference).
pub const PHYSCANNON_PULL_FORCE: f32 = 4000.0;

/// What a trace from the eye along the view hit: fraction and the entity (not the
/// world).
fn view_trace(glua: &GLua, ply: EntRef, cmd: &WeaponCmd, range: f32, hull: f32) -> (f32, Option<EntRef>) {
    let f = srcds_lua::vecang::angle_vectors(cmd.angles).0;
    let eye = cmd.eye;
    let end = [eye[0] + f[0] * range, eye[1] + f[1] * range, eye[2] + f[2] * range];
    let run = || -> srcds_lua::mlua::Result<(f32, Option<EntRef>)> {
        let util: Table = glua.lua.globals().get("util")?;
        let tr_fn: srcds_lua::mlua::Function = util.get(if hull > 0.0 { "TraceHull" } else { "TraceLine" })?;
        let t = glua.lua.create_table()?;
        t.set("start", srcds_lua::vecang::Vector(eye))?;
        t.set("endpos", srcds_lua::vecang::Vector(end))?;
        t.set("filter", crate::weapons::ud(glua, ply)?)?;
        if hull > 0.0 {
            t.set("mins", srcds_lua::vecang::Vector([-hull; 3]))?;
            t.set("maxs", srcds_lua::vecang::Vector([hull; 3]))?;
        }
        let tr: Table = tr_fn.call(t)?;
        let frac: f32 = tr.get("Fraction").unwrap_or(1.0);
        let ent = match tr.get::<Value>("Entity")? {
            Value::UserData(u) => u.borrow::<entity::EntUd>().map(|e| e.0).unwrap_or(EntRef::NULL),
            _ => EntRef::NULL,
        };
        Ok((frac, (!ent.is_null() && ent.id != 0).then_some(ent)))
    };
    run().unwrap_or((1.0, None))
}

/// A movable physics object's mass (what the gravity gun can act on).
fn cannon_object(glua: &GLua, ent: EntRef) -> Option<f32> {
    with(&glua.lua, ent, |e| e.phys.as_ref().filter(|p| p.motion && !p.held).map(|p| p.mass)).ok().flatten()
}

impl WeaponSystem {
    /// The gravity gun's frame. [факт] reference `CWeaponPhysCannon` (own
    /// implementation): the claws open toward a pickup-able object in range and close
    /// half a second after losing it; alt-fire pulls (effect state 3); with alt-fire
    /// up the effect is "ready" (2); primary fire punts what is in front (effect 4,
    /// secondary-attack animation) or dry-fires; otherwise it idles. Alt-fire picks
    /// up an object within reach (held in front, effect 3, dropped by alt-fire again,
    /// thrown by primary fire) and pulls farther ones.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn physcannon_command(&mut self, glua: &GLua, models: &mut WeaponModels, ply: EntRef, w: EntRef, vm: EntRef, path: &str, cmd: &WeaponCmd) {
        let t = cmd.time;
        let b = cmd.buttons;
        let pressed2 = b & IN_ATTACK2 != 0 && cmd.old_buttons & IN_ATTACK2 == 0;
        let int_or = |k: &str, d: i64| if has_nw(glua, w, k) { nw_int(glua, w, k) } else { d };
        let ply_ud = crate::weapons::ud(glua, ply).ok();
        let fwd = srcds_lua::vecang::angle_vectors(cmd.angles).0;
        // the held object follows the view (its pre-frame); lost → dropped
        if let Some(h) = self.cannon_holds.get(&ply).copied() {
            if !self.cannon_carry(glua, cmd, h) {
                self.cannon_detach(glua, ply, w);
            }
        }
        let holding = self.cannon_holds.contains_key(&ply);
        // claws: toward a target (not while holding)
        if !holding {
            let (frac, ent) = view_trace(glua, ply, cmd, PHYSCANNON_RANGE, 0.0);
            let target = ent.filter(|e| frac < 1.0 && self.cannon_can_pick(glua, ply_ud.clone(), *e));
            let mut change = int_or("__srcds_pc_change", -1);
            if target.is_some() {
                change = -1;
                self.cannon_open(glua, models, w, vm, path, t, true);
            } else if t > nw_time(glua, w, "__srcds_pc_debounce") && change == -1 {
                change = 1;
                set_nw(glua, w, "__srcds_pc_debounce", NetValue::Float(t + 0.5));
            }
            if t > nw_time(glua, w, "__srcds_pc_debounce") && change != -1 {
                self.cannon_open(glua, models, w, vm, path, t, change == 0);
                change = -1;
            }
            set_nw(glua, w, "__srcds_pc_change", NetValue::Int(change));
        }
        // alt-fire (debounced after a pickup): drop, pick up or pull; up, "ready"
        let debounce2 = nw_int(glua, w, "__srcds_pc_a2debounce") as i32;
        if b & !debounce2 & IN_ATTACK2 != 0 {
            if t >= nw_time(glua, w, "__srcds_nsf") {
                if holding {
                    if pressed2 {
                        set_nw(glua, w, "__srcds_npf", NetValue::Float(t + 0.5));
                        set_nw(glua, w, "__srcds_nsf", NetValue::Float(t + 0.5));
                        self.cannon_detach(glua, ply, w);
                        set_nw(glua, w, "__srcds_pc_effect", NetValue::Int(2));
                        send_anim(glua, models, w, vm, path, "ACT_VM_PRIMARYATTACK", t);
                    }
                } else {
                    match self.cannon_find(glua, ply, w, cmd) {
                        // picked up
                        Some(true) => {
                            send_anim(glua, models, w, vm, path, "ACT_VM_PRIMARYATTACK", t);
                            set_nw(glua, w, "__srcds_nsf", NetValue::Float(t + 0.5));
                            set_nw(glua, w, "__srcds_pc_a2debounce", NetValue::Int((debounce2 | b) as i64));
                            self.cannon_open(glua, models, w, vm, path, t, true);
                        }
                        // nothing (or pulling from afar)
                        _ => {
                            set_nw(glua, w, "__srcds_nsf", NetValue::Float(t + 0.1f32 as f64));
                            self.cannon_open(glua, models, w, vm, path, t, false);
                        }
                    }
                    set_nw(glua, w, "__srcds_pc_effect", NetValue::Int(3));
                }
            }
        } else if !holding {
            set_nw(glua, w, "__srcds_pc_effect", NetValue::Int(2));
        }
        if b & IN_ATTACK2 == 0 {
            set_nw(glua, w, "__srcds_pc_a2debounce", NetValue::Int(0));
        }
        let holding = self.cannon_holds.contains_key(&ply);
        if b & IN_ATTACK != 0 {
            if t >= nw_time(glua, w, "__srcds_npf") {
                set_nw(glua, w, "__srcds_npf", NetValue::Float(t + 0.5));
                // what to throw: the held object, else what is in front (a punt)
                let punt = if holding {
                    self.cannon_holds.get(&ply).map(|h| h.ent)
                } else {
                    let (frac, ent) = view_trace(glua, ply, cmd, PHYSCANNON_RANGE, 8.0);
                    ent.filter(|e| frac < 1.0 && cannon_object(glua, *e).is_some())
                };
                let punt = punt.filter(|e| !crate::weapons::hook_is_false(glua, "GravGunPunt", (ply_ud.clone(), crate::weapons::ud(glua, *e).ok())));
                match punt {
                    Some(e) => {
                        if holding {
                            self.cannon_detach(glua, ply, w);
                            set_nw(glua, w, "__srcds_nsf", NetValue::Float(t + 0.5));
                        }
                        // [гипотеза] throw strength: the max force as a speed change,
                        // scaled down for objects heavier than the pickup limit
                        let mass = with(&glua.lua, e, |d| d.phys.as_ref().map_or(1.0, |p| p.mass)).unwrap_or(1.0).max(1.0);
                        let k = PHYSCANNON_MAX_FORCE * (PHYSCANNON_MAX_MASS / mass).min(1.0);
                        let _ = with(&glua.lua, e, |d| {
                            for j in 0..3 {
                                d.vel[j] += fwd[j] * k;
                            }
                            if let Some(p) = &mut d.phys {
                                p.asleep = false;
                            }
                        });
                        set_nw(glua, w, "__srcds_pc_effect", NetValue::Int(4));
                        send_anim(glua, models, w, vm, path, "ACT_VM_SECONDARYATTACK", t);
                    }
                    None => {
                        send_anim(glua, models, w, vm, path, "ACT_VM_PRIMARYATTACK", t);
                    }
                }
            }
        } else if t > nw_time(glua, w, "__srcds_idle") {
            // [факт] reference: the "reload" animation is the holding idle
            send_anim(glua, models, w, vm, path, if holding { "ACT_VM_RELOAD" } else { "ACT_VM_IDLE" }, t);
        }
    }

    /// Whether the gravity gun may pick `e` up: a movable physics object within the
    /// mass limit that the gamemode allows.
    fn cannon_can_pick(&self, glua: &GLua, ply_ud: Option<Value>, e: EntRef) -> bool {
        cannon_object(glua, e).is_some_and(|m| m <= PHYSCANNON_MAX_MASS)
            && !crate::weapons::hook_is_false(glua, "GravGunPickupAllowed", (ply_ud, crate::weapons::ud(glua, e).ok()))
    }

    /// Alt-fire's search ([факт] reference `FindObject`): along the view up to four
    /// times the reach; a pickup-able object within reach is picked up (`Some(true)`),
    /// one farther away is pulled toward the player (`Some(false)`).
    fn cannon_find(&mut self, glua: &GLua, ply: EntRef, w: EntRef, cmd: &WeaponCmd) -> Option<bool> {
        let far = PHYSCANNON_RANGE * 4.0;
        let (frac, ent) = view_trace(glua, ply, cmd, far, 0.0);
        let e = ent.filter(|_| frac < 1.0)?;
        let ply_ud = crate::weapons::ud(glua, ply).ok();
        if !self.cannon_can_pick(glua, ply_ud.clone(), e) {
            return None;
        }
        if frac <= 0.25 {
            // attach: held in front of the eye at its current distance from the eye
            let (pos, mins, maxs) = with(&glua.lua, e, |d| (d.pos, d.mins, d.maxs)).ok()?;
            let center = [pos[0] + (mins[0] + maxs[0]) * 0.5, pos[1] + (mins[1] + maxs[1]) * 0.5, pos[2] + (mins[2] + maxs[2]) * 0.5];
            let size = [maxs[0] - mins[0], maxs[1] - mins[1], maxs[2] - mins[2]];
            let radius = 0.5 * (size[0] * size[0] + size[1] * size[1] + size[2] * size[2]).sqrt();
            // [гипотеза] hold distance: the object's radius plus 24 units
            let dist = radius + 24.0;
            let off = [center[0] - pos[0], center[1] - pos[1], center[2] - pos[2]];
            self.cannon_holds.insert(ply, crate::weapons::CannonHold { ent: e, dist, center_offset: off });
            let _ = with(&glua.lua, e, |d| {
                if let Some(p) = &mut d.phys {
                    p.held = true;
                    p.asleep = false;
                }
            });
            set_nw(glua, w, "__srcds_pc_held", NetValue::Int(1));
            glua.hook_call("GravGunOnPickedUp", (ply_ud, crate::weapons::ud(glua, e).ok()));
            return Some(true);
        }
        // pull: [факт] `physcannon_pullforce` 4000, light objects (< 50) pulled less;
        // [гипотеза] as an impulse (speed change = force / mass)
        let (pos, mass) = with(&glua.lua, e, |d| (d.pos, d.phys.as_ref().map_or(1.0, |p| p.mass))).ok()?;
        let d = [cmd.eye[0] - pos[0], cmd.eye[1] - pos[1], cmd.eye[2] - pos[2]];
        let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt().max(1e-3);
        let mut k = PHYSCANNON_PULL_FORCE;
        if mass < 50.0 {
            k *= (mass + 0.5) * 0.02;
        }
        let k = k / mass.max(1.0);
        let _ = with(&glua.lua, e, |dd| {
            for j in 0..3 {
                dd.vel[j] += d[j] / l * k;
            }
            if let Some(p) = &mut dd.phys {
                p.asleep = false;
            }
        });
        Some(false)
    }

    /// Moves a held object to the hold point in front of the eye; false when it is
    /// gone or stuck too far away.
    fn cannon_carry(&mut self, glua: &GLua, cmd: &WeaponCmd, h: crate::weapons::CannonHold) -> bool {
        if !entity::valid(&glua.lua, h.ent) {
            return false;
        }
        let f = srcds_lua::vecang::angle_vectors(cmd.angles).0;
        let target = [cmd.eye[0] + f[0] * h.dist, cmd.eye[1] + f[1] * h.dist, cmd.eye[2] + f[2] * h.dist];
        let dt = FRAME_TIME as f32;
        with(&glua.lua, h.ent, |e| {
            let desired = [target[0] - h.center_offset[0], target[1] - h.center_offset[1], target[2] - h.center_offset[2]];
            let gap = [desired[0] - e.pos[0], desired[1] - e.pos[1], desired[2] - e.pos[2]];
            // [гипотеза] dropped when held more than the reach away from its hold point
            if (gap[0] * gap[0] + gap[1] * gap[1] + gap[2] * gap[2]).sqrt() > PHYSCANNON_RANGE {
                return false;
            }
            for k in 0..3 {
                e.vel[k] = (gap[k] / dt).clamp(-3000.0, 3000.0);
            }
            e.pos = desired;
            if let Some(p) = &mut e.phys {
                p.asleep = false;
                p.held = true;
                p.ang_vel = [0.0; 3];
            }
            true
        })
        .unwrap_or(false)
    }

    /// Lets go of the held object.
    pub(crate) fn cannon_detach(&mut self, glua: &GLua, ply: EntRef, w: EntRef) {
        let Some(h) = self.cannon_holds.remove(&ply) else { return };
        let _ = with(&glua.lua, h.ent, |d| {
            if let Some(p) = &mut d.phys {
                p.held = false;
                p.asleep = false;
            }
        });
        let _ = with(&glua.lua, w, |e| e.nw.remove("__srcds_pc_held"));
        if entity::valid(&glua.lua, h.ent) {
            glua.hook_call("GravGunOnDropped", (crate::weapons::ud(glua, ply).ok(), crate::weapons::ud(glua, h.ent).ok()));
        }
    }

    /// Opens (`open`) or closes the gravity gun's claws when they are the other way.
    #[allow(clippy::too_many_arguments)]
    fn cannon_open(&mut self, glua: &GLua, models: &mut WeaponModels, w: EntRef, vm: EntRef, path: &str, t: f64, open: bool) {
        if (nw_int(glua, w, "__srcds_pc_open") != 0) == open {
            return;
        }
        send_anim(glua, models, w, vm, path, "ACT_VM_IDLE", t);
        set_nw(glua, w, "__srcds_pc_open", NetValue::Int(open as i64));
        set_nw(glua, w, "__srcds_pc_effect", NetValue::Int(if open { 2 } else { 1 }));
    }
}

fn flag(glua: &GLua, w: EntRef, k: &str) -> bool {
    nw_int(glua, w, k) != 0
}

fn set_flag(glua: &GLua, w: EntRef, k: &str, on: bool) {
    set_nw(glua, w, k, NetValue::Int(on as i64));
}

/// Moves one round from the owner's reserve into the clip.
fn fill_one(glua: &GLua, w: EntRef) {
    let Ok((o, a, clip, max)) = with(&glua.lua, w, |e| (e.weapon_owner, e.ammo1.clone(), e.clip1, e.max_clip1)) else { return };
    if clip >= max {
        return;
    }
    let took = entity::with_player(&glua.lua, o, |p, _| match p.ammo.get_mut(&a) {
        Some(n) if *n > 0 => {
            *n -= 1;
            true
        }
        _ => false,
    })
    .unwrap_or(false);
    if took {
        let _ = with(&glua.lua, w, |e| e.clip1 += 1);
    }
}

impl WeaponSystem {
    /// The shotgun's frame. [факт] reference `CWeaponShotgun` (own implementation):
    /// every shot needs a pump; reloading goes shell by shell (start, one shell per
    /// reload animation, finish) and a trigger pull during it fires as soon as the
    /// current shell is in; alt-fire fires both barrels (two shells).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn shotgun_command(&mut self, glua: &GLua, models: &mut WeaponModels, ply: EntRef, w: EntRef, vm: EntRef, path: &str, cmd: &WeaponCmd, killed: &mut Vec<EntRef>) {
        let t = cmd.time;
        let b = cmd.buttons;
        let pressed_attack = b & IN_ATTACK != 0 && cmd.old_buttons & IN_ATTACK == 0;
        let clip = || with(&glua.lua, w, |e| e.clip1).unwrap_or(0);
        let max_clip = with(&glua.lua, w, |e| e.max_clip1).unwrap_or(6);
        let npf = || nw_time(glua, w, "__srcds_npf");
        let in_reload = || has_nw(glua, w, "__srcds_in_reload");
        if flag(glua, w, "__srcds_sg_pump") && b & IN_RELOAD != 0 {
            set_flag(glua, w, "__srcds_sg_reload", true);
        }
        if in_reload() {
            if b & IN_ATTACK != 0 && clip() >= 1 && !flag(glua, w, "__srcds_sg_pump") {
                let _ = with(&glua.lua, w, |e| e.nw.remove("__srcds_in_reload"));
                set_flag(glua, w, "__srcds_sg_pump", false);
                set_flag(glua, w, "__srcds_sg_fire1", true);
            } else if b & IN_ATTACK2 != 0 && clip() >= 2 && !flag(glua, w, "__srcds_sg_pump") {
                let _ = with(&glua.lua, w, |e| e.nw.remove("__srcds_in_reload"));
                set_flag(glua, w, "__srcds_sg_pump", false);
                set_flag(glua, w, "__srcds_sg_fire2", true);
            } else if t >= npf() {
                if reserve_ammo(glua, w) <= 0 || clip() >= max_clip {
                    self.shotgun_finish_reload(glua, models, w, vm, path, t);
                } else {
                    self.shotgun_shell(glua, models, w, vm, path, t);
                }
                return;
            }
        }
        if flag(glua, w, "__srcds_sg_pump") && t >= npf() {
            set_flag(glua, w, "__srcds_sg_pump", false);
            if flag(glua, w, "__srcds_sg_reload") {
                set_flag(glua, w, "__srcds_sg_reload", false);
                self.shotgun_start_reload(glua, models, w, vm, path, t);
            }
            let dur = send_anim(glua, models, w, vm, path, "ACT_SHOTGUN_PUMP", t);
            set_nw(glua, w, "__srcds_npf", NetValue::Float(t + dur as f64));
            return;
        }
        let fire2 = flag(glua, w, "__srcds_sg_fire2");
        let fire1 = flag(glua, w, "__srcds_sg_fire1");
        if (fire2 || b & IN_ATTACK2 != 0) && t >= npf() {
            set_flag(glua, w, "__srcds_sg_fire2", false);
            match clip() {
                0 => self.shotgun_empty(glua, models, w, vm, path, t),
                1 => self.shotgun_fire(glua, models, ply, w, vm, path, cmd, false, killed),
                _ => {
                    if pressed_attack {
                        set_nw(glua, w, "__srcds_npf", NetValue::Float(t));
                    }
                    self.shotgun_fire(glua, models, ply, w, vm, path, cmd, true, killed);
                }
            }
        } else if (fire1 || b & IN_ATTACK != 0) && t >= npf() {
            set_flag(glua, w, "__srcds_sg_fire1", false);
            if clip() <= 0 {
                self.shotgun_empty(glua, models, w, vm, path, t);
            } else {
                if pressed_attack {
                    set_nw(glua, w, "__srcds_npf", NetValue::Float(t));
                }
                self.shotgun_fire(glua, models, ply, w, vm, path, cmd, false, killed);
            }
        }
        // reload key, else idle (an empty shotgun with shells reloads by itself)
        if b & IN_RELOAD != 0 && !in_reload() {
            if !flag(glua, w, "__srcds_sg_fire1") && !flag(glua, w, "__srcds_sg_fire2") {
                self.shotgun_start_reload(glua, models, w, vm, path, t);
            }
            return;
        }
        set_nw(glua, w, "__srcds_fire_on_empty", NetValue::Int(0));
        let has_any = clip() > 0 || reserve_ammo(glua, w) > 0;
        let idle = (!has_any && t > npf()) || clip() > 0 || t <= npf() || !self.shotgun_start_reload(glua, models, w, vm, path, t);
        if idle && t > nw_time(glua, w, "__srcds_idle") {
            send_anim(glua, models, w, vm, path, "ACT_VM_IDLE", t);
        }
    }

    /// No shells: reload when there are some in reserve, else the dry click.
    fn shotgun_empty(&mut self, glua: &GLua, models: &mut WeaponModels, w: EntRef, vm: EntRef, path: &str, t: f64) {
        if reserve_ammo(glua, w) != 0 {
            self.shotgun_start_reload(glua, models, w, vm, path, t);
        } else {
            let dur = send_anim(glua, models, w, vm, path, "ACT_VM_DRYFIRE", t);
            set_nw(glua, w, "__srcds_npf", NetValue::Float(t + dur as f64));
        }
    }

    /// One barrel (7 pellets) or both (`double`, 12 pellets, two shells); then a pump.
    #[allow(clippy::too_many_arguments)]
    fn shotgun_fire(&mut self, glua: &GLua, models: &mut WeaponModels, ply: EntRef, w: EntRef, vm: EntRef, path: &str, cmd: &WeaponCmd, double: bool, killed: &mut Vec<EntRef>) {
        let t = cmd.time;
        muzzle_flash(glua, w);
        muzzle_flash(glua, vm);
        let dur = send_anim(glua, models, w, vm, path, if double { "ACT_VM_SECONDARYATTACK" } else { "ACT_VM_PRIMARYATTACK" }, t);
        set_nw(glua, w, "__srcds_npf", NetValue::Float(t + dur as f64));
        let shells = if double { 2 } else { 1 };
        let _ = with(&glua.lua, w, |e| e.clip1 = (e.clip1 - shells).max(0));
        crate::weapons::fire_bullets(glua, ply, cmd, if double { 12 } else { 7 }, 8.0, 56756.0, 0.08, killed);
        set_flag(glua, w, "__srcds_sg_pump", true);
    }

    /// Starts a shell-by-shell reload; returns whether it started.
    fn shotgun_start_reload(&mut self, glua: &GLua, models: &mut WeaponModels, w: EntRef, vm: EntRef, path: &str, t: f64) -> bool {
        let (clip, max) = with(&glua.lua, w, |e| (e.clip1, e.max_clip1)).unwrap_or((0, 0));
        if flag(glua, w, "__srcds_sg_pump") || reserve_ammo(glua, w) <= 0 || clip >= max {
            return false;
        }
        let dur = send_anim(glua, models, w, vm, path, "ACT_SHOTGUN_RELOAD_START", t);
        set_nw(glua, w, "__srcds_npf", NetValue::Float(t + dur as f64));
        set_nw(glua, w, "__srcds_in_reload", NetValue::Int(1));
        true
    }

    /// One shell in.
    fn shotgun_shell(&mut self, glua: &GLua, models: &mut WeaponModels, w: EntRef, vm: EntRef, path: &str, t: f64) {
        fill_one(glua, w);
        let dur = send_anim(glua, models, w, vm, path, "ACT_VM_RELOAD", t);
        set_nw(glua, w, "__srcds_npf", NetValue::Float(t + dur as f64));
    }

    fn shotgun_finish_reload(&mut self, glua: &GLua, models: &mut WeaponModels, w: EntRef, vm: EntRef, path: &str, t: f64) {
        let _ = with(&glua.lua, w, |e| e.nw.remove("__srcds_in_reload"));
        let dur = send_anim(glua, models, w, vm, path, "ACT_SHOTGUN_RELOAD_FINISH", t);
        set_nw(glua, w, "__srcds_npf", NetValue::Float(t + dur as f64));
    }
}

/// A flying projectile of an HL2 weapon (a networked model entity moved by the
/// server each tick).
#[derive(Debug, Clone, Copy)]
pub struct Projectile {
    pub ent: EntRef,
    pub owner: EntRef,
    pub vel: [f32; 3],
    pub kind: ProjectileKind,
    /// When it is removed regardless (s).
    pub expires: f64,
    /// A bolt stuck in the world (no longer moves).
    pub stuck: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectileKind {
    /// Crossbow bolt: fast, nearly straight, damages what it hits.
    Bolt,
    /// SMG1 grenade: ballistic, explodes on contact.
    Grenade,
}

/// [гипотеза] projectile numbers from HL2 defaults: bolt 3500 u/s (from the
/// reference: 3500, 1500 under water), 100 damage; grenade 1000 u/s (reference),
/// 100 damage in a 250-unit radius.
pub const BOLT_SPEED: f32 = 3500.0;
pub const BOLT_DAMAGE: f32 = 100.0;
pub const GRENADE_SPEED: f32 = 1000.0;
pub const GRENADE_DAMAGE: f32 = 100.0;
pub const GRENADE_RADIUS: f32 = 250.0;

impl WeaponSystem {
    /// Launches a projectile from the command's eye along its view.
    pub(crate) fn launch(&mut self, glua: &GLua, ply: EntRef, cmd: &WeaponCmd, kind: ProjectileKind) {
        let (model, speed, life) = match kind {
            ProjectileKind::Bolt => ("models/crossbow_bolt.mdl", BOLT_SPEED, 10.0),
            ProjectileKind::Grenade => ("models/items/ar2_grenade.mdl", GRENADE_SPEED, 10.0),
        };
        let f = srcds_lua::vecang::angle_vectors(cmd.angles).0;
        let Ok(mut st) = store(&glua.lua) else { return };
        let class = match kind {
            ProjectileKind::Bolt => "crossbow_bolt",
            ProjectileKind::Grenade => "grenade_ar2",
        };
        let Some(r) = st.create(class, None) else { return };
        if let Some(e) = st.get_mut(r) {
            e.model = model.into();
            e.pos = [cmd.eye[0] + f[0] * 16.0, cmd.eye[1] + f[1] * 16.0, cmd.eye[2] + f[2] * 16.0];
            e.ang = cmd.angles;
            e.vel = [f[0] * speed, f[1] * speed, f[2] * speed];
            e.owner = ply;
            e.spawned = true;
        }
        drop(st);
        self.projectiles.push(Projectile { ent: r, owner: ply, vel: [f[0] * speed, f[1] * speed, f[2] * speed], kind, expires: cmd.time + life, stuck: false });
    }

    /// Moves the projectiles one tick; returns players they killed.
    pub fn step_projectiles(&mut self, glua: &GLua, gravity: f32, dt: f32, now: f64) -> Vec<EntRef> {
        let mut killed = Vec::new();
        let mut keep = Vec::new();
        for mut p in std::mem::take(&mut self.projectiles) {
            if !entity::valid(&glua.lua, p.ent) {
                continue;
            }
            if now > p.expires {
                entity::remove(&glua.lua, p.ent);
                continue;
            }
            if p.stuck {
                keep.push(p);
                continue;
            }
            let g = match p.kind {
                ProjectileKind::Bolt => gravity * 0.05,
                ProjectileKind::Grenade => gravity,
            };
            p.vel[2] -= g * dt;
            let Ok(pos) = with(&glua.lua, p.ent, |e| e.pos) else { continue };
            let end = [pos[0] + p.vel[0] * dt, pos[1] + p.vel[1] * dt, pos[2] + p.vel[2] * dt];
            let hit = (|| -> srcds_lua::mlua::Result<(f32, [f32; 3], Option<EntRef>)> {
                let util: Table = glua.lua.globals().get("util")?;
                let tl: srcds_lua::mlua::Function = util.get("TraceLine")?;
                let t = glua.lua.create_table()?;
                t.set("start", srcds_lua::vecang::Vector(pos))?;
                t.set("endpos", srcds_lua::vecang::Vector(end))?;
                let filter = glua.lua.create_table()?;
                filter.set(1, crate::weapons::ud(glua, p.owner)?)?;
                filter.set(2, crate::weapons::ud(glua, p.ent)?)?;
                t.set("filter", filter)?;
                let tr: Table = tl.call(t)?;
                let frac: f32 = tr.get("Fraction").unwrap_or(1.0);
                let hp: srcds_lua::vecang::Vector = tr.get("HitPos")?;
                let ent = match tr.get::<Value>("Entity")? {
                    Value::UserData(u) => u.borrow::<entity::EntUd>().map(|e| e.0).unwrap_or(EntRef::NULL),
                    _ => EntRef::NULL,
                };
                Ok((frac, hp.0, (!ent.is_null() && ent.id != 0).then_some(ent)))
            })();
            let (frac, hit_pos, ent) = hit.unwrap_or((1.0, end, None));
            if frac >= 1.0 {
                let v = p.vel;
                let _ = with(&glua.lua, p.ent, |e| {
                    e.pos = end;
                    e.vel = v;
                    e.ang = srcds_lua::vecang::vector_angles(v);
                });
                keep.push(p);
                continue;
            }
            let speed = (p.vel[0] * p.vel[0] + p.vel[1] * p.vel[1] + p.vel[2] * p.vel[2]).sqrt().max(1e-3);
            let dir = [p.vel[0] / speed, p.vel[1] / speed, p.vel[2] / speed];
            match p.kind {
                ProjectileKind::Bolt => {
                    if let Some(e) = ent {
                        crate::weapons::apply_hit(glua, e, dir, BOLT_DAMAGE, &mut killed);
                        entity::remove(&glua.lua, p.ent);
                    } else {
                        // stuck in the world until it expires
                        let _ = with(&glua.lua, p.ent, |e| {
                            e.pos = hit_pos;
                            e.vel = [0.0; 3];
                        });
                        p.stuck = true;
                        p.expires = p.expires.min(now + 5.0);
                        keep.push(p);
                    }
                }
                ProjectileKind::Grenade => {
                    explode(glua, hit_pos, GRENADE_DAMAGE, GRENADE_RADIUS, &mut killed);
                    entity::remove(&glua.lua, p.ent);
                }
            }
        }
        self.projectiles = keep;
        killed
    }
}

/// Radius damage falling off linearly, pushing physics objects away.
fn explode(glua: &GLua, at: [f32; 3], damage: f32, radius: f32, killed: &mut Vec<EntRef>) {
    let targets: Vec<(EntRef, [f32; 3])> = store(&glua.lua)
        .map(|s| {
            s.ents
                .values()
                .filter(|e| e.spawned && !e.removing && e.r.id != 0 && (e.phys.is_some() || e.player.is_some() || e.max_health > 0))
                .map(|e| (e.r, [e.pos[0] + (e.mins[0] + e.maxs[0]) * 0.5, e.pos[1] + (e.mins[1] + e.maxs[1]) * 0.5, e.pos[2] + (e.mins[2] + e.maxs[2]) * 0.5]))
                .collect()
        })
        .unwrap_or_default();
    for (r, c) in targets {
        let d = [c[0] - at[0], c[1] - at[1], c[2] - at[2]];
        let l = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if l > radius {
            continue;
        }
        let k = 1.0 - l / radius;
        let dir = if l > 1e-3 { [d[0] / l, d[1] / l, d[2] / l] } else { [0.0, 0.0, 1.0] };
        crate::weapons::apply_hit(glua, r, dir, damage * k, killed);
    }
}
