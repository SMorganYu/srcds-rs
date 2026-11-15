//! Rigid-body motion for physics props (srcds-rs' own simple model, not VPhysics):
//! gravity, swept-box collision against the world and other solid entities, sliding
//! with friction and restitution, angular velocity damping, and sleeping when at rest.
//!
//! Boxes are the world-space AABB of the model's oriented bounds; rotation does not
//! change on contact. Enough for props that fall, land, slide and can be thrown.

use srcds_lua::entity::{EntData, EntityStore};

use crate::collision::{CollisionMap, MASK_PLAYERSOLID};

pub const SLEEP_SPEED: f32 = 2.0;
pub const SLEEP_TICKS: u32 = 20;
const RESTITUTION: f32 = 0.15;
const GROUND_FRICTION: f32 = 4.0;
const AIR_DRAG: f32 = 0.01;
const ANGULAR_DAMPING: f32 = 2.0;
/// Velocity limit like `sv_maxvelocity` / VPhysics' default speed cap.
const MAX_SPEED: f32 = 4000.0;

fn rotate(v: [f32; 3], a: [f32; 3]) -> [f32; 3] {
    srcds_lua::vecang::rotate(v, a)
}

/// World-space AABB of the entity's bounds relative to its origin.
pub fn world_box(e: &EntData) -> ([f32; 3], [f32; 3]) {
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for i in 0..8 {
        let c = [
            if i & 1 == 0 { e.mins[0] } else { e.maxs[0] },
            if i & 2 == 0 { e.mins[1] } else { e.maxs[1] },
            if i & 4 == 0 { e.mins[2] } else { e.maxs[2] },
        ];
        let w = rotate(c, e.ang);
        for k in 0..3 {
            lo[k] = lo[k].min(w[k]);
            hi[k] = hi[k].max(w[k]);
        }
    }
    if lo[0] > hi[0] {
        return ([0.0; 3], [0.0; 3]);
    }
    (lo, hi)
}

/// Per-entity rest counters live in the entity's NW table key `__srcds_rest`.
fn rest_ticks(e: &EntData) -> u32 {
    match e.nw.get("__srcds_rest") {
        Some(srcds_lua::entity::NetValue::Int(i)) => *i as u32,
        _ => 0,
    }
}

/// Advances every awake, movable physics prop by `dt` seconds.
pub fn step(store: &mut EntityStore, world: &CollisionMap, gravity: f32, dt: f32) {
    // solid boxes of other entities (props and players) at the start of the step
    let solids: Vec<(u32, [f32; 3], [f32; 3])> = store
        .ents
        .values()
        .filter(|e| e.r.id != 0 && e.spawned && !e.removing && e.solid != 0 && e.weapon_owner.is_null() && e.solid_flags & 4 == 0)
        .filter(|e| e.phys.is_some() || e.player.is_some())
        .map(|e| {
            let (lo, hi) = world_box(e);
            (e.r.id, [e.pos[0] + lo[0], e.pos[1] + lo[1], e.pos[2] + lo[2]], [e.pos[0] + hi[0], e.pos[1] + hi[1], e.pos[2] + hi[2]])
        })
        .collect();
    for e in store.ents.values_mut() {
        let Some(p) = e.phys.clone() else { continue };
        if !p.motion || p.asleep || p.held || !e.spawned || e.removing || !e.parent.is_null() || e.player.is_some() {
            continue;
        }
        if !e.weapon_owner.is_null() {
            continue;
        }
        let (lo, hi) = world_box(e);
        // gravity (half before, half after, like the engine's integrators)
        if p.gravity {
            e.vel[2] -= gravity * dt;
        }
        let drag = 1.0 - AIR_DRAG * dt * if p.drag { 1.0 } else { 0.0 };
        for k in 0..3 {
            e.vel[k] = (e.vel[k] * drag).clamp(-MAX_SPEED, MAX_SPEED);
        }
        let mut on_ground = false;
        let mut remaining = dt;
        for _ in 0..4 {
            if remaining <= 0.0 {
                break;
            }
            let end = [
                e.pos[0] + e.vel[0] * remaining,
                e.pos[1] + e.vel[1] * remaining,
                e.pos[2] + e.vel[2] * remaining,
            ];
            let mut t = if p.collisions {
                world.trace_box(e.pos, end, lo, hi, MASK_PLAYERSOLID)
            } else {
                crate::collision::Trace {
                    fraction: 1.0,
                    end,
                    normal: [0.0; 3],
                    start_solid: false,
                    all_solid: false,
                }
            };
            // other solids: swept AABB vs AABB (Minkowski box)
            if p.collisions {
                for (id, slo, shi) in &solids {
                    if *id == e.r.id {
                        continue;
                    }
                    let blo = [slo[0] - hi[0], slo[1] - hi[1], slo[2] - hi[2]];
                    let bhi = [shi[0] - lo[0], shi[1] - lo[1], shi[2] - lo[2]];
                    if let Some((f, n)) = sweep(e.pos, end, blo, bhi) {
                        if f < t.fraction {
                            t.fraction = f;
                            t.normal = n;
                            t.end = [
                                e.pos[0] + (end[0] - e.pos[0]) * f,
                                e.pos[1] + (end[1] - e.pos[1]) * f,
                                e.pos[2] + (end[2] - e.pos[2]) * f,
                            ];
                        }
                    }
                }
            }
            if t.start_solid && t.fraction == 0.0 {
                // stuck: push up out of the solid a little and stop
                e.pos[2] += 0.5;
                e.vel = [0.0; 3];
                break;
            }
            e.pos = t.end;
            if t.fraction >= 1.0 {
                break;
            }
            remaining *= 1.0 - t.fraction;
            let n = t.normal;
            let vn = e.vel[0] * n[0] + e.vel[1] * n[1] + e.vel[2] * n[2];
            if vn < 0.0 {
                let bounce = if -vn > 60.0 { RESTITUTION } else { 0.0 };
                for k in 0..3 {
                    e.vel[k] -= (1.0 + bounce) * vn * n[k];
                }
            }
            if n[2] > 0.7 {
                on_ground = true;
            }
            // nudge off the surface
            for k in 0..3 {
                e.pos[k] += n[k] * 0.03125;
            }
        }
        if on_ground {
            let speed = (e.vel[0] * e.vel[0] + e.vel[1] * e.vel[1]).sqrt();
            if speed > 0.0 {
                let drop = speed.max(10.0) * GROUND_FRICTION * dt;
                let s = ((speed - drop) / speed).max(0.0);
                e.vel[0] *= s;
                e.vel[1] *= s;
            }
        }
        if let Some(ph) = &mut e.phys {
            let damp = (1.0 - ANGULAR_DAMPING * dt).max(0.0);
            for k in 0..3 {
                ph.ang_vel[k] *= damp;
            }
            for k in 0..3 {
                e.ang[k] = srcds_lua::vecang::normalize_angle(e.ang[k] + ph.ang_vel[k] * dt);
            }
        }
        let speed2 = e.vel.iter().map(|v| v * v).sum::<f32>();
        let rest = if on_ground && speed2 < SLEEP_SPEED * SLEEP_SPEED { rest_ticks(e) + 1 } else { 0 };
        e.nw.insert("__srcds_rest".into(), srcds_lua::entity::NetValue::Int(rest as i64));
        if rest >= SLEEP_TICKS {
            e.vel = [0.0; 3];
            if let Some(ph) = &mut e.phys {
                ph.asleep = true;
                ph.ang_vel = [0.0; 3];
            }
        }
    }
}

// ---------------------------------------------------------------------------
// constraints (srcds-rs' own position-based solver, not VPhysics)

type M3 = [[f32; 3]; 3]; // columns

fn mat(a: [f32; 3]) -> M3 {
    let (f, r, u) = srcds_lua::vecang::angle_vectors(a);
    [f, [-r[0], -r[1], -r[2]], u]
}
fn mul_v(m: &M3, v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[1][0] * v[1] + m[2][0] * v[2],
        m[0][1] * v[0] + m[1][1] * v[1] + m[2][1] * v[2],
        m[0][2] * v[0] + m[1][2] * v[1] + m[2][2] * v[2],
    ]
}
fn mul_m(a: &M3, b: &M3) -> M3 {
    [mul_v(a, b[0]), mul_v(a, b[1]), mul_v(a, b[2])]
}
fn transpose(m: &M3) -> M3 {
    [[m[0][0], m[1][0], m[2][0]], [m[0][1], m[1][1], m[2][1]], [m[0][2], m[1][2], m[2][2]]]
}
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn angles_of(m: &M3) -> [f32; 3] {
    srcds_lua::entity::angles_from_basis(m[0], m[2])
}

/// Pose and mobility of a constrained body (`None` = the world).
fn body(store: &EntityStore, r: srcds_lua::entity::EntRef) -> Option<(&EntData, bool)> {
    if r.is_null() || r.id == 0 {
        return None;
    }
    let e = store.get(r)?;
    let movable = e.phys.as_ref().is_some_and(|p| p.motion && !p.held) && e.player.is_none();
    Some((e, movable))
}

/// Enforces all constraints (a few relaxation passes).
pub fn solve_constraints(store: &mut EntityStore, dt: f32) {
    use srcds_lua::entity::{Constraint, ConstraintKind};
    let cons: Vec<Constraint> = store.ents.values().filter(|e| !e.removing).filter_map(|e| e.constraint.clone()).collect();
    if cons.is_empty() {
        return;
    }
    for _ in 0..4 {
        for c in &cons {
            let pose = |s: &EntityStore, r| body(s, r).map_or(([0.0; 3], [0.0; 3], false, 1.0), |(e, m)| (e.pos, e.ang, m, e.phys.as_ref().map_or(1.0, |p| p.mass.max(0.1))));
            let (pa, aa, ma, mass_a) = pose(store, c.a);
            let (pb, ab, mb, mass_b) = pose(store, c.b);
            if !ma && !mb {
                continue;
            }
            match &c.kind {
                ConstraintKind::Weld { rel_pos, rel_fwd, rel_up } => {
                    let rel: M3 = [*rel_fwd, cross(*rel_up, *rel_fwd), *rel_up];
                    if mb {
                        // b follows a (a may be the world, a frozen or a held prop)
                        let m_a = mat(aa);
                        let m_b = mul_m(&m_a, &rel);
                        let new_pos = add(pa, mul_v(&m_a, *rel_pos));
                        let (va, a_moving) = store
                            .get(c.a)
                            .map_or(([0.0; 3], false), |e| (e.vel, e.phys.as_ref().is_some_and(|p| p.held || (p.motion && !p.asleep))));
                        if let Some(e) = store.get_mut(c.b) {
                            e.pos = new_pos;
                            e.ang = angles_of(&m_b);
                            e.vel = if a_moving { va } else { [0.0; 3] };
                            if let Some(p) = &mut e.phys {
                                p.ang_vel = [0.0; 3];
                                // held fixed by the world or a frozen prop: rest
                                p.asleep = !a_moving;
                            }
                        }
                    } else {
                        // a follows the pinned b
                        let m_b = mat(ab);
                        let m_a = mul_m(&m_b, &transpose(&rel));
                        let new_pos = sub(pb, mul_v(&m_a, *rel_pos));
                        let (vb, b_moving) = store
                            .get(c.b)
                            .map_or(([0.0; 3], false), |e| (e.vel, e.phys.as_ref().is_some_and(|p| p.held)));
                        if let Some(e) = store.get_mut(c.a) {
                            e.pos = new_pos;
                            e.ang = angles_of(&m_a);
                            e.vel = if b_moving { vb } else { [0.0; 3] };
                            if let Some(p) = &mut e.phys {
                                p.ang_vel = [0.0; 3];
                                p.asleep = !b_moving;
                            }
                        }
                    }
                }
                ConstraintKind::Length { .. } | ConstraintKind::Point | ConstraintKind::Spring { .. } => {
                    let wa = add(pa, mul_v(&mat(aa), c.local_a));
                    let wb = add(pb, mul_v(&mat(ab), c.local_b));
                    let d = sub(wb, wa);
                    let dist = dot(d, d).sqrt();
                    if dist < 1e-4 {
                        continue;
                    }
                    let n = [d[0] / dist, d[1] / dist, d[2] / dist];
                    let (sa, sb) = match (ma, mb) {
                        (true, true) => (mass_b / (mass_a + mass_b), mass_a / (mass_a + mass_b)),
                        (true, false) => (1.0, 0.0),
                        _ => (0.0, 1.0),
                    };
                    let va = store.get(c.a).map_or([0.0; 3], |e| e.vel);
                    let vb = store.get(c.b).map_or([0.0; 3], |e| e.vel);
                    let rel_v = dot(sub(vb, va), n);
                    let (dpos, dvel) = match &c.kind {
                        ConstraintKind::Length { length, rigid } => {
                            let err = dist - length;
                            if err <= 0.0 && !rigid {
                                continue;
                            }
                            (err, if err > 0.0 { rel_v.max(0.0) } else { rel_v })
                        }
                        ConstraintKind::Point => (dist, rel_v),
                        ConstraintKind::Spring { rest, constant, damping } => {
                            // force along the spring, applied as a velocity change (once per tick)
                            let f = (constant * (dist - rest) + damping * rel_v) * dt / 4.0;
                            (0.0, f / mass_a.min(mass_b).max(1.0))
                        }
                        _ => continue,
                    };
                    let wake = dpos.abs() > 0.05 || dvel.abs() > 1.0;
                    if let Some(e) = store.get_mut(c.a).filter(|_| ma) {
                        e.pos = add(e.pos, scale(n, dpos * sa));
                        e.vel = add(e.vel, scale(n, dvel * sa));
                        if wake {
                            if let Some(p) = &mut e.phys {
                                p.asleep = false;
                            }
                        }
                    }
                    if let Some(e) = store.get_mut(c.b).filter(|_| mb) {
                        e.pos = sub(e.pos, scale(n, dpos * sb));
                        e.vel = sub(e.vel, scale(n, dvel * sb));
                        if wake {
                            if let Some(p) = &mut e.phys {
                                p.asleep = false;
                            }
                        }
                    }
                }
                ConstraintKind::Other => {}
            }
        }
    }
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}

/// Swept point against an AABB; (fraction, normal) of the first contact.
fn sweep(start: [f32; 3], end: [f32; 3], lo: [f32; 3], hi: [f32; 3]) -> Option<(f32, [f32; 3])> {
    let d = [end[0] - start[0], end[1] - start[1], end[2] - start[2]];
    if (0..3).all(|k| start[k] > lo[k] && start[k] < hi[k]) {
        return None; // already overlapping: let them separate
    }
    let (mut t0, mut t1) = (0.0f32, 1.0f32);
    let mut normal = [0.0; 3];
    for k in 0..3 {
        if d[k].abs() < 1e-9 {
            if start[k] <= lo[k] || start[k] >= hi[k] {
                return None;
            }
            continue;
        }
        let (mut a, mut b) = ((lo[k] - start[k]) / d[k], (hi[k] - start[k]) / d[k]);
        let mut n = [0.0; 3];
        n[k] = -d[k].signum();
        if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        if a > t0 {
            t0 = a;
            normal = n;
        }
        t1 = t1.min(b);
        if t0 > t1 {
            return None;
        }
    }
    (t0 > 0.0 || normal != [0.0; 3]).then_some((t0, normal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use srcds_lua::entity::PhysData;

    #[test]
    fn prop_falls_lands_and_sleeps() {
        let world = crate::collision::tests::box_world([-512.0, -512.0, -512.0], [512.0, 512.0, 0.0]);
        let mut s = EntityStore::default();
        s.max_clients = 1;
        let r = s.create("prop_physics", None).unwrap();
        {
            let e = s.get_mut(r).unwrap();
            e.spawned = true;
            e.solid = 6;
            e.mins = [-10.0, -10.0, 0.0];
            e.maxs = [10.0, 10.0, 40.0];
            e.pos = [0.0, 0.0, 200.0];
            e.phys = Some(PhysData::default());
        }
        for _ in 0..400 {
            step(&mut s, &world, 600.0, 0.015);
        }
        let e = s.get(r).unwrap();
        assert!((e.pos[2] - 0.0).abs() < 1.0, "{:?}", e.pos);
        assert!(e.phys.as_ref().unwrap().asleep);
    }

    #[test]
    fn props_stack() {
        let world = crate::collision::tests::box_world([-512.0, -512.0, -512.0], [512.0, 512.0, 0.0]);
        let mut s = EntityStore::default();
        s.max_clients = 1;
        let mut ids = Vec::new();
        for z in [0.0, 100.0] {
            let r = s.create("prop_physics", None).unwrap();
            let e = s.get_mut(r).unwrap();
            e.spawned = true;
            e.solid = 6;
            e.mins = [-10.0, -10.0, 0.0];
            e.maxs = [10.0, 10.0, 20.0];
            e.pos = [0.0, 0.0, z + 1.0];
            e.phys = Some(PhysData::default());
            ids.push(r);
        }
        for _ in 0..400 {
            step(&mut s, &world, 600.0, 0.015);
        }
        let top = s.get(ids[1]).unwrap().pos[2];
        assert!((top - 20.0).abs() < 1.5, "top at {top}");
    }
}
