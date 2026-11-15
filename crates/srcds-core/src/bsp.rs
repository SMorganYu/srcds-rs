//! Minimal BSP (VBSP) reader: what the server needs to describe a map to clients.
//!
//! * Map MD5 as checked by clients: MD5 over every lump's bytes in lump order,
//!   skipping the entity lump (verified: gm_construct в†’ `3b5c56cfвЂ¦`, identical to the
//!   value the reference server sends in `svc_ServerInfo`).
//! * Number of brush models (lump 14, 48-byte `dmodel_t`), which become the `*N`
//!   entries of `modelprecache`.
//! * `skyname` of `worldspawn` from the entity lump.
//! * Player spawn points (`info_player_start` and friends) and the world model bounds.

use std::io;
use std::path::Path;

use md5::{Digest, Md5};

const HEADER_LUMPS: usize = 64;
const LUMP_ENTITIES: usize = 0;
const LUMP_MODELS: usize = 14;
const DMODEL_SIZE: usize = 48;
const HEADER_LEN: usize = 8 + HEADER_LUMPS * 16 + 4;

#[derive(Debug, Clone, PartialEq)]
pub struct MapInfo {
    pub name: String,
    pub md5: [u8; 16],
    /// Number of brush models including the world (model 0).
    pub brush_models: usize,
    pub sky_name: String,
    /// Bounds of the world model (model 0), zero if the map has no models.
    pub world_mins: [f32; 3],
    pub world_maxs: [f32; 3],
    /// Player spawn points in map order.
    pub spawn_points: Vec<SpawnPoint>,
    /// Key/value blocks of the entity lump, in map order (worldspawn first).
    pub entities: Vec<Vec<(String, String)>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpawnPoint {
    pub origin: [f32; 3],
    /// View yaw in degrees.
    pub yaw: f32,
}

/// Entity classes treated as player spawn points, in order of preference.
const SPAWN_CLASSES: &[&str] = &[
    "info_player_start",
    "info_player_deathmatch",
    "info_player_combine",
    "info_player_rebel",
    "info_player_counterterrorist",
    "info_player_terrorist",
    "gmod_player_start",
];

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

fn check_header(d: &[u8]) -> io::Result<()> {
    if d.len() < HEADER_LEN || &d[..4] != b"VBSP" {
        return Err(bad("not a VBSP file"));
    }
    Ok(())
}

/// Bytes of lump `i` of a BSP image (bounds-checked).
pub fn lump(d: &[u8], i: usize) -> io::Result<&[u8]> {
    check_header(d)?;
    if i >= HEADER_LUMPS {
        return Err(bad("lump index out of range"));
    }
    let o = 8 + i * 16;
    let ofs = i32::from_le_bytes(d[o..o + 4].try_into().unwrap());
    let len = i32::from_le_bytes(d[o + 4..o + 8].try_into().unwrap());
    if ofs < 0 || len < 0 {
        return Err(bad("negative lump bounds"));
    }
    let (ofs, len) = (ofs as usize, len as usize);
    d.get(ofs..ofs.checked_add(len).ok_or_else(|| bad("lump overflow"))?)
        .ok_or_else(|| bad("lump outside file"))
}

/// Format version of lump `i`.
pub fn lump_version(d: &[u8], i: usize) -> io::Result<i32> {
    check_header(d)?;
    if i >= HEADER_LUMPS {
        return Err(bad("lump index out of range"));
    }
    let o = 8 + i * 16 + 8;
    Ok(i32::from_le_bytes(d[o..o + 4].try_into().unwrap()))
}

/// Map metadata and world collision, reading the file once.
pub fn load_with_collision(
    game_dir: &Path,
    map: &str,
) -> io::Result<(MapInfo, crate::collision::CollisionMap)> {
    let data = read_map(game_dir, map)?;
    Ok((
        parse(map, &data)?,
        crate::collision::CollisionMap::parse(&data)?,
    ))
}

fn read_map(game_dir: &Path, map: &str) -> io::Result<Vec<u8>> {
    if map.contains(['/', '\\', '.']) {
        return Err(bad("map name must not contain path separators or dots"));
    }
    std::fs::read(game_dir.join("maps").join(format!("{map}.bsp")))
}

/// Parses a BSP image held in memory.
pub fn parse(name: &str, d: &[u8]) -> io::Result<MapInfo> {
    if d.len() < HEADER_LEN || &d[..4] != b"VBSP" {
        return Err(bad("not a VBSP file"));
    }
    let version = i32::from_le_bytes(d[4..8].try_into().unwrap());
    if !(19..=21).contains(&version) {
        return Err(bad("unsupported BSP version"));
    }
    let lump = |i: usize| -> io::Result<&[u8]> {
        let o = 8 + i * 16;
        let ofs = i32::from_le_bytes(d[o..o + 4].try_into().unwrap());
        let len = i32::from_le_bytes(d[o + 4..o + 8].try_into().unwrap());
        if ofs < 0 || len < 0 {
            return Err(bad("negative lump bounds"));
        }
        let (ofs, len) = (ofs as usize, len as usize);
        d.get(ofs..ofs.checked_add(len).ok_or_else(|| bad("lump overflow"))?)
            .ok_or_else(|| bad("lump outside file"))
    };
    let mut h = Md5::new();
    for i in 0..HEADER_LUMPS {
        if i != LUMP_ENTITIES {
            h.update(lump(i)?);
        }
    }
    let md5: [u8; 16] = h.finalize().into();
    let models = lump(LUMP_MODELS)?;
    let brush_models = models.len() / DMODEL_SIZE;
    let (mut world_mins, mut world_maxs) = ([0.0f32; 3], [0.0f32; 3]);
    if brush_models > 0 {
        let f = |i: usize| f32::from_le_bytes(models[i * 4..i * 4 + 4].try_into().unwrap());
        world_mins = [f(0), f(1), f(2)];
        world_maxs = [f(3), f(4), f(5)];
    }
    let ents = lump(LUMP_ENTITIES)?;
    let sky_name = worldspawn_key(ents, "skyname").unwrap_or_else(|| "sky_day01_01".to_string());
    Ok(MapInfo {
        name: name.to_string(),
        md5,
        brush_models,
        sky_name,
        world_mins,
        world_maxs,
        spawn_points: spawn_points(ents),
        entities: entity_blocks(ents),
    })
}

/// Splits the entity lump into key/value blocks.
fn entity_blocks(ents: &[u8]) -> Vec<Vec<(String, String)>> {
    let text = String::from_utf8_lossy(ents);
    let mut out = Vec::new();
    let mut rest = &text[..];
    while let Some(s) = rest.find('{') {
        let Some(len) = rest[s..].find('}') else {
            break;
        };
        let block = &rest[s + 1..s + len];
        let mut kv = Vec::new();
        let mut toks = block.split('"').skip(1).step_by(2);
        while let (Some(k), Some(v)) = (toks.next(), toks.next()) {
            kv.push((k.to_string(), v.to_string()));
        }
        out.push(kv);
        rest = &rest[s + len + 1..];
    }
    out
}

fn parse_vec3(s: &str) -> Option<[f32; 3]> {
    let mut it = s.split_whitespace().map(|t| t.parse::<f32>().ok());
    let v = [it.next()??, it.next()??, it.next()??];
    v.iter().all(|c| c.is_finite()).then_some(v)
}

/// Spawn points, grouped by class preference (all `info_player_start` first, …).
fn spawn_points(ents: &[u8]) -> Vec<SpawnPoint> {
    let blocks = entity_blocks(ents);
    let get = |b: &[(String, String)], key: &str| {
        b.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.clone())
    };
    let mut out = Vec::new();
    for class in SPAWN_CLASSES {
        for b in &blocks {
            if !get(b, "classname").is_some_and(|c| c.eq_ignore_ascii_case(class)) {
                continue;
            }
            let Some(origin) = get(b, "origin").and_then(|o| parse_vec3(&o)) else {
                continue;
            };
            let yaw = get(b, "angles")
                .and_then(|a| parse_vec3(&a))
                .map(|a| a[1])
                .or_else(|| get(b, "angle").and_then(|a| a.trim().parse().ok()))
                .unwrap_or(0.0);
            out.push(SpawnPoint { origin, yaw });
        }
    }
    out
}

pub fn load(game_dir: &Path, map: &str) -> io::Result<MapInfo> {
    let data = read_map(game_dir, map)?;
    parse(map, &data)
}

/// Looks up `key` in the first entity block (worldspawn).
fn worldspawn_key(ents: &[u8], key: &str) -> Option<String> {
    let text = String::from_utf8_lossy(ents);
    let start = text.find('{')?;
    let end = start + text[start..].find('}')?;
    let block = &text[start + 1..end];
    let mut toks = block.split('"').skip(1).step_by(2);
    while let (Some(k), Some(v)) = (toks.next(), toks.next()) {
        if k.eq_ignore_ascii_case(key) {
            return Some(v.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth() -> Vec<u8> {
        let ents = b"{\n\"classname\" \"worldspawn\"\n\"skyname\" \"painted\"\n}\n{\"classname\" \"x\"}\n{\n\"origin\" \"-10 20.5 -30\"\n\"angles\" \"0 90 0\"\n\"classname\" \"info_player_start\"\n}\0";
        let mut models = vec![0u8; DMODEL_SIZE * 3];
        for (i, v) in [-1.0f32, -2.0, -3.0, 4.0, 5.0, 6.0].iter().enumerate() {
            models[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        let other = b"lumpdata";
        let mut d = vec![0u8; HEADER_LEN];
        d[..4].copy_from_slice(b"VBSP");
        d[4..8].copy_from_slice(&20i32.to_le_bytes());
        let put = |d: &mut Vec<u8>, i: usize, bytes: &[u8]| {
            let ofs = d.len() as i32;
            d.extend_from_slice(bytes);
            let o = 8 + i * 16;
            d[o..o + 4].copy_from_slice(&ofs.to_le_bytes());
            d[o + 4..o + 8].copy_from_slice(&(bytes.len() as i32).to_le_bytes());
        };
        put(&mut d, 0, ents);
        put(&mut d, 1, other);
        put(&mut d, LUMP_MODELS, &models);
        d
    }

    #[test]
    fn parses_synthetic_map() {
        let d = synth();
        let m = parse("t", &d).unwrap();
        assert_eq!(m.brush_models, 3);
        assert_eq!(m.sky_name, "painted");
        // MD5 excludes the entity lump: lumps 1 and 14 only.
        let mut h = Md5::new();
        h.update(b"lumpdata");
        h.update(&d[d.len() - DMODEL_SIZE * 3..]);
        let want: [u8; 16] = h.finalize().into();
        assert_eq!(m.md5, want);
        assert_eq!(m.world_mins, [-1.0, -2.0, -3.0]);
        assert_eq!(m.world_maxs, [4.0, 5.0, 6.0]);
        assert_eq!(
            m.spawn_points,
            [SpawnPoint {
                origin: [-10.0, 20.5, -30.0],
                yaw: 90.0
            }]
        );
    }

    #[test]
    fn rejects_bad_files() {
        assert!(parse("t", b"").is_err());
        assert!(parse("t", &[0u8; 2000]).is_err());
        let mut d = synth();
        d[8..12].copy_from_slice(&i32::MAX.to_le_bytes());
        assert!(parse("t", &d).is_err());
        for cut in [0, 10, 100, HEADER_LEN] {
            let _ = parse("t", &synth()[..cut]);
        }
        assert!(load(Path::new("."), "../x").is_err());
    }
}
