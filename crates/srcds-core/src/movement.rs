//! Server-side player movement.
//!
//! The client predicts its own movement and corrects towards the server's result, so
//! the server must compute the same thing from the same inputs and parameters. The
//! parameters are sent to the client as replicated convars at signon
//! ([`MoveParams::replicated_convars`]), so both sides use identical values.
//!
//! The walking model follows the order of steps of the publicly available Source SDK
//! 2013 player movement (gravity in two half steps around the move, jump, friction,
//! ground/air acceleration, slide move with up to four clip planes, stair stepping,
//! sticking to the ground, ground categorisation with the quarter-box fallback, fall
//! velocity). It was checked against the unmodified client's prediction with
//! `cl_showerror 2`, which lists every field where server and client disagree.
//!
//! Not implemented yet: crouching (hull change), water, ladders, base velocity from
//! conveyors, surface materials, collision with displacements / props / other players.

use srcds_protocol::usercmd::{UserCmd, IN_DUCK, IN_FORWARD, IN_JUMP, IN_SPEED};

use crate::collision::{CollisionMap, Trace, DIST_EPSILON, MASK_PLAYERSOLID};

/// `IN_WALK` (slow walk).
pub const IN_WALK: u32 = 1 << 18;

/// Standing player hull.
pub const HULL_MIN: [f32; 3] = [-16.0, -16.0, 0.0];
pub const HULL_MAX: [f32; 3] = [16.0, 16.0, 72.0];
/// Crouching player hull.
pub const DUCK_HULL_MIN: [f32; 3] = [-16.0, -16.0, 0.0];
pub const DUCK_HULL_MAX: [f32; 3] = [16.0, 16.0, 36.0];
/// Eye heights standing / crouched.
pub const VIEW_STAND: f32 = 64.0;
pub const VIEW_DUCK: f32 = 28.0;
/// The duck timer counts down from this many milliseconds.
const DUCK_TIMER_MS: f32 = 1000.0;
/// `m_fFlags` bits handled by the movement code.
pub const FL_DUCKING: i32 = 1 << 1;
pub const FL_ANIMDUCKING: i32 = 1 << 2;

fn hull(ducked: bool) -> (V3, V3) {
    if ducked {
        (DUCK_HULL_MIN, DUCK_HULL_MAX)
    } else {
        (HULL_MIN, HULL_MAX)
    }
}

/// Smooth 0→1 ease (3x² − 2x³).
fn simple_spline(x: f32) -> f32 {
    let x2 = x * x;
    3.0 * x2 - 2.0 * x2 * x
}

/// Surfaces steeper than this (normal z below) are not ground.
const MIN_GROUND_NORMAL_Z: f32 = 0.7;
/// Moving up faster than this means the player is not standing on the ground.
const NON_JUMP_VELOCITY: f32 = 140.0;
/// At the start of a move, rising faster than this leaves the ground.
const LEAVE_GROUND_VELOCITY: f32 = 250.0;
const MAX_CLIP_PLANES: usize = 5;
/// Air acceleration only works up to this speed along the wish direction.
const AIR_SPEED_CAP: f32 = 30.0;
/// Friction factor while moving upwards in the air.
const AIR_RISING_SURFACE_FRICTION: f32 = 0.25;
/// Landing faster than this punches the view ([факт] the client predicted a roll
/// punch of 0.013 × fall speed after a ~430 u/s landing; HL2 threshold value).
const FALL_PUNCH_THRESHOLD: f32 = 303.0;
const PUNCH_DAMPING: f32 = 9.0;
const PUNCH_SPRING: f32 = 65.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveParams {
    /// `sv_maxspeed`
    pub max_speed: f32,
    /// `sv_noclipspeed`
    pub noclip_speed: f32,
    /// `sv_noclipaccelerate`
    pub noclip_accelerate: f32,
    /// `sv_friction`
    pub friction: f32,
    /// `sv_gravity`
    pub gravity: f32,
    /// `sv_accelerate`
    pub accelerate: f32,
    /// `sv_airaccelerate`
    pub air_accelerate: f32,
    /// `sv_stopspeed`
    pub stop_speed: f32,
    /// `sv_maxvelocity` (per axis)
    pub max_velocity: f32,
    /// Player walk / sprint (IN_SPEED) / slow walk (IN_WALK) speeds.
    pub walk_speed: f32,
    pub run_speed: f32,
    pub slow_walk_speed: f32,
    /// Vertical speed gained by a jump.
    pub jump_power: f32,
    pub step_size: f32,
    /// Seconds to crouch / to stand up (sandbox `DuckSpeed` / `UnDuckSpeed`).
    pub duck_time: f32,
    pub unduck_time: f32,
    /// Move speed multiplier while crouched on the ground (`CrouchedWalkSpeed`).
    pub crouched_speed: f32,
    /// `sv_sticktoground`: [факт] GMod convar (default 1, replicated) that disables
    /// leaving the ground when moving up faster than 140 u/s (running up slopes).
    pub stick_to_ground: bool,
    /// Sandbox's jump boost (`player_sandbox` StartMove/FinishMove, run by the client
    /// during prediction).
    pub jump_boost: bool,
    /// Ladder climb speed (`Player:GetLadderClimbSpeed`).
    pub ladder_speed: f32,
}

/// [факт] defaults are GMod's own (sv_stopspeed 10, sv_maxspeed 10000, … read from
/// the client after disconnecting) and the sandbox player class values.
impl Default for MoveParams {
    fn default() -> Self {
        Self {
            max_speed: 10000.0,
            noclip_speed: 5.0,
            noclip_accelerate: 5.0,
            friction: 8.0,
            gravity: 600.0,
            accelerate: 10.0,
            air_accelerate: 10.0,
            stop_speed: 10.0,
            max_velocity: 3500.0,
            walk_speed: 200.0,
            run_speed: 400.0,
            slow_walk_speed: 100.0,
            jump_power: 200.0,
            step_size: 18.0,
            duck_time: 0.1,
            unduck_time: 0.1,
            crouched_speed: 0.3,
            stick_to_ground: true,
            jump_boost: true,
            ladder_speed: 200.0,
        }
    }
}

impl MoveParams {
    /// Convars sent to the client so its prediction uses these exact values.
    pub fn replicated_convars(&self) -> Vec<(&'static str, String)> {
        vec![
            ("sv_maxspeed", fmt(self.max_speed)),
            ("sv_noclipspeed", fmt(self.noclip_speed)),
            ("sv_noclipaccelerate", fmt(self.noclip_accelerate)),
            ("sv_friction", fmt(self.friction)),
            ("sv_gravity", fmt(self.gravity)),
            ("sv_accelerate", fmt(self.accelerate)),
            ("sv_airaccelerate", fmt(self.air_accelerate)),
            ("sv_stopspeed", fmt(self.stop_speed)),
            ("sv_maxvelocity", fmt(self.max_velocity)),
            ("sv_sticktoground", (if self.stick_to_ground { "1" } else { "0" }).to_string()),
        ]
    }

    /// Speed limit (`m_flMaxspeed`) for the buttons held. [факт] the client predicts
    /// 200 while walking (prediction error `net 400 pred 200` when the server sent a
    /// fixed 400).
    pub fn wish_speed_limit(&self, buttons: u32) -> f32 {
        if buttons & IN_SPEED != 0 {
            self.run_speed
        } else if buttons & IN_WALK != 0 {
            self.slow_walk_speed
        } else {
            self.walk_speed
        }
    }
}

fn fmt(v: f32) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v}")
    }
}

type V3 = [f32; 3];

fn len(v: V3) -> f32 {
    (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt()
}

fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn add_scaled(a: V3, b: V3, s: f32) -> V3 {
    [a[0] + b[0] * s, a[1] + b[1] * s, a[2] + b[2] * s]
}

fn scale(a: V3, s: f32) -> V3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Forward and right unit vectors for pitch/yaw/roll in degrees.
pub fn angle_vectors(a: V3) -> (V3, V3) {
    let (sp, cp) = a[0].to_radians().sin_cos();
    let (sy, cy) = a[1].to_radians().sin_cos();
    let (sr, cr) = a[2].to_radians().sin_cos();
    let forward = [cp * cy, cp * sy, -sp];
    let right = [-sr * sp * cy + cr * sy, -sr * sp * sy - cr * cy, -sr * cp];
    (forward, right)
}

fn normalize(v: V3) -> (V3, f32) {
    let l = len(v);
    if l > 0.0 {
        ([v[0] / l, v[1] / l, v[2] / l], l)
    } else {
        ([0.0; 3], 0.0)
    }
}

/// Movement state of one player (persists between commands).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveState {
    pub origin: V3,
    pub velocity: V3,
    pub on_ground: bool,
    /// Buttons of the previous command (jump needs a fresh press).
    pub old_buttons: u32,
    /// `m_flFallVelocity`: downward speed while airborne, reset on landing.
    pub fall_velocity: f32,
    /// Friction factor of the current surface (1 normally, 0.25 rising in the air).
    pub surface_friction: f32,
    /// `m_flMaxspeed` used by the last command.
    pub max_speed: f32,
    /// Fully crouched (`m_bDucked`, `FL_DUCKING`): uses the crouch hull.
    pub ducked: bool,
    /// In a crouch / stand-up transition (`m_bDucking`).
    pub ducking: bool,
    /// `m_flDucktime`: transition timer in ms, counting down from 1000.
    pub duck_time: f32,
    /// `FL_ANIMDUCKING`.
    pub anim_ducking: bool,
    /// `m_vecViewOffset.z`.
    pub view_z: f32,
    /// `m_vecPunchAngle` / `m_vecPunchAngleVel`: view kick and its spring velocity.
    pub punch: V3,
    pub punch_vel: V3,
    /// Sprint (`m_fIsSprinting`) and slow-walk (`m_fIsWalking`) states.
    pub sprinting: bool,
    pub walking: bool,
    /// The ladder being climbed (index into the map's ladders) and its networked
    /// handle (`m_hLadder`, all ones = none).
    pub ladder: Option<usize>,
    pub ladder_handle: i32,
    /// A move onto / off a ladder in progress.
    pub forced: Option<ForcedMove>,
}

/// A timed straight move the player cannot steer (mounting or leaving a ladder).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ForcedMove {
    pub from: V3,
    pub to: V3,
    pub start: f64,
    pub end: f64,
    /// Ladder entered when the move ends (`None` = walking afterwards).
    pub onto: Option<usize>,
}

/// An HL2 useable ladder (`func_useableladder`) of the map.
#[derive(Debug, Clone, PartialEq)]
pub struct Ladder {
    pub bottom: V3,
    pub top: V3,
    /// Networked entity handle.
    pub handle: i32,
    /// `info_ladder_dismount` points near the ladder.
    pub dismounts: Vec<V3>,
}

/// What the ladder code decided for one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LadderOutcome {
    /// Not on a ladder: the normal walk move runs.
    Walk,
    /// On the ladder: the velocity was set, the ladder move runs.
    Ladder,
    /// A forced move placed the player: nothing else this command.
    Frozen,
}

const NO_LADDER: i32 = (1 << 23) - 1;
/// [факт] (reference server.dll) ladder search radius, automount view threshold
/// (`sv_ladderautomountdot` 0.4), leave distance and dismount-node range.
const LADDER_SEARCH: f32 = 64.0;
const LADDER_AUTOMOUNT_DOT: f32 = 0.4;
const LADDER_LEAVE_DIST: f32 = 36.0;
const LADDER_DISMOUNT_RANGE: f32 = 60.0;

/// Closest point of segment `a`–`b` to `p`, and the segment parameter.
fn closest_on_segment(p: V3, a: V3, b: V3) -> (V3, f32) {
    let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let l2 = dot(ab, ab);
    let t = if l2 > 0.0 {
        (dot([p[0] - a[0], p[1] - a[1], p[2] - a[2]], ab) / l2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (add_scaled(a, ab, t), t)
}

fn dist2(a: V3, b: V3) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    dot(d, d)
}

fn flat(v: V3) -> V3 {
    normalize([v[0], v[1], 0.0]).0
}

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

/// The nearest ladder in reach and in clear line from the player: its index and the
/// mount point (the closest point of its segment).
fn find_ladder(world: &CollisionMap, ladders: &[Ladder], s: &MoveState) -> Option<(usize, V3)> {
    let mut best: Option<(usize, V3, f32)> = None;
    let up = |v: V3, z: f32| [v[0], v[1], v[2] + z];
    for (i, l) in ladders.iter().enumerate() {
        let (c, _) = closest_on_segment(s.origin, l.bottom, l.top);
        let d2 = dist2(c, s.origin);
        if d2 > LADDER_SEARCH * LADDER_SEARCH || best.is_some_and(|b| d2 >= b.2) {
            continue;
        }
        // a small box from just above the feet, else the full hull
        let small = world.trace_box(up(s.origin, 5.0), up(c, 5.0), [-4.0; 3], [4.0; 3], MASK_PLAYERSOLID);
        let clear = small.fraction >= 1.0 || {
            let (mn, mx) = hull(s.ducked);
            let mid = (mx[2] - mn[2]) * 0.5;
            let t = world.trace_box(up(s.origin, mid), up(c, mid), [mn[0], mn[1], -mid], [mx[0], mx[1], mid], MASK_PLAYERSOLID);
            t.fraction >= 1.0
        };
        if clear {
            best = Some((i, c, d2));
        }
    }
    best.map(|(i, c, _)| (i, c))
}

fn start_forced(s: &mut MoveState, to: V3, speed: f32, now: f64, onto: Option<usize>, ladders: &[Ladder]) {
    let d = dist2(to, s.origin).sqrt();
    let secs = (d / speed.max(1.0)).max(0.001) as f64;
    s.forced = Some(ForcedMove { from: s.origin, to, start: now, end: now + secs, onto });
    s.velocity = [0.0; 3];
    s.on_ground = false;
    s.ladder = onto;
    s.ladder_handle = onto.and_then(|i| ladders.get(i)).map_or(NO_LADDER, |l| l.handle);
}

fn leave_ladder(s: &mut MoveState) {
    s.ladder = None;
    s.ladder_handle = NO_LADDER;
}

/// Leaves the ladder through the nearest dismount point in range (a forced move);
/// false when there is none.
fn dismount(s: &mut MoveState, ladders: &[Ladder], speed: f32, now: f64) -> bool {
    let Some(l) = s.ladder.and_then(|i| ladders.get(i)) else { return false };
    let best = l
        .dismounts
        .iter()
        .map(|d| (*d, dist2(*d, s.origin)))
        .filter(|(_, d2)| *d2 <= LADDER_DISMOUNT_RANGE * LADDER_DISMOUNT_RANGE)
        .min_by(|a, b| a.1.total_cmp(&b.1));
    let Some((to, _)) = best else { return false };
    start_forced(s, to, speed, now, None, ladders);
    true
}

/// HL2 useable-ladder logic for one command, run before the walk move: mounting by
/// walking at a ladder or pressing use, a timed move onto it, climbing along it,
/// leaving by jumping or moving away. (Own implementation of the behaviour observed
/// in the reference server; see docs/PROTOCOL.md.)
pub fn ladder_move(world: &CollisionMap, ladders: &[Ladder], s: &mut MoveState, cmd: &UserCmd, p: &MoveParams, now: f64) -> LadderOutcome {
    const IN_USE: u32 = 1 << 5;
    const IN_BACK: u32 = 1 << 4;
    const IN_MOVELEFT: u32 = 1 << 9;
    const IN_MOVERIGHT: u32 = 1 << 10;
    let speed = p.ladder_speed;
    if let Some(f) = s.forced {
        let frac = ((now - f.start) / (f.end - f.start).max(1e-6)).clamp(0.0, 1.0) as f32;
        s.origin = add_scaled(f.from, sub(f.to, f.from), frac);
        s.velocity = [0.0; 3];
        if frac >= 1.0 {
            s.forced = None;
            if f.onto.is_none() {
                leave_ladder(s);
            }
        }
        s.old_buttons = cmd.buttons;
        return LadderOutcome::Frozen;
    }
    let (fwd, right) = angle_vectors(cmd.view_angles);
    let use_pressed = cmd.buttons & IN_USE != 0 && s.old_buttons & IN_USE == 0;
    let Some(li) = s.ladder.filter(|i| *i < ladders.len()) else {
        leave_ladder(s);
        let Some((li, mount)) = find_ladder(world, ladders, s) else { return LadderOutcome::Walk };
        let l = &ladders[li];
        let axis = normalize(sub(l.top, l.bottom)).0;
        let mounts = if use_pressed {
            let f2 = flat(fwd);
            let from = [s.origin[0] - f2[0] * 8.0, s.origin[1] - f2[1] * 8.0, 0.0];
            dot(flat([mount[0] - from[0], mount[1] - from[1], 0.0]), f2) >= 0.0
        } else if cmd.forward_move > 0.0 {
            let d = dist2(mount, s.origin).sqrt();
            let facing = dot(flat(sub(mount, s.origin)), flat(fwd));
            let angle = facing.clamp(-1.0, 1.0).acos().to_degrees();
            let along = dot(axis, fwd).abs();
            let at_side = ((facing > 0.0 && cmd.side_move.abs() < 1.0) || along > LADDER_AUTOMOUNT_DOT)
                && (angle < 15.0 || d < 4.0)
                && d > 0.0
                && d < 32.0;
            // walking into either end while looking along the ladder
            let at_end = [(l.top, l.bottom), (l.bottom, l.top)]
                .iter()
                .any(|(end, other)| dist2(s.origin, *end) <= 256.0 && dot(normalize(sub(*other, *end)).0, fwd) > LADDER_AUTOMOUNT_DOT);
            at_side || at_end
        } else {
            false
        };
        if !mounts {
            return LadderOutcome::Walk;
        }
        start_forced(s, mount, speed, now, Some(li), ladders);
        s.old_buttons = cmd.buttons | IN_USE;
        return LadderOutcome::Frozen;
    };
    let l = &ladders[li];
    s.on_ground = false;
    if use_pressed && dismount(s, ladders, speed, now) {
        s.old_buttons = cmd.buttons;
        return LadderOutcome::Frozen;
    }
    let b = cmd.buttons;
    if b & IN_JUMP != 0 {
        // jump off along the view (backwards when pulling back), a little upwards
        leave_ladder(s);
        let dir = normalize(if cmd.forward_move < 0.0 { scale(fwd, -1.0) } else { fwd }).0;
        s.velocity = scale(dir, speed);
        if fwd[2] >= 0.0 {
            s.velocity[2] += 50.0;
        }
        return LadderOutcome::Walk;
    }
    let f = (if b & IN_FORWARD != 0 { speed } else { 0.0 }) - (if b & IN_BACK != 0 { speed } else { 0.0 });
    let r = (if b & IN_MOVERIGHT != 0 { speed } else { 0.0 }) - (if b & IN_MOVELEFT != 0 { speed } else { 0.0 });
    if f == 0.0 && r == 0.0 {
        s.velocity = [0.0; 3];
        return LadderOutcome::Ladder;
    }
    let wish = normalize(add_scaled(scale(fwd, f), right, r)).0;
    let axis = normalize(sub(l.top, l.bottom)).0;
    let (c, _) = closest_on_segment(s.origin, l.bottom, l.top);
    if dist2(c, s.origin) > LADDER_LEAVE_DIST * LADDER_LEAVE_DIST {
        leave_ladder(s);
        return LadderOutcome::Walk;
    }
    let tolerance = if (l.top[2] - l.bottom[2]).abs() >= 64.0 { 0.3 } else { 0.0 };
    let sign = if dot(axis, wish) < -tolerance { -1.0 } else { 1.0 };
    s.velocity = scale(axis, speed * sign);
    LadderOutcome::Ladder
}

/// The move on a ladder: the set velocity with collisions and no gravity; past either
/// end the player leaves through a dismount point (or simply lets go).
#[allow(clippy::too_many_arguments)]
pub fn ladder_fly(
    world: &CollisionMap,
    solids: &[SolidBox],
    ladders: &[Ladder],
    state: &mut MoveState,
    cmd: &UserCmd,
    p: &MoveParams,
    dt: f32,
    now: f64,
) {
    let mut m = Mover { world, solids, p, dt, s: *state, fmove: 0.0, smove: 0.0, view: cmd.view_angles };
    begin_command(&mut m.s, dt);
    m.s.on_ground = false;
    m.s.fall_velocity = 0.0;
    m.try_move();
    m.s.old_buttons = cmd.buttons;
    *state = m.s;
    let Some(l) = state.ladder.and_then(|i| ladders.get(i)) else { return };
    let moving = state.velocity != [0.0; 3];
    let (_, t) = closest_on_segment(state.origin, l.bottom, l.top);
    let going_up = state.velocity[2] > 0.0;
    let past_end = moving && ((t >= 1.0 && going_up) || (t <= 0.0 && !going_up));
    if past_end && !dismount(state, ladders, p.ladder_speed, now) {
        leave_ladder(state);
    }
}

/// Sprint / slow-walk transitions; `m_flMaxspeed` changes only when one starts or
/// stops. [факт] (live client prediction errors) the client switches sprinting with
/// IN_SPEED in noclip too, and keeps the speed it set there after landing; a
/// speed recomputed from the held keys each command disagreed both ways.
pub fn update_speed_state(s: &mut MoveState, buttons: u32, p: &MoveParams) {
    if s.max_speed <= 0.0 {
        s.max_speed = p.walk_speed;
    }
    // [факт] (live client) crouching ends sprinting: `m_fIsSprinting` false and the
    // walk speed in the same command as the duck
    let want_sprint = buttons & IN_SPEED != 0 && buttons & IN_DUCK == 0 && !s.walking && !(s.ducked && !s.ducking);
    if s.sprinting && (buttons & IN_SPEED == 0 || buttons & IN_DUCK != 0) {
        s.sprinting = false;
        s.max_speed = p.walk_speed;
    } else if !s.sprinting && want_sprint {
        s.sprinting = true;
        s.max_speed = p.run_speed;
    }
    let want_walk = buttons & IN_WALK != 0 && !s.sprinting && buttons & IN_DUCK == 0;
    if want_walk != s.walking {
        s.walking = want_walk;
        s.max_speed = if want_walk { p.slow_walk_speed } else { p.walk_speed };
    }
}

impl MoveState {
    pub fn new(origin: V3) -> Self {
        Self {
            origin,
            velocity: [0.0; 3],
            on_ground: false,
            old_buttons: 0,
            fall_velocity: 0.0,
            surface_friction: 1.0,
            max_speed: 0.0,
            ducked: false,
            ducking: false,
            duck_time: 0.0,
            anim_ducking: false,
            view_z: VIEW_STAND,
            punch: [0.0; 3],
            punch_vel: [0.0; 3],
            sprinting: false,
            walking: false,
            ladder: None,
            ladder_handle: NO_LADDER,
            forced: None,
        }
    }

    /// Movement-related `m_fFlags` bits.
    /// Networked duck flags. [факт] (live client prediction): `FL_ANIMDUCKING` follows
    /// the duck key — while standing up the client already has 259 (`FL_DUCKING` only)
    /// for the whole un-duck transition.
    /// Standing, not crouching (entering noclip: the client does not duck there).
    pub fn stand_up(&mut self) {
        self.ducked = false;
        self.ducking = false;
        self.anim_ducking = false;
        self.duck_time = 0.0;
        self.view_z = VIEW_STAND;
    }

    pub fn duck_flags(&self) -> i32 {
        (if self.ducked { FL_DUCKING } else { 0 }) | (if self.anim_ducking { FL_ANIMDUCKING } else { 0 })
    }

    pub fn hull(&self) -> (V3, V3) {
        hull(self.ducked)
    }
}

/// One command of noclip flight over `frametime` seconds.
pub fn noclip_move(
    origin: &mut V3,
    velocity: &mut V3,
    cmd: &UserCmd,
    p: &MoveParams,
    frametime: f32,
) {
    // [факт] GMod's noclip (reference server): direction = normalised(forward·sign of
    // forwardmove) + normalised(right·sign of sidemove) + up while jump is held,
    // normalised again; speed = sv_noclipspeed · 100, ×3 with IN_SPEED, ×0.1 with
    // IN_DUCK; the velocity is set directly (no acceleration or friction).
    let (forward, right) = angle_vectors(cmd.view_angles);
    let (f, _) = normalize(scale(forward, cmd.forward_move));
    let (r, _) = normalize(scale(right, cmd.side_move));
    let up = if cmd.buttons & IN_JUMP != 0 { 1.0 } else { 0.0 };
    let (dir, _) = normalize([f[0] + r[0], f[1] + r[1], f[2] + r[2] + up]);
    let mut speed = p.noclip_speed * 100.0;
    if cmd.buttons & IN_SPEED != 0 {
        speed *= 3.0;
    }
    if cmd.buttons & IN_DUCK != 0 {
        speed *= 0.1;
    }
    *velocity = scale(dir, speed);
    *origin = add_scaled(*origin, *velocity, frametime);
}
/// Adds speed towards `dir` up to `wish_speed`, limited by
/// `accel · dt · wish_speed · surface_friction`.
fn accelerate(velocity: &mut V3, dir: V3, wish_speed: f32, accel: f32, dt: f32, surface: f32) {
    let add = wish_speed - dot(*velocity, dir);
    if add <= 0.0 {
        return;
    }
    let a = (accel * dt * wish_speed * surface).min(add);
    *velocity = add_scaled(*velocity, dir, a);
}

/// Removes the part of `v` going into the plane `n` (`overbounce` 1 = slide).
fn clip_velocity(v: V3, n: V3, overbounce: f32) -> V3 {
    let mut out = add_scaled(v, n, -dot(v, n) * overbounce);
    let adjust = dot(out, n);
    if adjust < 0.0 {
        out = add_scaled(out, n, -adjust);
    }
    out
}

/// A solid entity the player collides with: convex planes (normal, distance).
/// Something solid the player collides with besides the world.
#[derive(Debug, Clone, PartialEq)]
pub enum SolidBox {
    /// A prop: the planes of its oriented box.
    Planes(Vec<([f32; 3], f32)>),
    /// A map brush entity (`*model`) at `origin`.
    Brush { model: usize, origin: [f32; 3] },
}

struct Mover<'a> {
    world: &'a CollisionMap,
    solids: &'a [SolidBox],
    p: &'a MoveParams,
    dt: f32,
    s: MoveState,
    /// Requested move after the speed clamp (`forwardmove`, `sidemove`).
    fmove: f32,
    smove: f32,
    view: V3,
}

impl Mover<'_> {
    fn trace(&self, from: V3, to: V3) -> Trace {
        self.trace_hull(from, to, self.s.ducked)
    }

    fn trace_hull(&self, from: V3, to: V3, ducked: bool) -> Trace {
        let (mn, mx) = hull(ducked);
        self.trace_box(from, to, mn, mx)
    }

    /// World trace plus the solids of entities (props, as oriented boxes).
    fn trace_box(&self, from: V3, to: V3, mn: V3, mx: V3) -> Trace {
        let mut t = self.world.trace_box(from, to, mn, mx, MASK_PLAYERSOLID);
        for solid in self.solids {
            let mut p = Trace {
                fraction: t.fraction,
                end: to,
                normal: [0.0; 3],
                start_solid: false,
                all_solid: false,
            };
            match solid {
                SolidBox::Planes(planes) => crate::collision::clip_box_to_planes(planes.iter().copied(), from, to, mn, mx, &mut p),
                SolidBox::Brush { model, origin } => p = self.world.trace_model(*model, *origin, from, to, mn, mx, MASK_PLAYERSOLID),
            }
            // starting inside a prop: ignore it (never trap the player)
            if p.start_solid {
                continue;
            }
            if p.fraction < t.fraction {
                t.fraction = p.fraction;
                t.normal = p.normal;
                t.end = [
                    from[0] + (to[0] - from[0]) * p.fraction,
                    from[1] + (to[1] - from[1]) * p.fraction,
                    from[2] + (to[2] - from[2]) * p.fraction,
                ];
            }
        }
        t
    }
    fn set_eye(&mut self, duck_fraction: f32) {
        self.s.view_z = VIEW_DUCK * duck_fraction + VIEW_STAND * (1.0 - duck_fraction);
    }

    /// Hull height difference between standing and crouching.
    fn hull_delta_z() -> f32 {
        (HULL_MAX[2] - HULL_MIN[2]) - (DUCK_HULL_MAX[2] - DUCK_HULL_MIN[2])
    }

    fn finish_duck(&mut self) {
        if self.s.ducked {
            return;
        }
        self.s.ducked = true;
        self.s.ducking = false;
        self.s.view_z = VIEW_DUCK;
        if !self.s.on_ground {
            // crouching in the air pulls the feet up
            self.s.origin[2] += Self::hull_delta_z();
        }
        self.fix_crouch_stuck();
        self.categorize();
    }

    /// Standing up is only possible where the standing hull fits.
    fn unduck_origin(&self) -> V3 {
        let mut o = self.s.origin;
        if !self.s.on_ground {
            o[2] -= Self::hull_delta_z();
        }
        o
    }

    fn can_unduck(&self) -> bool {
        let t = self.trace_hull(self.s.origin, self.unduck_origin(), false);
        !t.start_solid && t.fraction == 1.0
    }

    fn finish_unduck(&mut self) {
        let o = self.unduck_origin();
        self.s.ducked = false;
        self.s.ducking = false;
        self.s.anim_ducking = false;
        self.s.view_z = VIEW_STAND;
        self.s.duck_time = 0.0;
        self.s.origin = o;
        self.categorize();
    }

    /// Moves the player up (at most 36 units) out of a solid after crouching.
    fn fix_crouch_stuck(&mut self) {
        let solid = |m: &Self, o: V3| m.trace(o, o).start_solid;
        if !solid(self, self.s.origin) {
            return;
        }
        let start = self.s.origin;
        for _ in 0..36 {
            self.s.origin[2] += 1.0;
            if !solid(self, self.s.origin) {
                return;
            }
        }
        self.s.origin = start;
    }

    /// Crouch state machine (multiplayer variant: no duck-jump timers).
    fn duck(&mut self, buttons: u32, old_buttons: u32) {
        let changed = old_buttons ^ buttons;
        let pressed = changed & buttons;
        let released = changed & old_buttons;
        let in_air = !self.s.on_ground;
        let in_duck = self.s.ducked;
        // crouched on the ground: slower
        if in_duck && self.s.on_ground {
            self.fmove *= self.p.crouched_speed;
            self.smove *= self.p.crouched_speed;
        }
        let holding = buttons & IN_DUCK != 0;
        if !holding {
            self.s.anim_ducking = false;
        }
        if !(holding || self.s.ducking || in_duck) {
            if (self.s.view_z - VIEW_STAND).abs() > 0.1 {
                self.set_eye(0.0);
            }
            return;
        }
        if holding {
            self.s.anim_ducking = true;
            if pressed & IN_DUCK != 0 && !in_duck {
                self.s.duck_time = DUCK_TIMER_MS;
                self.s.ducking = true;
            }
            if self.s.ducking {
                let secs = (DUCK_TIMER_MS - self.s.duck_time).max(0.0) * 0.001;
                if secs > self.p.duck_time || in_duck || in_air {
                    self.finish_duck();
                } else {
                    self.set_eye(simple_spline(secs / self.p.duck_time));
                }
            }
            return;
        }
        // released: stand up when possible
        if released & IN_DUCK != 0 {
            if in_duck {
                self.s.duck_time = DUCK_TIMER_MS;
            } else if self.s.ducking && !self.s.ducked {
                // released before fully crouched: run the stand-up for the part done
                let elapsed = DUCK_TIMER_MS - self.s.duck_time;
                let frac = elapsed / (1000.0 * self.p.duck_time);
                let unduck_ms = 1000.0 * self.p.unduck_time;
                self.s.duck_time = DUCK_TIMER_MS - unduck_ms + frac * unduck_ms;
            }
        }
        if self.can_unduck() {
            if self.s.ducking || self.s.ducked {
                let secs = (DUCK_TIMER_MS - self.s.duck_time).max(0.0) * 0.001;
                if secs > self.p.unduck_time || in_air {
                    self.finish_unduck();
                } else {
                    self.set_eye(simple_spline(1.0 - secs / self.p.unduck_time));
                    self.s.ducking = true;
                }
            }
        } else if self.s.duck_time != DUCK_TIMER_MS {
            // under something: stay crouched and retry once free
            self.set_eye(1.0);
            self.s.duck_time = DUCK_TIMER_MS;
            self.s.ducked = true;
            self.s.ducking = false;
        }
    }

    fn check_velocity(&mut self) {
        for v in self.s.velocity.iter_mut() {
            if !v.is_finite() {
                *v = 0.0;
            }
            *v = v.clamp(-self.p.max_velocity, self.p.max_velocity);
        }
    }

    fn set_ground(&mut self, ground: bool) {
        self.s.on_ground = ground;
        if ground {
            self.s.velocity[2] = 0.0;
        }
    }

    /// Ground check: a short trace down from the feet; if the full hull only finds a
    /// steep plane, each quarter of the hull is tried (an edge may still stand on a
    /// walkable surface). Does not move the player.
    fn categorize(&mut self) {
        self.s.surface_friction = 1.0;
        let o = self.s.origin;
        if !self.p.stick_to_ground && self.s.velocity[2] > NON_JUMP_VELOCITY {
            self.set_ground(false);
            return;
        }
        // [факт] GMod (reference server) probes from 2 units above the feet to 1
        // below; stock Source traces from the feet to 2 below. With the stock probe the
        // server landed a tick before the client on slopes.
        let up = [o[0], o[1], o[2] + 2.0];
        let down = [o[0], o[1], o[2] - 1.0];
        let standable =
            |t: &Trace| (t.fraction < 1.0 || t.start_solid) && t.normal[2] >= MIN_GROUND_NORMAL_Z;
        let t = self.trace(up, down);
        if standable(&t) {
            self.set_ground(true);
            return;
        }
        let (mn, mx) = self.s.hull();
        let quarters = [
            (
                [mn[0], mn[1], mn[2]],
                [mx[0].min(0.0), mx[1].min(0.0), mx[2]],
            ),
            (
                [mn[0].max(0.0), mn[1].max(0.0), mn[2]],
                [mx[0], mx[1], mx[2]],
            ),
            (
                [mn[0], mn[1].max(0.0), mn[2]],
                [mx[0].min(0.0), mx[1], mx[2]],
            ),
            (
                [mn[0].max(0.0), mn[1], mn[2]],
                [mx[0], mx[1].min(0.0), mx[2]],
            ),
        ];
        for (qmin, qmax) in quarters {
            let t = self.trace_box(up, down, qmin, qmax);
            if standable(&t) {
                self.set_ground(true);
                return;
            }
        }
        self.set_ground(false);
        if self.s.velocity[2] > 0.0 {
            self.s.surface_friction = AIR_RISING_SURFACE_FRICTION;
        }
    }

    fn friction(&mut self) {
        let v = self.s.velocity;
        let speed = len(v);
        if speed < 0.1 {
            return;
        }
        let mut drop = 0.0;
        if self.s.on_ground {
            let control = speed.max(self.p.stop_speed);
            drop = control * self.p.friction * self.s.surface_friction * self.dt;
        }
        let new = (speed - drop).max(0.0);
        if new != speed {
            self.s.velocity = scale(v, new / speed);
        }
    }

    /// Slide move: up to four bumps, clipping velocity against hit planes.
    fn try_move(&mut self) {
        let primal = self.s.velocity;
        let mut original = self.s.velocity;
        let mut planes: Vec<V3> = Vec::with_capacity(MAX_CLIP_PLANES);
        let mut time_left = self.dt;
        for _ in 0..4 {
            if len(self.s.velocity) == 0.0 {
                break;
            }
            let end = add_scaled(self.s.origin, self.s.velocity, time_left);
            let t = self.trace(self.s.origin, end);
            if t.all_solid {
                self.s.velocity = [0.0; 3];
                return;
            }
            if t.fraction > 0.0 {
                self.s.origin = t.end;
                original = self.s.velocity;
                planes.clear();
            }
            if t.fraction == 1.0 {
                break;
            }
            time_left -= time_left * t.fraction;
            if planes.len() >= MAX_CLIP_PLANES {
                self.s.velocity = [0.0; 3];
                break;
            }
            planes.push(t.normal);
            if planes.len() == 1 && !self.s.on_ground {
                // first impact while airborne: plain slide (sv_bounce = 0)
                let v = clip_velocity(original, planes[0], 1.0);
                self.s.velocity = v;
                original = v;
                continue;
            }
            // find a plane whose slide does not run into any other plane hit so far
            let mut found = false;
            for (i, pl) in planes.iter().enumerate() {
                self.s.velocity = clip_velocity(original, *pl, 1.0);
                let v = self.s.velocity;
                if planes
                    .iter()
                    .enumerate()
                    .all(|(j, q)| j == i || dot(v, *q) >= 0.0)
                {
                    found = true;
                    break;
                }
            }
            if !found {
                if planes.len() != 2 {
                    self.s.velocity = [0.0; 3];
                    break;
                }
                // move along the crease of the two planes
                let (dir, _) = normalize(cross(planes[0], planes[1]));
                let d = dot(dir, self.s.velocity);
                self.s.velocity = scale(dir, d);
            }
            // moving against the original direction: stop (avoids jitter in corners)
            if dot(self.s.velocity, primal) <= 0.0 {
                self.s.velocity = [0.0; 3];
                break;
            }
        }
    }

    /// Slide move that may also step up onto ledges up to `step_size` high.
    fn step_move(&mut self) {
        let (start_pos, start_vel) = (self.s.origin, self.s.velocity);
        self.try_move();
        let (down_pos, down_vel) = (self.s.origin, self.s.velocity);

        self.s.origin = start_pos;
        self.s.velocity = start_vel;
        let up = [
            start_pos[0],
            start_pos[1],
            start_pos[2] + self.p.step_size + DIST_EPSILON,
        ];
        let t = self.trace(self.s.origin, up);
        if !t.start_solid && !t.all_solid {
            self.s.origin = t.end;
        }
        self.try_move();
        let o = self.s.origin;
        let t = self.trace(o, [o[0], o[1], o[2] - self.p.step_size - DIST_EPSILON]);
        // not standing on anything walkable after stepping: keep the plain slide
        if t.normal[2] < MIN_GROUND_NORMAL_Z {
            self.s.origin = down_pos;
            self.s.velocity = down_vel;
            return;
        }
        if !t.start_solid && !t.all_solid {
            self.s.origin = t.end;
        }
        let up_pos = self.s.origin;
        let dist = |p: V3| (p[0] - start_pos[0]).powi(2) + (p[1] - start_pos[1]).powi(2);
        if dist(down_pos) > dist(up_pos) {
            self.s.origin = down_pos;
            self.s.velocity = down_vel;
        } else {
            self.s.velocity[2] = down_vel[2];
        }
    }

    /// Keeps a walking player on the ground when going down slopes and steps.
    fn stay_on_ground(&mut self) {
        let o = self.s.origin;
        let t = self.trace(o, [o[0], o[1], o[2] + 2.0]);
        let start = t.end;
        let t = self.trace(start, [o[0], o[1], o[2] - self.p.step_size]);
        if t.fraction > 0.0
            && t.fraction < 1.0
            && !t.start_solid
            && t.normal[2] >= MIN_GROUND_NORMAL_Z
            && (self.s.origin[2] - t.end[2]).abs() > 0.5 * (1.0 / 32.0)
        {
            self.s.origin = t.end;
        }
    }

    /// Horizontal wish direction and speed from the (already clamped) move.
    fn wish(&self) -> (V3, f32) {
        let (mut f, mut r) = angle_vectors(self.view);
        f[2] = 0.0;
        r[2] = 0.0;
        let (f, _) = normalize(f);
        let (r, _) = normalize(r);
        let w = [
            f[0] * self.fmove + r[0] * self.smove,
            f[1] * self.fmove + r[1] * self.smove,
            0.0,
        ];
        let (dir, speed) = normalize(w);
        (dir, speed.min(self.s.max_speed))
    }

    fn walk(&mut self) {
        let (dir, speed) = self.wish();
        self.s.velocity[2] = 0.0;
        let sf = self.s.surface_friction;
        accelerate(
            &mut self.s.velocity,
            dir,
            speed,
            self.p.accelerate,
            self.dt,
            sf,
        );
        self.s.velocity[2] = 0.0;
        if len(self.s.velocity) < 1.0 {
            self.s.velocity = [0.0; 3];
            return;
        }
        let o = self.s.origin;
        let dest = [
            o[0] + self.s.velocity[0] * self.dt,
            o[1] + self.s.velocity[1] * self.dt,
            o[2],
        ];
        let t = self.trace(o, dest);
        if t.fraction == 1.0 {
            self.s.origin = t.end;
            self.stay_on_ground();
            return;
        }
        self.step_move();
        self.stay_on_ground();
    }

    fn air(&mut self) {
        let (dir, speed) = self.wish();
        let add = speed.min(AIR_SPEED_CAP) - dot(self.s.velocity, dir);
        if add > 0.0 {
            let a = (self.p.air_accelerate * speed * self.dt * self.s.surface_friction).min(add);
            self.s.velocity = add_scaled(self.s.velocity, dir, a);
        }
        self.try_move();
    }
}

/// One command of walking movement over `frametime` seconds.
pub fn walk_move(
    world: &CollisionMap,
    state: &mut MoveState,
    cmd: &UserCmd,
    p: &MoveParams,
    frametime: f32,
) {
    walk_move_with(world, &[], state, cmd, p, frametime)
}

/// Ground check only (no movement), as the engine runs at the start of every command
/// whose move type is not walk (noclip): leaving noclip near the ground then starts
/// standing, like the client.
pub fn categorize_state(world: &CollisionMap, solids: &[SolidBox], state: &mut MoveState, p: &MoveParams) {
    let mut m = Mover {
        world,
        solids,
        p,
        dt: 0.0,
        s: *state,
        fmove: 0.0,
        smove: 0.0,
        view: [0.0; 3],
    };
    m.categorize();
    *state = m.s;
}

/// [`walk_move`] that also collides with entity boxes.
pub fn walk_move_with(
    world: &CollisionMap,
    solids: &[SolidBox],
    state: &mut MoveState,
    cmd: &UserCmd,
    p: &MoveParams,
    frametime: f32,
) {
    // Speed limit for this command and the requested move clamped to it.
    update_speed_state(state, cmd.buttons, p);
    let max_speed = state.max_speed;
    let (mut fmove, mut smove, umove) = (cmd.forward_move, cmd.side_move, cmd.up_move);
    let spd = (fmove * fmove + smove * smove + umove * umove).sqrt();
    if spd != 0.0 && spd > max_speed {
        fmove *= max_speed / spd;
        smove *= max_speed / spd;
    }
    let mut m = Mover {
        world,
        solids,
        p,
        dt: frametime,
        s: *state,
        fmove,
        smove,
        view: cmd.view_angles,
    };
    m.s.max_speed = max_speed;
    // The ground state carries over from the previous command; only a fast upward
    // motion leaves it here.
    if m.s.velocity[2] > LEAVE_GROUND_VELOCITY {
        m.set_ground(false);
    }
    begin_command(&mut m.s, frametime);
    // timers
    m.s.duck_time = (m.s.duck_time - 1000.0 * frametime).max(0.0);
    if !m.s.on_ground {
        m.s.fall_velocity = -m.s.velocity[2];
    }
    let old_buttons = m.s.old_buttons;
    // sandbox: a jump pressed while on the ground (checked before the move)
    let boost = p.jump_boost && cmd.buttons & IN_JUMP != 0 && old_buttons & IN_JUMP == 0 && m.s.on_ground;
    m.duck(cmd.buttons, old_buttons);
    let half_g = p.gravity * 0.5 * frametime;
    // first half of gravity (also while standing; cleared below if still grounded)
    m.s.velocity[2] -= half_g;
    m.check_velocity();
    if cmd.buttons & IN_JUMP != 0 {
        let fresh = m.s.old_buttons & IN_JUMP == 0;
        // no jumping in the middle of standing up from a crouch
        let unducking = m.s.ducking && m.s.ducked;
        if m.s.on_ground && fresh && !unducking {
            m.set_ground(false);
            if m.s.ducking || m.s.ducked {
                m.s.velocity[2] = p.jump_power;
            } else {
                m.s.velocity[2] += p.jump_power;
            }
            // the jump's own second half of gravity
            m.s.velocity[2] -= half_g;
            m.check_velocity();
        }
    }
    if m.s.on_ground {
        m.s.velocity[2] = 0.0;
        m.friction();
    }
    m.check_velocity();
    if m.s.on_ground {
        m.walk();
    } else {
        m.air();
    }
    m.categorize();
    m.check_velocity();
    m.s.velocity[2] -= half_g;
    m.check_velocity();
    if m.s.on_ground {
        m.s.velocity[2] = 0.0;
        if m.s.fall_velocity > 0.0 {
            // hard landing knocks the view (roll) a little
            if m.s.fall_velocity >= FALL_PUNCH_THRESHOLD {
                m.s.punch[2] = m.s.fall_velocity * 0.013;
            }
            m.s.fall_velocity = 0.0;
        }
    }
    if boost {
        sandbox_jump_boost(&mut m.s, cmd.view_angles[1], m.fmove);
    }
    m.s.old_buttons = cmd.buttons;
    *state = m.s;
}

/// Sandbox's HL2-style jump boost: forward speed added along the view yaw, half the
/// (clamped) forward move standing / a tenth crouched, capped so the horizontal speed
/// stays within the speed limit times the same factor, reversed when moving backwards.
fn sandbox_jump_boost(s: &mut MoveState, yaw: f32, fmove: f32) {
    let (sy, cy) = yaw.to_radians().sin_cos();
    let forward = [cy, sy, 0.0];
    let perc = if s.ducked { 0.1 } else { 0.5 };
    let mut add = (fmove * perc).abs();
    let max = s.max_speed * (1.0 + perc);
    let speed2d = (s.velocity[0] * s.velocity[0] + s.velocity[1] * s.velocity[1]).sqrt();
    if add + speed2d > max {
        add -= add + speed2d - max;
    }
    if dot(s.velocity, forward) < 0.0 {
        add = -add;
    }
    s.velocity[0] += forward[0] * add;
    s.velocity[1] += forward[1] * add;
}

/// Lets the view punch spring back towards zero (damped spring).
fn decay_punch(s: &mut MoveState, dt: f32) {
    let l2 = |v: V3| dot(v, v);
    if l2(s.punch) <= 0.001 && l2(s.punch_vel) <= 0.001 {
        s.punch = [0.0; 3];
        s.punch_vel = [0.0; 3];
        return;
    }
    s.punch = add_scaled(s.punch, s.punch_vel, dt);
    let damping = (1.0 - PUNCH_DAMPING * dt).max(0.0);
    s.punch_vel = scale(s.punch_vel, damping);
    let spring = (PUNCH_SPRING * dt).clamp(0.0, 2.0);
    s.punch_vel = add_scaled(s.punch_vel, s.punch, -spring);
    s.punch = [
        s.punch[0].clamp(-89.0, 89.0),
        s.punch[1].clamp(-179.0, 179.0),
        s.punch[2].clamp(-89.0, 89.0),
    ];
}

/// Per-command bookkeeping shared by all move types (view punch decay).
pub fn begin_command(state: &mut MoveState, frametime: f32) {
    decay_punch(state, frametime);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collision::tests::{box_world, merge};

    fn fwd(yaw: f32) -> UserCmd {
        UserCmd {
            view_angles: [0.0, yaw, 0.0],
            forward_move: 10000.0,
            ..Default::default()
        }
    }

    #[test]
    fn vectors() {
        let (f, r) = angle_vectors([0.0, 90.0, 0.0]);
        assert!((f[1] - 1.0).abs() < 1e-6 && f[0].abs() < 1e-6);
        // right of "facing +y" is +x
        assert!((r[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn noclip_moves_at_fixed_gmod_speeds() {
        let p = MoveParams::default();
        let (mut o, mut v) = ([0.0f32; 3], [0.0f32; 3]);
        // forwardmove 10000 (what the GMod client sends) still gives 5·100 u/s
        let mut cmd = fwd(0.0);
        cmd.forward_move = 10000.0;
        noclip_move(&mut o, &mut v, &cmd, &p, 0.015);
        assert!((len(v) - 500.0).abs() < 1e-3, "{v:?}");
        assert!(o[0] > 0.0 && o[1].abs() < 1e-3 && o[2].abs() < 1e-3);
        cmd.buttons = IN_SPEED;
        noclip_move(&mut o, &mut v, &cmd, &p, 0.015);
        assert!((len(v) - 1500.0).abs() < 1e-2);
        cmd.buttons = IN_DUCK;
        noclip_move(&mut o, &mut v, &cmd, &p, 0.015);
        assert!((len(v) - 50.0).abs() < 1e-3);
        // no input: stops at once
        noclip_move(&mut o, &mut v, &UserCmd::default(), &p, 0.015);
        assert_eq!(v, [0.0; 3]);
    }
    #[test]
    fn rotated_prop_blocks_by_its_real_shape() {
        let (w, p) = (floor(), MoveParams::default());
        // a 40×40 box turned 45° at x=100: a diamond reaching ±28.3 on both axes
        let (f, r, u) = srcds_lua::vecang::angle_vectors([0.0, 45.0, 0.0]);
        let solid = SolidBox::Planes(crate::collision::oriented_box_planes(
            [100.0, 0.0, 0.0],
            [f, [-r[0], -r[1], -r[2]], u],
            [-20.0, -20.0, 0.0],
            [20.0, 20.0, 40.0],
        ));
        let run = |y: f32| {
            let mut s = state(0.0);
            s.origin = [0.0, y, 0.05];
            s.on_ground = true;
            for _ in 0..200 {
                walk_move_with(&w, std::slice::from_ref(&solid), &mut s, &fwd(0.0), &p, 0.015);
            }
            s.origin[0]
        };
        // straight at the corner: stops at 100 - 28.3 - 16
        let head_on = run(0.0);
        assert!((head_on - 55.7).abs() < 1.0, "{head_on}");
        // passing at y=40 (hull edge at y=24): the bounding box would stop at 55.7, the
        // diamond is only 4.3 wide there, so the player gets much further (or slides past)
        let side = run(40.0);
        assert!(side > 75.0, "{side}");
    }

    fn floor() -> CollisionMap {
        box_world([-4096.0, -4096.0, -64.0], [4096.0, 4096.0, 0.0])
    }

    fn state(z: f32) -> MoveState {
        MoveState::new([0.0, 0.0, z])
    }

    #[test]
    fn falls_and_lands() {
        let (w, p) = (floor(), MoveParams::default());
        let mut s = state(100.0);
        let mut max_fall = 0.0f32;
        for _ in 0..200 {
            walk_move(&w, &mut s, &UserCmd::default(), &p, 0.015);
            max_fall = max_fall.max(s.fall_velocity);
        }
        assert!(s.on_ground);
        assert!(s.origin[2] >= 0.0 && s.origin[2] < 0.1, "{:?}", s.origin);
        assert_eq!(s.velocity, [0.0; 3]);
        assert!(max_fall > 300.0 && s.fall_velocity == 0.0);
    }

    #[test]
    fn walks_at_walk_speed_and_sprints() {
        let (w, p) = (floor(), MoveParams::default());
        let mut s = state(0.05);
        for _ in 0..200 {
            walk_move(&w, &mut s, &fwd(0.0), &p, 0.015);
        }
        assert!(s.on_ground);
        let speed = len(s.velocity);
        assert!((speed - 200.0).abs() < 1.0, "{speed}");
        assert_eq!(s.max_speed, 200.0);
        assert!(s.origin[0] > 400.0 && s.origin[2] < 0.1);
        let mut run = fwd(0.0);
        run.buttons = IN_SPEED;
        for _ in 0..200 {
            walk_move(&w, &mut s, &run, &p, 0.015);
        }
        assert!((len(s.velocity) - 400.0).abs() < 1.0);
        for _ in 0..100 {
            walk_move(&w, &mut s, &UserCmd::default(), &p, 0.015);
        }
        assert_eq!(s.velocity, [0.0; 3]);
    }

    #[test]
    fn jump_matches_client_prediction() {
        let (w, p) = (floor(), MoveParams::default());
        let mut s = state(0.05);
        walk_move(&w, &mut s, &UserCmd::default(), &p, 0.015);
        assert!(s.on_ground);
        let jump = UserCmd {
            buttons: IN_JUMP,
            ..Default::default()
        };
        walk_move(&w, &mut s, &jump, &p, 0.015);
        // [факт] the client predicts vz = 186.5 after the jump command
        // (200 − three half steps of gravity of 4.5)
        assert!((s.velocity[2] - 186.5).abs() < 1e-3, "{:?}", s.velocity);
        let mut peak = 0.0f32;
        let mut landed = false;
        for _ in 0..200 {
            walk_move(&w, &mut s, &jump, &p, 0.015);
            peak = peak.max(s.origin[2]);
            landed |= s.on_ground;
        }
        assert!((peak - 33.0).abs() < 2.0, "{peak}");
        // holding jump does not jump again after landing
        assert!(landed && s.on_ground);
    }

    #[test]
    fn wall_blocks_and_step_climbs() {
        let p = MoveParams::default();
        let step = box_world([64.0, -4096.0, -64.0], [4096.0, 4096.0, 16.0]);
        let both = merge(&floor(), &step);
        let mut s = state(0.05);
        for _ in 0..300 {
            walk_move(&both, &mut s, &fwd(0.0), &p, 0.015);
        }
        assert!(s.origin[0] > 100.0, "{:?}", s.origin);
        assert!((s.origin[2] - 16.0).abs() < 0.1, "{:?}", s.origin);
        let wall = box_world([64.0, -4096.0, -64.0], [4096.0, 4096.0, 100.0]);
        let both = merge(&floor(), &wall);
        let mut s = state(0.05);
        for _ in 0..300 {
            walk_move(&both, &mut s, &fwd(0.0), &p, 0.015);
        }
        assert!(
            s.origin[0] < 48.0 + 0.1 && s.origin[2] < 0.1,
            "{:?}",
            s.origin
        );
    }

    #[test]
    fn walking_off_a_ledge_falls() {
        let p = MoveParams::default();
        // ledge: floor only for x < 0, ground far below
        let ledge = box_world([-4096.0, -4096.0, -64.0], [0.0, 4096.0, 0.0]);
        let low = box_world([-4096.0, -4096.0, -564.0], [4096.0, 4096.0, -500.0]);
        let w = merge(&ledge, &low);
        let mut s = MoveState::new([-100.0, 0.0, 0.05]);
        for _ in 0..300 {
            walk_move(&w, &mut s, &fwd(0.0), &p, 0.015);
        }
        assert!(
            s.on_ground && (s.origin[2] + 500.0).abs() < 0.1,
            "{:?}",
            s.origin
        );
    }

    /// Terrain (displacement) ramp rising along +x with slope `rise` per unit.
    fn terrain_ramp(rise: f32) -> CollisionMap {
        let size = 17;
        let cell = 64.0;
        let mut h = vec![0.0; size * size];
        for i in 0..size {
            for j in 0..size {
                h[i * size + j] = (i as f32 * cell * rise).max(0.0);
            }
        }
        // flat start area at x < 0, ramp from x = 0
        let ramp =
            crate::displacement::Displacements::from_heights([0.0, -512.0, 0.0], cell, size, &h);
        box_world([-4096.0, -4096.0, -64.0], [0.0, 4096.0, 0.0]).with_displacements(ramp)
    }

    #[test]
    fn walks_up_and_down_a_terrain_slope() {
        let p = MoveParams::default();
        // 30°: walkable (normal z ≈ 0.87)
        let w = terrain_ramp(0.577);
        let mut s = MoveState::new([-100.0, 0.0, 0.05]);
        for _ in 0..300 {
            walk_move(&w, &mut s, &fwd(0.0), &p, 0.015);
        }
        assert!(s.on_ground, "{s:?}");
        // a box rests on a slope with its uphill bottom edge (x + 16)
        let expect = (s.origin[0] + 16.0) * 0.577;
        assert!(s.origin[0] > 300.0, "{:?}", s.origin);
        assert!(
            (s.origin[2] - expect).abs() < 2.0,
            "{:?} vs {expect}",
            s.origin
        );
        // and back down without leaving the ground
        let mut airborne = 0;
        for _ in 0..200 {
            walk_move(&w, &mut s, &fwd(180.0), &p, 0.015);
            airborne += (!s.on_ground) as i32;
        }
        assert!(airborne < 5, "{airborne}");
        // 60°: too steep, the player cannot walk up it
        let w = terrain_ramp(1.732);
        let mut s = MoveState::new([-100.0, 0.0, 0.05]);
        for _ in 0..300 {
            walk_move(&w, &mut s, &fwd(0.0), &p, 0.015);
        }
        assert!(s.origin[2] < 40.0, "{:?}", s.origin);
    }

    #[test]
    fn hard_landing_punches_the_view_then_settles() {
        let (w, p) = (floor(), MoveParams::default());
        let mut s = state(160.0);
        let mut max_roll = 0.0f32;
        for _ in 0..200 {
            walk_move(&w, &mut s, &UserCmd::default(), &p, 0.015);
            max_roll = max_roll.max(s.punch[2]);
        }
        // ~438 u/s landing → ~5.7° roll, then the spring returns to zero
        assert!((max_roll - 5.7).abs() < 0.3, "{max_roll}");
        assert_eq!(s.punch, [0.0; 3]);
        // a small hop does not punch
        let mut s = state(20.0);
        for _ in 0..100 {
            walk_move(&w, &mut s, &UserCmd::default(), &p, 0.015);
            assert_eq!(s.punch, [0.0; 3]);
        }
    }

    #[test]
    fn crouch_and_stand_up() {
        let (w, p) = (floor(), MoveParams::default());
        let mut s = state(0.05);
        walk_move(&w, &mut s, &UserCmd::default(), &p, 0.015);
        let duck = UserCmd {
            buttons: IN_DUCK,
            ..Default::default()
        };
        // transition takes duck_time (0.1 s ≈ 7 ticks)
        walk_move(&w, &mut s, &duck, &p, 0.015);
        assert!(s.ducking && !s.ducked && s.duck_time > 900.0);
        for _ in 0..10 {
            walk_move(&w, &mut s, &duck, &p, 0.015);
        }
        assert!(s.ducked && !s.ducking);
        assert_eq!(s.view_z, VIEW_DUCK);
        assert_eq!(s.duck_flags(), FL_DUCKING | FL_ANIMDUCKING);
        // crouched walking is slower: 200 · 0.3
        let mut duck_fwd = fwd(0.0);
        duck_fwd.buttons = IN_DUCK;
        for _ in 0..200 {
            walk_move(&w, &mut s, &duck_fwd, &p, 0.015);
        }
        assert!((len(s.velocity) - 60.0).abs() < 1.0, "{:?}", s.velocity);
        // release: stands up again
        for _ in 0..20 {
            walk_move(&w, &mut s, &UserCmd::default(), &p, 0.015);
        }
        assert!(!s.ducked && !s.ducking && s.view_z == VIEW_STAND);
        // cannot stand up under a low ceiling
        let ceiling = box_world([-4096.0, -4096.0, 50.0], [4096.0, 4096.0, 100.0]);
        let w2 = merge(&floor(), &ceiling);
        let mut s = state(0.05);
        walk_move(&w2, &mut s, &UserCmd::default(), &p, 0.015);
        for _ in 0..20 {
            walk_move(&w2, &mut s, &duck, &p, 0.015);
        }
        assert!(s.ducked);
        for _ in 0..20 {
            walk_move(&w2, &mut s, &UserCmd::default(), &p, 0.015);
        }
        assert!(s.ducked, "stood up into the ceiling");
    }

    #[test]
    fn convars_format() {
        let c = MoveParams::default().replicated_convars();
        assert!(c.contains(&("sv_noclipspeed", "5".to_string())));
        assert!(c.contains(&("sv_gravity", "600".to_string())));
    }

    /// Sprinting starts / stops with IN_SPEED (also in noclip) and only the
    /// transitions change the speed limit.
    #[test]
    fn sprint_state_sets_speed_on_transitions() {
        let p = MoveParams::default();
        let mut s = MoveState::new([0.0; 3]);
        update_speed_state(&mut s, 0, &p);
        assert_eq!(s.max_speed, p.walk_speed);
        update_speed_state(&mut s, IN_SPEED, &p);
        assert!(s.sprinting);
        assert_eq!(s.max_speed, p.run_speed);
        update_speed_state(&mut s, IN_SPEED, &p);
        assert_eq!(s.max_speed, p.run_speed);
        update_speed_state(&mut s, 0, &p);
        assert!(!s.sprinting);
        assert_eq!(s.max_speed, p.walk_speed);
        update_speed_state(&mut s, IN_WALK, &p);
        assert!(s.walking);
        assert_eq!(s.max_speed, p.slow_walk_speed);
        update_speed_state(&mut s, 0, &p);
        assert_eq!(s.max_speed, p.walk_speed);
    }

    /// A useable ladder: walking at it mounts it (a timed move onto it), W while
    /// looking up climbs, and the top end leaves through the dismount point.
    #[test]
    fn useable_ladder_mount_climb_dismount() {
        let (w, p) = (floor(), MoveParams::default());
        let ladders = vec![Ladder { bottom: [40.0, 0.0, 0.0], top: [40.0, 0.0, 120.0], handle: 77, dismounts: vec![[80.0, 0.0, 130.0]] }];
        let mut s = state(0.0);
        s.on_ground = true;
        let dt = 0.015f32;
        let mut cmd = fwd(0.0);
        cmd.buttons = IN_FORWARD;
        let mut now = 0.0f64;
        let mut mounted_at = None;
        for i in 0..200 {
            let out = ladder_move(&w, &ladders, &mut s, &cmd, &p, now);
            match out {
                LadderOutcome::Walk => walk_move(&w, &mut s, &cmd, &p, dt),
                LadderOutcome::Ladder => ladder_fly(&w, &[], &ladders, &mut s, &cmd, &p, dt, now),
                LadderOutcome::Frozen => {}
            }
            now += dt as f64;
            if s.ladder.is_some() && s.forced.is_none() && mounted_at.is_none() {
                mounted_at = Some(i);
                break;
            }
        }
        assert!(mounted_at.is_some(), "mounted {s:?}");
        assert_eq!(s.ladder_handle, 77);
        // look up and climb
        cmd.view_angles = [-60.0, 0.0, 0.0];
        let z0 = s.origin[2];
        let mut left = false;
        for _ in 0..200 {
            match ladder_move(&w, &ladders, &mut s, &cmd, &p, now) {
                LadderOutcome::Walk => walk_move(&w, &mut s, &cmd, &p, dt),
                LadderOutcome::Ladder => ladder_fly(&w, &[], &ladders, &mut s, &cmd, &p, dt, now),
                LadderOutcome::Frozen => {}
            }
            now += dt as f64;
            if s.ladder.is_none() && s.forced.is_none() {
                left = true;
                break;
            }
        }
        assert!(s.origin[2] > z0 + 100.0, "climbed {s:?}");
        assert!(left, "left at the top");
        assert!((s.origin[0] - 80.0).abs() < 0.5, "at the dismount point {s:?}");
    }
}