//! World collision from the BSP brush data: swept axis-aligned box traces.
//!
//! Uses the map's convex brushes (planes, brush sides incl. bevel planes, the node tree
//! and leaf→brush lists of the world model). A trace sweeps a box from `start` to
//! `end`; each brush is expanded by the box (Minkowski sum: every side plane is pushed
//! out by the box's support point) and the segment is clipped against it.
//!
//! Displacement surfaces (terrain) are handled by [`crate::displacement`].
//! Not covered yet: static props, brush entities other than the world model.

use std::io;

use crate::bsp;
use crate::displacement::Displacements;

const LUMP_PLANES: usize = 1;
const LUMP_NODES: usize = 5;
const LUMP_LEAFS: usize = 10;
const LUMP_MODELS: usize = 14;
const LUMP_LEAFBRUSHES: usize = 17;
const LUMP_BRUSHES: usize = 18;
const LUMP_BRUSHSIDES: usize = 19;

/// Contents bits that block player movement (solid, window, grate, moveable,
/// player clip, monster).
pub const MASK_PLAYERSOLID: i32 = 0x1 | 0x2 | 0x8 | 0x4000 | 0x10000 | 0x200_0000;

/// Distance kept from surfaces after a collision (1/32 unit).
pub const DIST_EPSILON: f32 = 0.031_25;

#[derive(Debug, Clone, Copy)]
struct Plane {
    n: [f32; 3],
    d: f32,
}

#[derive(Debug, Clone, Copy)]
struct Node {
    plane: u32,
    children: [i32; 2],
}

#[derive(Debug, Clone, Copy)]
struct Leaf {
    first_brush: u32,
    num_brushes: u32,
}

#[derive(Debug, Clone, Copy)]
struct Brush {
    first_side: u32,
    num_sides: u32,
    contents: i32,
}

/// Collision data of the world model.
#[derive(Debug, Clone, Default)]
pub struct CollisionMap {
    planes: Vec<Plane>,
    nodes: Vec<Node>,
    leafs: Vec<Leaf>,
    leaf_brushes: Vec<u16>,
    brushes: Vec<Brush>,
    /// Plane index of each brush side.
    sides: Vec<u32>,
    head_node: i32,
    /// Head node of every brush model (`*N` = index N; 0 is the world).
    model_heads: Vec<i32>,
    /// Displacement (terrain) triangles.
    disp: Displacements,
}

/// Result of a trace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trace {
    /// Fraction of the move completed (1 = no hit).
    pub fraction: f32,
    pub end: [f32; 3],
    /// Normal of the surface hit (zero when nothing was hit).
    pub normal: [f32; 3],
    /// The start position is inside a solid.
    pub start_solid: bool,
    /// The whole move is inside a solid.
    pub all_solid: bool,
}

fn rd_f32(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd_i32(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

impl CollisionMap {
    /// An empty world (nothing collides).
    pub fn empty() -> Self {
        Self {
            head_node: -1,
            ..Default::default()
        }
    }

    /// Reads the world model's collision data from a BSP image.
    pub fn parse(d: &[u8]) -> io::Result<Self> {
        let lump = |i| bsp::lump(d, i);
        let planes = lump(LUMP_PLANES)?
            .chunks_exact(20)
            .map(|c| Plane {
                n: [rd_f32(c, 0), rd_f32(c, 4), rd_f32(c, 8)],
                d: rd_f32(c, 12),
            })
            .collect::<Vec<_>>();
        let nodes = lump(LUMP_NODES)?
            .chunks_exact(32)
            .map(|c| Node {
                plane: rd_i32(c, 0) as u32,
                children: [rd_i32(c, 4), rd_i32(c, 8)],
            })
            .collect::<Vec<_>>();
        // Leaf records are 32 bytes from lump version 1 on (56 with the older
        // embedded ambient lighting); the brush list sits at offset 24 in both.
        let leaf_size = if bsp::lump_version(d, LUMP_LEAFS)? >= 1 {
            32
        } else {
            56
        };
        let leafs = lump(LUMP_LEAFS)?
            .chunks_exact(leaf_size)
            .map(|c| Leaf {
                first_brush: rd_u16(c, 24) as u32,
                num_brushes: rd_u16(c, 26) as u32,
            })
            .collect::<Vec<_>>();
        let leaf_brushes = lump(LUMP_LEAFBRUSHES)?
            .chunks_exact(2)
            .map(|c| rd_u16(c, 0))
            .collect::<Vec<_>>();
        let brushes = lump(LUMP_BRUSHES)?
            .chunks_exact(12)
            .map(|c| Brush {
                first_side: rd_i32(c, 0).max(0) as u32,
                num_sides: rd_i32(c, 4).max(0) as u32,
                contents: rd_i32(c, 8),
            })
            .collect::<Vec<_>>();
        let sides = lump(LUMP_BRUSHSIDES)?
            .chunks_exact(8)
            .map(|c| rd_u16(c, 0) as u32)
            .collect::<Vec<_>>();
        let models = lump(LUMP_MODELS)?;
        let head_node = if models.len() >= 48 {
            rd_i32(models, 36)
        } else {
            -1
        };
        // dmodel_t: mins, maxs, origin (3 × 12 bytes), head node, first face, faces
        let model_heads = models.chunks_exact(48).map(|c| rd_i32(c, 36)).collect();
        let m = Self {
            planes,
            nodes,
            leafs,
            leaf_brushes,
            brushes,
            sides,
            head_node,
            model_heads,
            disp: Displacements::parse(d)?,
        };
        m.validate()?;
        Ok(m)
    }

    /// Checks every index so traces never go out of bounds.
    fn validate(&self) -> io::Result<()> {
        let leaf_ok = |c: i32| {
            if c < 0 {
                ((-1 - c) as usize) < self.leafs.len()
            } else {
                (c as usize) < self.nodes.len()
            }
        };
        if !self.nodes.is_empty() && !leaf_ok(self.head_node) {
            return Err(bad("bad head node"));
        }
        for n in &self.nodes {
            if n.plane as usize >= self.planes.len() || !n.children.iter().all(|&c| leaf_ok(c)) {
                return Err(bad("bad BSP node"));
            }
        }
        for l in &self.leafs {
            if (l.first_brush + l.num_brushes) as usize > self.leaf_brushes.len() {
                return Err(bad("bad leaf brush range"));
            }
        }
        if self
            .leaf_brushes
            .iter()
            .any(|&b| b as usize >= self.brushes.len())
        {
            return Err(bad("bad leaf brush index"));
        }
        for b in &self.brushes {
            if (b.first_side + b.num_sides) as usize > self.sides.len() {
                return Err(bad("bad brush side range"));
            }
        }
        if self.sides.iter().any(|&p| p as usize >= self.planes.len()) {
            return Err(bad("bad brush side plane"));
        }
        Ok(())
    }

    /// Replaces the displacement set (tests and tools).
    pub fn with_displacements(mut self, disp: Displacements) -> Self {
        self.disp = disp;
        self
    }

    pub fn displacement_triangle_count(&self) -> usize {
        self.disp.triangle_count()
    }

    pub fn brush_count(&self) -> usize {
        self.brushes.len()
    }

    /// Leaves touched by the axis-aligned box `[lo, hi]`.
    fn leaves_in_box(&self, head: i32, lo: [f32; 3], hi: [f32; 3], out: &mut Vec<u32>) {
        let mut stack = vec![head];
        while let Some(i) = stack.pop() {
            if i < 0 {
                out.push((-1 - i) as u32);
                continue;
            }
            let node = self.nodes[i as usize];
            let p = self.planes[node.plane as usize];
            // distance range of the box from the plane
            let (mut near, mut far) = (-p.d, -p.d);
            for k in 0..3 {
                let (a, b) = (p.n[k] * lo[k], p.n[k] * hi[k]);
                near += a.min(b);
                far += a.max(b);
            }
            if near >= 0.0 {
                stack.push(node.children[0]);
            } else if far < 0.0 {
                stack.push(node.children[1]);
            } else {
                stack.push(node.children[0]);
                stack.push(node.children[1]);
            }
        }
    }

    /// Sweeps the box `[mins, maxs]` (relative to the moving point) from `start` to
    /// `end` against all brushes and displacements whose contents intersect `mask`.
    pub fn trace_box(
        &self,
        start: [f32; 3],
        end: [f32; 3],
        mins: [f32; 3],
        maxs: [f32; 3],
        mask: i32,
    ) -> Trace {
        let mut t = Trace {
            fraction: 1.0,
            end,
            normal: [0.0; 3],
            start_solid: false,
            all_solid: false,
        };
        if !self.nodes.is_empty() {
            self.trace_brushes(self.head_node, start, end, mins, maxs, mask, &mut t);
        }
        if !t.all_solid {
            if let Some(h) = self.disp.sweep(start, end, mins, maxs, mask, t.fraction) {
                t.fraction = h.fraction;
                t.normal = h.normal;
            }
        }
        for k in 0..3 {
            t.end[k] = start[k] + (end[k] - start[k]) * t.fraction;
        }
        t
    }

    /// Sweeps a box against brush model `*model` placed at `origin` (no rotation).
    pub fn trace_model(
        &self,
        model: usize,
        origin: [f32; 3],
        start: [f32; 3],
        end: [f32; 3],
        mins: [f32; 3],
        maxs: [f32; 3],
        mask: i32,
    ) -> Trace {
        let mut t = Trace {
            fraction: 1.0,
            end,
            normal: [0.0; 3],
            start_solid: false,
            all_solid: false,
        };
        let Some(&head) = self.model_heads.get(model).filter(|_| model > 0) else { return t };
        if self.nodes.is_empty() || head < 0 || head as usize >= self.nodes.len() {
            return t;
        }
        let s = [start[0] - origin[0], start[1] - origin[1], start[2] - origin[2]];
        let e = [end[0] - origin[0], end[1] - origin[1], end[2] - origin[2]];
        self.trace_brushes(head, s, e, mins, maxs, mask, &mut t);
        for k in 0..3 {
            t.end[k] = start[k] + (end[k] - start[k]) * t.fraction;
        }
        t
    }

    #[allow(clippy::too_many_arguments)]
    fn trace_brushes(
        &self,
        head: i32,
        start: [f32; 3],
        end: [f32; 3],
        mins: [f32; 3],
        maxs: [f32; 3],
        mask: i32,
        t: &mut Trace,
    ) {
        let mut lo = [0.0f32; 3];
        let mut hi = [0.0f32; 3];
        for k in 0..3 {
            lo[k] = start[k].min(end[k]) + mins[k] - 1.0;
            hi[k] = start[k].max(end[k]) + maxs[k] + 1.0;
        }
        let mut leaves = Vec::new();
        self.leaves_in_box(head, lo, hi, &mut leaves);
        let mut seen = vec![0u64; self.brushes.len().div_ceil(64)];
        for leaf in leaves {
            let l = self.leafs[leaf as usize];
            for k in l.first_brush..l.first_brush + l.num_brushes {
                let b = self.leaf_brushes[k as usize] as usize;
                let (w, bit) = (b / 64, 1u64 << (b % 64));
                if seen[w] & bit != 0 {
                    continue;
                }
                seen[w] |= bit;
                let brush = self.brushes[b];
                if brush.contents & mask == 0 || brush.num_sides == 0 {
                    continue;
                }
                self.clip_to_brush(&brush, start, end, mins, maxs, t);
                if t.all_solid {
                    return;
                }
            }
        }
    }

    fn clip_to_brush(
        &self,
        brush: &Brush,
        start: [f32; 3],
        end: [f32; 3],
        mins: [f32; 3],
        maxs: [f32; 3],
        t: &mut Trace,
    ) {
        let planes = (brush.first_side..brush.first_side + brush.num_sides).map(|s| {
            let p = self.planes[self.sides[s as usize] as usize];
            (p.n, p.d)
        });
        clip_box_to_planes(planes, start, end, mins, maxs, t);
    }
}

/// Clips a box sweep against one convex solid given by its planes (normal, distance);
/// updates `t` (fraction, normal, start/all solid) when it hits earlier.
pub fn clip_box_to_planes(
    planes: impl Iterator<Item = ([f32; 3], f32)>,
    start: [f32; 3],
    end: [f32; 3],
    mins: [f32; 3],
    maxs: [f32; 3],
    t: &mut Trace,
) {
    let mut enter = -1.0f32;
    let mut leave = 1.0f32;
    let mut hit_normal = [0.0f32; 3];
    let (mut starts_out, mut ends_out) = (false, false);
    for (n, d) in planes {
        // push the plane out by the box corner that leads into it
        let mut corner = [0.0f32; 3];
        for k in 0..3 {
            corner[k] = if n[k] < 0.0 { maxs[k] } else { mins[k] };
        }
        let dist = d - dot(corner, n);
        let d1 = dot(start, n) - dist;
        let d2 = dot(end, n) - dist;
        if d2 > 0.0 {
            ends_out = true;
        }
        if d1 > 0.0 {
            starts_out = true;
        }
        // entirely in front of this side: no contact with the solid
        if d1 > 0.0 && (d2 >= DIST_EPSILON || d2 >= d1) {
            return;
        }
        if d1 <= 0.0 && d2 <= 0.0 {
            continue;
        }
        if d1 > d2 {
            // entering
            let f = ((d1 - DIST_EPSILON) / (d1 - d2)).max(0.0);
            if f > enter {
                enter = f;
                hit_normal = n;
            }
        } else {
            // leaving
            let f = ((d1 + DIST_EPSILON) / (d1 - d2)).min(1.0);
            if f < leave {
                leave = f;
            }
        }
    }
    if !starts_out {
        t.start_solid = true;
        if !ends_out {
            t.all_solid = true;
            t.fraction = 0.0;
        }
        return;
    }
    if enter < leave && enter > -1.0 && enter < t.fraction {
        t.fraction = enter.max(0.0);
        t.normal = hit_normal;
    }
}

/// Planes of an oriented box (`mins`/`maxs` in local space at `origin` with the given
/// axes forward/left/up) plus the six axial bevels of its world bounds, so an
/// axis-aligned hull sweeps against it exactly at the corners.
pub fn oriented_box_planes(origin: [f32; 3], axes: [[f32; 3]; 3], mins: [f32; 3], maxs: [f32; 3]) -> Vec<([f32; 3], f32)> {
    let mut out = Vec::with_capacity(12);
    for k in 0..3 {
        let a = axes[k];
        let c = dot(a, origin);
        out.push((a, c + maxs[k]));
        out.push(([-a[0], -a[1], -a[2]], -(c + mins[k])));
    }
    // world bounds of the eight corners
    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
    for i in 0..8 {
        let l = [
            if i & 1 == 0 { mins[0] } else { maxs[0] },
            if i & 2 == 0 { mins[1] } else { maxs[1] },
            if i & 4 == 0 { mins[2] } else { maxs[2] },
        ];
        for k in 0..3 {
            let w = origin[k] + axes[0][k] * l[0] + axes[1][k] * l[1] + axes[2][k] * l[2];
            lo[k] = lo[k].min(w);
            hi[k] = hi[k].max(w);
        }
    }
    for k in 0..3 {
        let mut n = [0.0; 3];
        n[k] = 1.0;
        out.push((n, hi[k]));
        n[k] = -1.0;
        out.push((n, -lo[k]));
    }
    out
}
#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A world consisting of one axis-aligned box brush `[lo, hi]` (6 planes, a single
    /// node splitting nothing useful: both children lead to the one leaf).
    pub fn box_world(lo: [f32; 3], hi: [f32; 3]) -> CollisionMap {
        let mut planes = Vec::new();
        for k in 0..3 {
            let mut n = [0.0; 3];
            n[k] = 1.0;
            planes.push(Plane { n, d: hi[k] });
            let mut n = [0.0; 3];
            n[k] = -1.0;
            planes.push(Plane { n, d: -lo[k] });
        }
        CollisionMap {
            planes,
            nodes: vec![Node {
                plane: 0,
                children: [-1, -1],
            }],
            leafs: vec![Leaf {
                first_brush: 0,
                num_brushes: 1,
            }],
            leaf_brushes: vec![0],
            brushes: vec![Brush {
                first_side: 0,
                num_sides: 6,
                contents: 1,
            }],
            sides: (0..6).collect(),
            head_node: 0,
            model_heads: vec![0],
            disp: Displacements::default(),
        }
    }

    /// Union of two test worlds: brushes of both in a single leaf.
    pub fn merge(a: &CollisionMap, b: &CollisionMap) -> CollisionMap {
        let mut m = a.clone();
        let po = m.planes.len() as u32;
        let so = m.sides.len() as u32;
        let bo = m.brushes.len() as u16;
        m.planes.extend_from_slice(&b.planes);
        m.sides.extend(b.sides.iter().map(|s| s + po));
        m.brushes.extend(b.brushes.iter().map(|br| Brush {
            first_side: br.first_side + so,
            ..*br
        }));
        m.leaf_brushes = (0..m.brushes.len() as u16).collect();
        let _ = bo;
        m.leafs = vec![Leaf {
            first_brush: 0,
            num_brushes: m.brushes.len() as u32,
        }];
        m
    }

    const HULL_MIN: [f32; 3] = [-16.0, -16.0, 0.0];
    const HULL_MAX: [f32; 3] = [16.0, 16.0, 72.0];

    #[test]
    fn falling_box_lands_on_floor() {
        // floor slab: top at z = 0
        let w = box_world([-512.0, -512.0, -64.0], [512.0, 512.0, 0.0]);
        let t = w.trace_box(
            [0.0, 0.0, 100.0],
            [0.0, 0.0, -100.0],
            HULL_MIN,
            HULL_MAX,
            MASK_PLAYERSOLID,
        );
        assert!(t.fraction < 1.0);
        assert_eq!(t.normal, [0.0, 0.0, 1.0]);
        assert!(t.end[2] > 0.0 && t.end[2] < 0.1, "{:?}", t.end);
        assert!(!t.start_solid);
    }

    #[test]
    fn misses_and_solid_starts() {
        let w = box_world([-512.0, -512.0, -64.0], [512.0, 512.0, 0.0]);
        // moving sideways above the floor: no hit
        let t = w.trace_box(
            [0.0, 0.0, 10.0],
            [300.0, 0.0, 10.0],
            HULL_MIN,
            HULL_MAX,
            MASK_PLAYERSOLID,
        );
        assert_eq!(t.fraction, 1.0);
        // starting inside
        let t = w.trace_box(
            [0.0, 0.0, -30.0],
            [0.0, 0.0, -40.0],
            HULL_MIN,
            HULL_MAX,
            MASK_PLAYERSOLID,
        );
        assert!(t.start_solid && t.all_solid);
        // mask filters the brush
        let t = w.trace_box(
            [0.0, 0.0, 100.0],
            [0.0, 0.0, -100.0],
            HULL_MIN,
            HULL_MAX,
            0x8000_0000u32 as i32,
        );
        assert_eq!(t.fraction, 1.0);
    }

    #[test]
    fn wall_stops_horizontal_move() {
        // wall occupying x in [100, 116]
        let w = box_world([100.0, -512.0, -512.0], [116.0, 512.0, 512.0]);
        let t = w.trace_box(
            [0.0, 0.0, 0.0],
            [200.0, 0.0, 0.0],
            HULL_MIN,
            HULL_MAX,
            MASK_PLAYERSOLID,
        );
        assert_eq!(t.normal, [-1.0, 0.0, 0.0]);
        // the hull's +x face (16) stops just before x = 100
        assert!((t.end[0] - 84.0).abs() < 0.1, "{:?}", t.end);
    }
}
