//! Displacement (terrain) collision: triangle meshes built from the BSP displacement
//! lumps and swept-box tests against them.
//!
//! Geometry: each displacement replaces a 4-sided map face. Its base points are
//! interpolated over a `(2^power + 1)²` grid starting at the corner nearest
//! `startPosition`, each grid point is offset by its displacement vector × distance,
//! and every grid cell becomes two triangles whose shared diagonal alternates with the
//! parity of the cell's first vertex index (public Source SDK 2013 layout).
//!
//! Collision follows the engine's model for displacements, which differs from brushes:
//! a triangle is one-sided — the solid is the region behind its face plane, bounded by
//! the triangle's axis-aligned extent and by the nine "edge × axis" planes. A swept box
//! hits it when the motion goes against the face normal and the entry fraction
//! (with the 1/32 unit skin) is not after the exit fraction.

use std::io;

use crate::bsp;

const LUMP_PLANES: usize = 1;
const LUMP_VERTEXES: usize = 3;
const LUMP_FACES: usize = 7;
const LUMP_EDGES: usize = 12;
const LUMP_SURFEDGES: usize = 13;
const LUMP_DISPINFO: usize = 26;
const LUMP_DISP_VERTS: usize = 33;

const DISPINFO_SIZE: usize = 176;
const DISP_VERT_SIZE: usize = 20;
const FACE_SIZE: usize = 56;
/// `minTess` with the high bit set carries collision flags; this one disables hull
/// (player box) collision.
const FLAG_NO_HULL_COLLISION: i32 = 0x4;
const SKIN: f32 = 1.0 / 32.0;

type V3 = [f32; 3];

fn dot(a: V3, b: V3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn length(a: V3) -> f32 {
    dot(a, a).sqrt()
}

fn lerp(a: V3, b: V3, t: f32) -> V3 {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

#[derive(Debug, Clone, Copy)]
struct Plane {
    n: V3,
    d: f32,
}

/// One collision triangle with its precomputed separating planes.
#[derive(Debug, Clone)]
struct Tri {
    mins: V3,
    maxs: V3,
    /// Face plane (normal points out of the solid side).
    face: Plane,
    /// Axis-aligned bounds planes (6) followed by the valid edge × axis planes.
    sides: Vec<Plane>,
}

/// All displacement triangles of a map, grouped per displacement for culling.
#[derive(Debug, Clone, Default)]
pub struct Displacements {
    groups: Vec<(V3, V3, Vec<Tri>)>,
    contents: Vec<i32>,
}

/// Outcome of a sweep against displacements.
#[derive(Debug, Clone, Copy)]
pub struct DispHit {
    pub fraction: f32,
    pub normal: V3,
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
fn rd_v3(b: &[u8], o: usize) -> V3 {
    [rd_f32(b, o), rd_f32(b, o + 4), rd_f32(b, o + 8)]
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl Tri {
    fn new(p: [V3; 3], up_hint: V3) -> Option<Self> {
        let mut n = cross(sub(p[1], p[0]), sub(p[2], p[0]));
        let l = length(n);
        if l < 1e-6 {
            return None;
        }
        n = [n[0] / l, n[1] / l, n[2] / l];
        if dot(n, up_hint) < 0.0 {
            n = [-n[0], -n[1], -n[2]];
        }
        let face = Plane { n, d: dot(n, p[0]) };
        let mut mins = p[0];
        let mut maxs = p[0];
        for v in &p[1..] {
            for k in 0..3 {
                mins[k] = mins[k].min(v[k]);
                maxs[k] = maxs[k].max(v[k]);
            }
        }
        let mut sides = Vec::with_capacity(15);
        for k in 0..3 {
            let mut lo = [0.0; 3];
            lo[k] = -1.0;
            sides.push(Plane { n: lo, d: -mins[k] });
            let mut hi = [0.0; 3];
            hi[k] = 1.0;
            sides.push(Plane { n: hi, d: maxs[k] });
        }
        for axis in 0..3 {
            for e in 0..3 {
                let on = p[e];
                let next = p[(e + 1) % 3];
                let off = p[(e + 2) % 3];
                let mut ax = [0.0; 3];
                ax[axis] = 1.0;
                let mut en = cross(sub(next, on), ax);
                let l = length(en);
                if l < 1e-6 {
                    continue;
                }
                en = [en[0] / l, en[1] / l, en[2] / l];
                let (a1, a2) = ((axis + 1) % 3, (axis + 2) % 3);
                if en[a1] == 0.0 || en[a2] == 0.0 {
                    continue;
                }
                let mut d = dot(en, on);
                // the triangle must lie behind the plane
                let off_d = dot(en, off);
                if (off_d - d).abs() >= SKIN && off_d > d {
                    en = [-en[0], -en[1], -en[2]];
                    d = -d;
                }
                sides.push(Plane { n: en, d });
            }
        }
        Some(Self {
            mins,
            maxs,
            face,
            sides,
        })
    }

    /// Sweeps a box (`center` moving by `delta`, half-size `ext`); returns the entry
    /// fraction and normal if it hits before `best`.
    fn sweep(&self, center: V3, delta: V3, ext: V3, best: f32) -> Option<(f32, V3)> {
        if dot(self.face.n, delta) > SKIN {
            return None; // moving away from the surface
        }
        let mut enter = f32::MIN;
        let mut enter_n = [0.0; 3];
        let mut leave = 1.0f32;
        let end = [
            center[0] + delta[0],
            center[1] + delta[1],
            center[2] + delta[2],
        ];
        let mut clip = |pl: &Plane| -> bool {
            // push the plane out by the box's support in its direction
            let expand =
                pl.d + pl.n[0].abs() * ext[0] + pl.n[1].abs() * ext[1] + pl.n[2].abs() * ext[2];
            let s = dot(pl.n, center) - expand;
            let e = dot(pl.n, end) - expand;
            if s > 0.0 && e > 0.0 {
                return false;
            }
            if s < 0.0 && e < 0.0 {
                return true;
            }
            let den = s - e;
            if s >= 0.0 && e <= 0.0 {
                let t = if den != 0.0 { (s - SKIN) / den } else { 0.0 };
                if t > enter {
                    enter = t;
                    enter_n = pl.n;
                }
            } else {
                let t = if den != 0.0 { (s + SKIN) / den } else { 0.0 };
                if t < leave {
                    leave = t;
                }
            }
            true
        };
        for pl in &self.sides {
            if !clip(pl) {
                return None;
            }
        }
        if !clip(&self.face) {
            return None;
        }
        let valid = enter != f32::MIN;
        if valid && (enter < leave || (enter - leave).abs() < 0.001) && enter < best {
            Some((enter.max(0.0), enter_n))
        } else {
            None
        }
    }
}

impl Displacements {
    /// Builds all displacement collision triangles of a BSP image.
    pub fn parse(d: &[u8]) -> io::Result<Self> {
        let infos = bsp::lump(d, LUMP_DISPINFO)?;
        let verts = bsp::lump(d, LUMP_DISP_VERTS)?;
        let faces = bsp::lump(d, LUMP_FACES)?;
        let vertexes = bsp::lump(d, LUMP_VERTEXES)?;
        let edges = bsp::lump(d, LUMP_EDGES)?;
        let surfedges = bsp::lump(d, LUMP_SURFEDGES)?;
        let planes = bsp::lump(d, LUMP_PLANES)?;
        let mut out = Self::default();
        for info in infos.chunks_exact(DISPINFO_SIZE) {
            let start = rd_v3(info, 0);
            let vert_start = rd_i32(info, 12);
            let power = rd_i32(info, 20);
            let min_tess = rd_i32(info, 24);
            let contents = rd_i32(info, 32);
            let face_idx = rd_u16(info, 36) as usize;
            if min_tess < 0 && (min_tess & 0x7FFF_FFFF) & FLAG_NO_HULL_COLLISION != 0 {
                continue;
            }
            if !(2..=4).contains(&power) || vert_start < 0 {
                return Err(bad("bad displacement power / vertex start"));
            }
            let face = faces
                .get(face_idx * FACE_SIZE..(face_idx + 1) * FACE_SIZE)
                .ok_or_else(|| bad("displacement face out of range"))?;
            let first_edge = rd_i32(face, 4);
            let num_edges = rd_u16(face, 8) as i16;
            if num_edges != 4 || first_edge < 0 {
                continue;
            }
            let mut corners = [[0.0f32; 3]; 4];
            for (k, c) in corners.iter_mut().enumerate() {
                let se_off = (first_edge as usize + k) * 4;
                let se = rd_i32(
                    surfedges
                        .get(se_off..se_off + 4)
                        .ok_or_else(|| bad("surfedge out of range"))?,
                    0,
                );
                let (edge, second) = if se >= 0 {
                    (se as usize, false)
                } else {
                    (se.unsigned_abs() as usize, true)
                };
                let e = edges
                    .get(edge * 4..edge * 4 + 4)
                    .ok_or_else(|| bad("edge out of range"))?;
                let vi = rd_u16(e, if second { 2 } else { 0 }) as usize;
                *c = rd_v3(
                    vertexes
                        .get(vi * 12..vi * 12 + 12)
                        .ok_or_else(|| bad("vertex out of range"))?,
                    0,
                );
            }
            // rotate so the corner nearest the start position comes first
            let first = (0..4)
                .min_by(|&a, &b| {
                    let da = length(sub(corners[a], start));
                    let db = length(sub(corners[b], start));
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or(0);
            corners.rotate_left(first);
            let size = (1usize << power) + 1;
            let n_verts = size * size;
            let vs = verts
                .get(
                    vert_start as usize * DISP_VERT_SIZE
                        ..(vert_start as usize + n_verts) * DISP_VERT_SIZE,
                )
                .ok_or_else(|| bad("displacement vertices out of range"))?;
            let mut grid = Vec::with_capacity(n_verts);
            for i in 0..size {
                let t = i as f32 / (size - 1) as f32;
                let a = lerp(corners[0], corners[1], t);
                let b = lerp(corners[3], corners[2], t);
                for j in 0..size {
                    let base = lerp(a, b, j as f32 / (size - 1) as f32);
                    let dv = &vs[(i * size + j) * DISP_VERT_SIZE..];
                    let dir = rd_v3(dv, 0);
                    let dist = rd_f32(dv, 12);
                    grid.push([
                        base[0] + dir[0] * dist,
                        base[1] + dir[1] * dist,
                        base[2] + dir[2] * dist,
                    ]);
                }
            }
            // orientation hint: the base face's plane normal (points to the open side;
            // `side` set means the face uses the plane flipped)
            let plane_num = rd_u16(face, 0) as usize;
            let side = face[2];
            let mut up = planes
                .get(plane_num * 20..plane_num * 20 + 12)
                .map(|b| rd_v3(b, 0))
                .unwrap_or([0.0, 0.0, 1.0]);
            if side != 0 {
                up = [-up[0], -up[1], -up[2]];
            }
            out.add_grid(&grid, size, up, contents);
        }
        Ok(out)
    }

    fn add_grid(&mut self, grid: &[V3], size: usize, up: V3, contents: i32) {
        let mut tris = Vec::with_capacity((size - 1) * (size - 1) * 2);
        for i in 0..size - 1 {
            for j in 0..size - 1 {
                let v = i * size + j;
                let idx = if v % 2 == 1 {
                    [[v, v + size, v + 1], [v + 1, v + size, v + size + 1]]
                } else {
                    [[v, v + size, v + size + 1], [v, v + size + 1, v + 1]]
                };
                for t in idx {
                    if let Some(tri) = Tri::new([grid[t[0]], grid[t[1]], grid[t[2]]], up) {
                        tris.push(tri);
                    }
                }
            }
        }
        if tris.is_empty() {
            return;
        }
        let mut mins = tris[0].mins;
        let mut maxs = tris[0].maxs;
        for t in &tris {
            for k in 0..3 {
                mins[k] = mins[k].min(t.mins[k]);
                maxs[k] = maxs[k].max(t.maxs[k]);
            }
        }
        self.groups.push((mins, maxs, tris));
        self.contents.push(contents);
    }

    /// A single flat-or-sloped test patch (for unit tests): `size`² grid with heights.
    pub fn from_heights(origin: V3, cell: f32, size: usize, heights: &[f32]) -> Self {
        let mut grid = Vec::with_capacity(size * size);
        for i in 0..size {
            for j in 0..size {
                grid.push([
                    origin[0] + i as f32 * cell,
                    origin[1] + j as f32 * cell,
                    origin[2] + heights[i * size + j],
                ]);
            }
        }
        let mut d = Self::default();
        d.add_grid(&grid, size, [0.0, 0.0, 1.0], 1);
        d
    }

    pub fn triangle_count(&self) -> usize {
        self.groups.iter().map(|g| g.2.len()).sum()
    }

    /// Sweeps the box `[mins, maxs]` from `start` to `end`; returns the earliest hit
    /// before `best` (a fraction from other collision already found).
    pub fn sweep(
        &self,
        start: V3,
        end: V3,
        mins: V3,
        maxs: V3,
        mask: i32,
        best: f32,
    ) -> Option<DispHit> {
        let mut c = [0.0f32; 3];
        let mut ext = [0.0f32; 3];
        let mut lo = [0.0f32; 3];
        let mut hi = [0.0f32; 3];
        for k in 0..3 {
            let off = (mins[k] + maxs[k]) * 0.5;
            ext[k] = (maxs[k] - mins[k]) * 0.5 + SKIN;
            c[k] = start[k] + off;
            lo[k] = start[k].min(end[k]) + mins[k] - 1.0;
            hi[k] = start[k].max(end[k]) + maxs[k] + 1.0;
        }
        let delta = sub(end, start);
        let ext_exact = [ext[0] - SKIN, ext[1] - SKIN, ext[2] - SKIN];
        let mut best = best;
        let mut hit = None;
        let overlaps = |a: V3, b: V3| (0..3).all(|k| a[k] <= hi[k] && b[k] >= lo[k]);
        for (g, (gmin, gmax, tris)) in self.groups.iter().enumerate() {
            if self.contents[g] & mask == 0 || !overlaps(*gmin, *gmax) {
                continue;
            }
            for t in tris {
                if !overlaps(t.mins, t.maxs) {
                    continue;
                }
                if let Some((f, n)) = t.sweep(c, delta, ext_exact, best) {
                    best = f;
                    hit = Some(DispHit {
                        fraction: f,
                        normal: n,
                    });
                }
            }
        }
        hit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HMIN: V3 = [-16.0, -16.0, 0.0];
    const HMAX: V3 = [16.0, 16.0, 72.0];

    fn flat() -> Displacements {
        Displacements::from_heights([-512.0, -512.0, 0.0], 64.0, 17, &[0.0; 17 * 17])
    }

    #[test]
    fn box_lands_on_flat_terrain() {
        let d = flat();
        assert_eq!(d.triangle_count(), 16 * 16 * 2);
        let h = d
            .sweep([0.0, 0.0, 100.0], [0.0, 0.0, -100.0], HMIN, HMAX, 1, 1.0)
            .unwrap();
        let z = 100.0 - 200.0 * h.fraction;
        assert!(z > 0.0 && z < 0.1, "{z}");
        assert!((h.normal[2] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn one_sided_and_moving_away() {
        let d = flat();
        // moving up from above: no hit
        assert!(d
            .sweep([0.0, 0.0, 10.0], [0.0, 0.0, 200.0], HMIN, HMAX, 1, 1.0)
            .is_none());
        // horizontal above the surface: no hit
        assert!(d
            .sweep([0.0, 0.0, 1.0], [300.0, 0.0, 1.0], HMIN, HMAX, 1, 1.0)
            .is_none());
        // masked out
        assert!(d
            .sweep([0.0, 0.0, 100.0], [0.0, 0.0, -100.0], HMIN, HMAX, 0, 1.0)
            .is_none());
    }

    /// Real map check, run with `SRCDS_RS_TEST_MAP=<path to a .bsp>`: every
    /// displacement must stop a box dropped onto its middle.
    #[test]
    fn real_map_terrain_is_solid() {
        let Ok(path) = std::env::var("SRCDS_RS_TEST_MAP") else {
            return;
        };
        let data = std::fs::read(path).unwrap();
        let d = Displacements::parse(&data).unwrap();
        assert!(d.triangle_count() > 0);
        let mut hits = 0;
        for (mins, maxs, tris) in &d.groups {
            // drop onto the centre of a triangle in the middle of the patch
            let t = &tris[tris.len() / 2];
            let cx = (t.mins[0] + t.maxs[0]) * 0.5;
            let cy = (t.mins[1] + t.maxs[1]) * 0.5;
            let top = maxs[2] + 200.0;
            let bottom = mins[2] - 200.0;
            if let Some(h) = d.sweep([cx, cy, top], [cx, cy, bottom], [-1.0; 3], [1.0; 3], 1, 1.0) {
                let z = top + (bottom - top) * h.fraction;
                assert!(
                    z >= mins[2] - 2.0 && z <= maxs[2] + 2.0,
                    "{z} not in {mins:?}..{maxs:?}"
                );
                if hits < 6 {
                    eprintln!("surface at {cx:.1} {cy:.1} {z:.2} normal {:?}", h.normal);
                }
                hits += 1;
            }
        }
        eprintln!(
            "{} displacements, {} triangles, {hits} hit",
            d.groups.len(),
            d.triangle_count()
        );
        assert_eq!(hits, d.groups.len());
    }

    #[test]
    fn slope_is_hit_from_the_side() {
        // 45° ramp rising along +x: h = x
        let size = 9;
        let mut h = vec![0.0; size * size];
        for i in 0..size {
            for j in 0..size {
                h[i * size + j] = i as f32 * 32.0;
            }
        }
        let d = Displacements::from_heights([0.0, -128.0, 0.0], 32.0, size, &h);
        // walking into the ramp at ground level stops at its foot
        let hit = d
            .sweep([-64.0, 0.0, 1.0], [64.0, 0.0, 1.0], HMIN, HMAX, 1, 1.0)
            .unwrap();
        let x = -64.0 + 128.0 * hit.fraction;
        assert!(x < 0.0 && x > -20.0, "{x}");
        assert!(
            hit.normal[0] < -0.5 && hit.normal[2] > 0.5,
            "{:?}",
            hit.normal
        );
    }
}
