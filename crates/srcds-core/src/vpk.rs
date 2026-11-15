//! Read-only access to the game's content: loose files and VPK archives (`*_dir.vpk`
//! directory files, versions 1 and 2), plus model bounds from `.mdl` headers.
//!
//! VPK layout (public format): a header (`0x55AA1234`, version, tree size; v2 adds
//! four section sizes), then a tree of extension → path → file name strings, each file
//! followed by a fixed entry (CRC, preload size, archive index, offset, length,
//! terminator `0xFFFF`) and its preload bytes. Archive index `0x7FFF` means the data
//! follows the tree in the directory file itself; otherwise it is in
//! `<base>_<index:03>.vpk`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const VPK_SIGNATURE: u32 = 0x55AA_1234;

#[derive(Debug, Clone)]
struct Entry {
    preload: Vec<u8>,
    archive: u16,
    offset: u32,
    length: u32,
}

#[derive(Debug)]
pub struct Vpk {
    /// `<base>` of `<base>_dir.vpk`.
    base: PathBuf,
    /// Offset of the embedded data section in the directory file.
    data_offset: u64,
    entries: HashMap<String, Entry>,
}

fn bad(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.to_string())
}

fn cstr(d: &[u8], pos: &mut usize) -> io::Result<String> {
    let end = d[*pos..].iter().position(|&b| b == 0).ok_or_else(|| bad("unterminated string"))?;
    let s = String::from_utf8_lossy(&d[*pos..*pos + end]).to_string();
    *pos += end + 1;
    Ok(s)
}

fn u16_at(d: &[u8], p: usize) -> io::Result<u16> {
    d.get(p..p + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).ok_or_else(|| bad("truncated"))
}

fn u32_at(d: &[u8], p: usize) -> io::Result<u32> {
    d.get(p..p + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(|| bad("truncated"))
}

impl Vpk {
    /// Opens a `*_dir.vpk` file.
    pub fn open(dir_file: &Path) -> io::Result<Vpk> {
        let d = std::fs::read(dir_file)?;
        if u32_at(&d, 0)? != VPK_SIGNATURE {
            return Err(bad("not a VPK"));
        }
        let version = u32_at(&d, 4)?;
        let tree_size = u32_at(&d, 8)? as usize;
        let header = match version {
            1 => 12,
            2 => 28,
            _ => return Err(bad("unsupported VPK version")),
        };
        let tree = d.get(header..header + tree_size).ok_or_else(|| bad("tree outside file"))?;
        let mut entries = HashMap::new();
        let mut p = 0usize;
        loop {
            let ext = cstr(tree, &mut p)?;
            if ext.is_empty() {
                break;
            }
            loop {
                let dir = cstr(tree, &mut p)?;
                if dir.is_empty() {
                    break;
                }
                loop {
                    let name = cstr(tree, &mut p)?;
                    if name.is_empty() {
                        break;
                    }
                    let preload_len = u16_at(tree, p + 4)? as usize;
                    let archive = u16_at(tree, p + 6)?;
                    let offset = u32_at(tree, p + 8)?;
                    let length = u32_at(tree, p + 12)?;
                    if u16_at(tree, p + 16)? != 0xFFFF {
                        return Err(bad("bad entry terminator"));
                    }
                    p += 18;
                    let preload = tree.get(p..p + preload_len).ok_or_else(|| bad("truncated preload"))?.to_vec();
                    p += preload_len;
                    let full = if dir == " " {
                        format!("{name}.{ext}")
                    } else {
                        format!("{dir}/{name}.{ext}")
                    };
                    entries.insert(full.to_ascii_lowercase(), Entry { preload, archive, offset, length });
                }
            }
        }
        let name = dir_file.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let base = dir_file.with_file_name(name.trim_end_matches("_dir.vpk"));
        Ok(Vpk {
            base,
            data_offset: (header + tree_size) as u64,
            entries,
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn contains(&self, path: &str) -> bool {
        self.entries.contains_key(&path.replace('\\', "/").to_ascii_lowercase())
    }

    /// Reads up to `max` bytes of `path` (all with `usize::MAX`).
    pub fn read(&self, path: &str, max: usize) -> Option<Vec<u8>> {
        let e = self.entries.get(&path.replace('\\', "/").to_ascii_lowercase())?;
        let mut out = e.preload.clone();
        let want = (e.length as usize).min(max.saturating_sub(out.len()));
        if want > 0 {
            let (file, base) = if e.archive == 0x7FFF {
                (PathBuf::from(format!("{}_dir.vpk", self.base.display())), self.data_offset)
            } else {
                (PathBuf::from(format!("{}_{:03}.vpk", self.base.display(), e.archive)), 0)
            };
            let mut f = File::open(file).ok()?;
            f.seek(SeekFrom::Start(base + e.offset as u64)).ok()?;
            let mut buf = vec![0u8; want];
            f.read_exact(&mut buf).ok()?;
            out.extend_from_slice(&buf);
        }
        out.truncate(max);
        Some(out)
    }
}

/// The game's search path: loose directories first, then VPKs, like the engine's
/// `GAME` path for a GMod server.
#[derive(Debug, Default)]
pub struct Content {
    loose: Vec<PathBuf>,
    vpks: Vec<Vpk>,
    bounds: Mutex<HashMap<String, Option<([f32; 3], [f32; 3])>>>,
}

impl Content {
    /// `garrysmod` = the game folder; its parent holds `sourceengine/`.
    pub fn mount(garrysmod: &Path) -> Content {
        let mut c = Content::default();
        c.loose.push(garrysmod.to_path_buf());
        let root = garrysmod.parent().unwrap_or(garrysmod);
        // shared engine files (resource/gameevents.res, …) live loose in sourceengine/
        if root.join("sourceengine").is_dir() {
            c.loose.push(root.join("sourceengine"));
        }
        let candidates = [
            garrysmod.join("garrysmod_dir.vpk"),
            root.join("sourceengine").join("hl2_misc_dir.vpk"),
            root.join("sourceengine").join("hl2_textures_dir.vpk"),
            root.join("sourceengine").join("content_hl2_dir.vpk"),
            root.join("sourceengine").join("content_cstrike_dir.vpk"),
        ];
        for f in candidates {
            if !f.is_file() {
                continue;
            }
            match Vpk::open(&f) {
                Ok(v) => {
                    tracing::info!(vpk = %f.display(), files = v.len(), "mounted VPK");
                    c.vpks.push(v);
                }
                Err(e) => tracing::warn!(vpk = %f.display(), error = %e, "could not mount VPK"),
            }
        }
        c
    }

    fn safe(path: &str) -> Option<String> {
        let p = path.replace('\\', "/").to_ascii_lowercase();
        (!p.split('/').any(|c| c == "..") && !p.contains(':') && !p.starts_with('/')).then_some(p)
    }

    pub fn exists(&self, path: &str) -> bool {
        let Some(p) = Self::safe(path) else { return false };
        self.loose.iter().any(|d| d.join(&p).is_file()) || self.vpks.iter().any(|v| v.contains(&p))
    }

    /// Reads up to `max` bytes of a game file.
    pub fn read(&self, path: &str, max: usize) -> Option<Vec<u8>> {
        let p = Self::safe(path)?;
        for d in &self.loose {
            let f = d.join(&p);
            if f.is_file() {
                let mut fh = File::open(f).ok()?;
                let mut buf = Vec::new();
                fh.by_ref().take(max as u64).read_to_end(&mut buf).ok()?;
                return Some(buf);
            }
        }
        self.vpks.iter().find_map(|v| v.read(&p, max))
    }

    /// Hull bounds from a model's header (`studiohdr_t::hull_min/hull_max`, at byte
    /// offsets 104 and 116 after id, version, checksum, name[64], length and the eye /
    /// illumination positions).
    pub fn model_bounds(&self, model: &str) -> Option<([f32; 3], [f32; 3])> {
        let key = model.replace('\\', "/").to_ascii_lowercase();
        if let Some(b) = self.bounds.lock().ok()?.get(&key) {
            return *b;
        }
        let b = self.read(&key, 128).and_then(|h| {
            if h.len() < 128 || &h[0..4] != b"IDST" {
                return None;
            }
            let f = |o: usize| f32::from_le_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]);
            let mins = [f(104), f(108), f(112)];
            let maxs = [f(116), f(120), f(124)];
            (mins.iter().chain(maxs.iter()).all(|v| v.is_finite() && v.abs() < 1.0e5)).then_some((mins, maxs))
        });
        if let Ok(mut m) = self.bounds.lock() {
            m.insert(key, b);
        }
        b
    }
}

/// One animation sequence of a model: its name and activity name.
#[derive(Debug, Clone, PartialEq)]
pub struct Sequence {
    pub label: String,
    pub activity: String,
    /// Seconds per cycle of its first animation ((frames - 1) / fps; 0 for a
    /// single-frame animation, as the engine computes a sequence's duration).
    pub duration: f32,
}

fn cstr_at(d: &[u8], o: usize) -> Option<String> {
    let rest = d.get(o..)?;
    let end = rest.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&rest[..end]).to_string())
}

fn i32_at(d: &[u8], o: usize) -> Option<i32> {
    d.get(o..o + 4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// The model's own sequences and its include models (public `.mdl` layout: sequence
/// count/offset at 188/192 in the header, 212-byte sequence descriptions with name
/// and activity-name offsets at +4/+8; include models at 336/340, 8 bytes each with
/// the file name offset at +4).
fn local_sequences(d: &[u8]) -> Option<(Vec<Sequence>, Vec<String>)> {
    if d.get(0..4)? != b"IDST" {
        return None;
    }
    let (n, at) = (i32_at(d, 188)?, i32_at(d, 192)?);
    let mut seqs = Vec::new();
    for k in 0..n.clamp(0, 4096) as usize {
        let b = at as usize + 212 * k;
        let label = cstr_at(d, b.checked_add_signed(i32_at(d, b + 4)? as isize)?)?;
        let activity = cstr_at(d, b.checked_add_signed(i32_at(d, b + 8)? as isize)?)?;
        // animation of blend 0: a short index at seqdesc + animindexindex (+60) into
        // the model's 100-byte animation descriptions (header +184): fps +8, frames +16
        let duration = (|| -> Option<f32> {
            let ai = b.checked_add_signed(i32_at(d, b + 60)? as isize)?;
            let idx = d.get(ai..ai + 2).map(|x| i16::from_le_bytes([x[0], x[1]]))? as usize;
            let anim = i32_at(d, 184)? as usize + idx * 100;
            let fps = f32::from_le_bytes(d.get(anim + 8..anim + 12)?.try_into().ok()?);
            let frames = i32_at(d, anim + 16)?;
            Some(if frames > 1 && fps > 0.0 { (frames - 1) as f32 / fps } else { 0.0 })
        })()
        .unwrap_or(0.0);
        seqs.push(Sequence { label, activity, duration });
    }
    let (ni, ii) = (i32_at(d, 336)?, i32_at(d, 340)?);
    let mut includes = Vec::new();
    for k in 0..ni.clamp(0, 64) as usize {
        let b = ii as usize + 8 * k;
        if let Some(name) = b.checked_add_signed(i32_at(d, b + 4)? as isize).and_then(|o| cstr_at(d, o)) {
            includes.push(name);
        }
    }
    Some((seqs, includes))
}

impl Content {
    /// Sequences as the client numbers them: the model's own, then those of each
    /// include model not already present by name ([гипотеза] the virtual-model
    /// order; checked only on the models in the tests).
    pub fn model_sequences(&self, model: &str) -> Vec<Sequence> {
        let mut out: Vec<Sequence> = Vec::new();
        let mut queue = vec![model.replace('\\', "/").to_ascii_lowercase()];
        let mut seen = Vec::new();
        while let Some(m) = queue.pop() {
            if seen.contains(&m) || seen.len() > 16 {
                continue;
            }
            seen.push(m.clone());
            let Some(d) = self.read(&m, 16 * 1024 * 1024) else { continue };
            let Some((seqs, includes)) = local_sequences(&d) else { continue };
            for sq in seqs {
                if !out.iter().any(|o| o.label.eq_ignore_ascii_case(&sq.label)) {
                    out.push(sq);
                }
            }
            for inc in includes.into_iter().rev() {
                queue.push(inc.replace('\\', "/").to_ascii_lowercase());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_vpk(dir: &Path) -> PathBuf {
        // one embedded file "models/a.mdl" with 2 preload bytes and 4 data bytes
        let mut tree = Vec::new();
        tree.extend_from_slice(b"mdl\0models\0a\0");
        tree.extend_from_slice(&0u32.to_le_bytes()); // crc
        tree.extend_from_slice(&2u16.to_le_bytes()); // preload
        tree.extend_from_slice(&0x7FFFu16.to_le_bytes());
        tree.extend_from_slice(&0u32.to_le_bytes()); // offset
        tree.extend_from_slice(&4u32.to_le_bytes()); // length
        tree.extend_from_slice(&0xFFFFu16.to_le_bytes());
        tree.extend_from_slice(b"AB");
        tree.extend_from_slice(b"\0\0\0");
        let mut d = Vec::new();
        d.extend_from_slice(&VPK_SIGNATURE.to_le_bytes());
        d.extend_from_slice(&1u32.to_le_bytes());
        d.extend_from_slice(&(tree.len() as u32).to_le_bytes());
        d.extend_from_slice(&tree);
        d.extend_from_slice(b"CDEF");
        let f = dir.join("test_dir.vpk");
        std::fs::write(&f, d).unwrap();
        f
    }

    #[test]
    fn reads_embedded_entry() {
        let dir = std::env::temp_dir().join(format!("srcds_rs_vpk_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let v = Vpk::open(&build_vpk(&dir)).unwrap();
        assert!(v.contains("MODELS\\a.mdl"));
        assert_eq!(v.read("models/a.mdl", usize::MAX).unwrap(), b"ABCDEF");
        assert_eq!(v.read("models/a.mdl", 3).unwrap(), b"ABC");
    }

    #[test]
    fn real_weapon_script() {
        let Ok(root) = std::env::var("SRCDS_RS_GMOD_DIR") else { return };
        let c = Content::mount(Path::new(&root));
        for w in ["weapon_physgun", "weapon_crowbar", "weapon_pistol"] {
            let t = c.read(&format!("scripts/weapons/{w}.txt"), 4096).expect("script");
            eprintln!("--- {w}\n{}", String::from_utf8_lossy(&t));
        }
    }

    /// Sequences of real models (`SRCDS_RS_GMOD_DIR`).
    #[test]
    fn real_model_sequences() {
        let Ok(root) = std::env::var("SRCDS_RS_GMOD_DIR") else { return };
        let c = Content::mount(Path::new(&root));
        for m in ["models/humans/group01/male_07.mdl", "models/player/kleiner.mdl", "models/combine_soldier.mdl", "models/props_c17/oildrum001.mdl"] {
            let s = c.model_sequences(m);
            let idle = s.iter().position(|q| q.activity.eq_ignore_ascii_case("ACT_IDLE"));
            eprintln!("{m}: {} sequences, first {:?}, ACT_IDLE at {idle:?}", s.len(), s.iter().take(4).map(|q| (&q.label, &q.activity)).collect::<Vec<_>>());
        }
        for (m, act) in [("models/weapons/c_pistol.mdl", "ACT_VM_PRIMARYATTACK"), ("models/weapons/c_toolgun.mdl", "ACT_VM_PRIMARYATTACK"), ("models/weapons/c_crowbar.mdl", "ACT_VM_MISSCENTER"), ("models/weapons/c_pistol.mdl", "ACT_VM_DRAW")] {
            let seqs = c.model_sequences(m);
            let i = seqs.iter().position(|q| q.activity == act);
            let idle = seqs.iter().find(|q| q.activity == "ACT_VM_IDLE").map(|q| (&q.label, q.duration));
            eprintln!("{m} {act}: {i:?} duration {:?} idle {idle:?}", i.map(|i| seqs[i].duration));
            assert!(i.is_some_and(|i| i > 0), "{m} {act}");
        }
        let s = c.model_sequences("models/humans/group01/male_07.mdl");
        assert!(s.iter().any(|q| q.activity == "ACT_IDLE"), "citizen has an idle");
    }

    #[test]
    fn real_decal_list() {
        let Ok(root) = std::env::var("SRCDS_RS_GMOD_DIR") else { return };
        let c = Content::mount(Path::new(&root));
        for f in ["scripts/decals_subrect.txt", "scripts/decals.txt"] {
            if let Some(t) = c.read(f, 1 << 20) {
                let t = String::from_utf8_lossy(&t);
                eprintln!("--- {f} {} bytes
{}", t.len(), &t[..t.len().min(1800)]);
            } else {
                eprintln!("--- {f} missing");
            }
        }
    }

    /// Bounds of a real HL2 model (needs `SRCDS_RS_GMOD_DIR`).
    #[test]
    fn real_model_bounds() {
        let Ok(root) = std::env::var("SRCDS_RS_GMOD_DIR") else { return };
        let c = Content::mount(Path::new(&root));
        let b = c.model_bounds("models/props_c17/oildrum001.mdl");
        eprintln!("oildrum bounds {b:?}");
        let (mn, mx) = b.expect("bounds");
        assert!(mx[2] > mn[2] + 20.0);
    }
}
