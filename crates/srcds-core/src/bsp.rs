//! Minimal BSP (VBSP) reader: what the server needs to describe a map to clients.
//!
//! * Map MD5 as checked by clients: MD5 over every lump's bytes in lump order,
//!   skipping the entity lump (verified: gm_construct → `3b5c56cf…`, identical to the
//!   value the reference server sends in `svc_ServerInfo`).
//! * Number of brush models (lump 14, 48-byte `dmodel_t`), which become the `*N`
//!   entries of `modelprecache`.
//! * `skyname` of `worldspawn` from the entity lump.

use std::io;
use std::path::Path;

use md5::{Digest, Md5};

const HEADER_LUMPS: usize = 64;
const LUMP_ENTITIES: usize = 0;
const LUMP_MODELS: usize = 14;
const DMODEL_SIZE: usize = 48;
const HEADER_LEN: usize = 8 + HEADER_LUMPS * 16 + 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapInfo {
    pub name: String,
    pub md5: [u8; 16],
    /// Number of brush models including the world (model 0).
    pub brush_models: usize,
    pub sky_name: String,
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
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
    let brush_models = lump(LUMP_MODELS)?.len() / DMODEL_SIZE;
    let ents = lump(LUMP_ENTITIES)?;
    let sky_name = worldspawn_key(ents, "skyname").unwrap_or_else(|| "sky_day01_01".to_string());
    Ok(MapInfo {
        name: name.to_string(),
        md5,
        brush_models,
        sky_name,
    })
}

pub fn load(game_dir: &Path, map: &str) -> io::Result<MapInfo> {
    if map.contains(['/', '\\', '.']) {
        return Err(bad("map name must not contain path separators or dots"));
    }
    let path = game_dir.join("maps").join(format!("{map}.bsp"));
    let data = std::fs::read(&path)?;
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
        let ents =
            b"{\n\"classname\" \"worldspawn\"\n\"skyname\" \"painted\"\n}\n{\"classname\" \"x\"}\0";
        let models = vec![0u8; DMODEL_SIZE * 3];
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
        h.update(vec![0u8; DMODEL_SIZE * 3]);
        let want: [u8; 16] = h.finalize().into();
        assert_eq!(m.md5, want);
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
