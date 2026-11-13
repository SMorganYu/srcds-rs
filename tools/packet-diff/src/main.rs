//! packet-diff: semantic comparison of two captured connectionless datagrams.
//!
//! ```text
//! packet-diff <reference.bin> <candidate.bin> [--ignore field,field...]
//! ```
//!
//! Each file holds one raw UDP payload (as written by `scripts/a2s_probe.py`).
//! Packets are decoded with `srcds-protocol`; fields are compared by name so that
//! dynamic values (challenge numbers, durations, hostnames) can be ignored.
//! Exit status: 0 = equivalent, 1 = differences, 2 = usage/decode error.

use std::collections::BTreeMap;
use std::process::ExitCode;

use srcds_protocol::a2s::{self, InfoResponse, PlayerResponse, RulesResponse};
use srcds_protocol::connectionless::{classify, kind, Framing};

type Fields = BTreeMap<String, String>;

fn decode(data: &[u8]) -> Result<Fields, String> {
    let payload = match classify(data).map_err(|e| e.to_string())? {
        Framing::Connectionless(p) => p,
        Framing::Split(_) => return Err("split packets: reassemble first".into()),
        Framing::Netchannel(_) => return Err("netchannel packet: not supported yet".into()),
    };
    let mut f = Fields::new();
    f.insert("_len".into(), data.len().to_string());
    let k = *payload.first().ok_or("empty payload")?;
    f.insert("_type".into(), char::from(k).to_string());
    match k {
        kind::S2A_INFO => {
            let i = InfoResponse::decode(payload).map_err(|e| e.to_string())?;
            for (n, v) in [
                ("protocol", i.protocol.to_string()),
                ("name", i.name.clone()),
                ("map", i.map.clone()),
                ("folder", i.folder.clone()),
                ("game", i.game.clone()),
                ("app_id", i.app_id.to_string()),
                ("players", i.players.to_string()),
                ("max_players", i.max_players.to_string()),
                ("bots", i.bots.to_string()),
                ("server_type", char::from(i.server_type).to_string()),
                ("environment", char::from(i.environment).to_string()),
                ("visibility", i.visibility.to_string()),
                ("vac", i.vac.to_string()),
                ("version", i.version.clone()),
                ("edf", format!("{:#04x}", i.edf())),
                ("port", format!("{:?}", i.port)),
                ("steam_id", format!("{:?}", i.steam_id)),
                ("source_tv", format!("{:?}", i.source_tv)),
                ("keywords", format!("{:?}", i.keywords)),
                ("game_id", format!("{:?}", i.game_id)),
            ] {
                f.insert(n.into(), v);
            }
        }
        kind::S2C_CHALLENGE => {
            let c = a2s::decode_challenge(payload).map_err(|e| e.to_string())?;
            f.insert("challenge".into(), format!("{c:#010x}"));
        }
        kind::S2A_PLAYER => {
            let p = PlayerResponse::decode(payload).map_err(|e| e.to_string())?;
            f.insert("count".into(), p.players.len().to_string());
            for (i, e) in p.players.iter().enumerate() {
                f.insert(format!("player[{i}].name"), e.name.clone());
                f.insert(format!("player[{i}].score"), e.score.to_string());
                f.insert(format!("player[{i}].duration"), e.duration.to_string());
            }
        }
        kind::S2A_RULES => {
            let r = RulesResponse::decode(payload).map_err(|e| e.to_string())?;
            f.insert("count".into(), r.rules.len().to_string());
            for (k, v) in r.rules {
                f.insert(format!("rule.{k}"), v);
            }
        }
        other => return Err(format!("unknown connectionless type {other:#04x}")),
    }
    Ok(f)
}

/// Returns `(field, reference, candidate)` triples that differ, skipping ignored
/// fields. A trailing `*` in an ignore pattern matches any suffix.
fn diff(a: &Fields, b: &Fields, ignore: &[String]) -> Vec<(String, String, String)> {
    let ignored = |k: &str| {
        ignore.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => k.starts_with(prefix),
            None => k == p,
        })
    };
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| !ignored(k))
        .filter_map(|k| {
            let va = a.get(k).cloned().unwrap_or_else(|| "<absent>".into());
            let vb = b.get(k).cloned().unwrap_or_else(|| "<absent>".into());
            (va != vb).then(|| (k.clone(), va, vb))
        })
        .collect()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut files = Vec::new();
    let mut ignore = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--ignore" {
            match it.next() {
                Some(v) => ignore.extend(v.split(',').map(str::to_string)),
                None => {
                    eprintln!("--ignore needs a value");
                    return ExitCode::from(2);
                }
            }
        } else {
            files.push(a.clone());
        }
    }
    if files.len() != 2 {
        eprintln!("usage: packet-diff <reference.bin> <candidate.bin> [--ignore f1,f2,prefix*]");
        return ExitCode::from(2);
    }
    let mut decoded = Vec::new();
    for p in &files {
        let data = match std::fs::read(p) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{p}: {e}");
                return ExitCode::from(2);
            }
        };
        match decode(&data) {
            Ok(f) => decoded.push(f),
            Err(e) => {
                eprintln!("{p}: decode error: {e}");
                return ExitCode::from(2);
            }
        }
    }
    let d = diff(&decoded[0], &decoded[1], &ignore);
    if d.is_empty() {
        println!("equivalent ({} fields compared)", decoded[0].len());
        return ExitCode::SUCCESS;
    }
    println!("{:<24} {:<32} {:<32}", "field", "reference", "candidate");
    for (k, a, b) in &d {
        println!("{k:<24} {a:<32} {b:<32}");
    }
    ExitCode::from(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use srcds_protocol::bytes::ByteWriter;

    fn info_bytes(map: &str, players: u8) -> Vec<u8> {
        let i = InfoResponse {
            protocol: 17,
            name: "a".into(),
            map: map.into(),
            folder: "garrysmod".into(),
            game: "g".into(),
            app_id: 4000,
            players,
            max_players: 8,
            bots: 0,
            server_type: b'd',
            environment: b'w',
            visibility: 0,
            vac: 0,
            version: "1".into(),
            port: None,
            steam_id: None,
            source_tv: None,
            keywords: None,
            game_id: None,
        };
        let mut w = ByteWriter::with_capacity(1400);
        i.encode(&mut w).unwrap();
        w.into_inner()
    }

    #[test]
    fn detects_and_ignores_differences() {
        let a = decode(&info_bytes("gm_construct", 0)).unwrap();
        let b = decode(&info_bytes("gm_flatgrass", 1)).unwrap();
        let d = diff(&a, &b, &[]);
        let names: Vec<_> = d.iter().map(|x| x.0.as_str()).collect();
        assert!(names.contains(&"map") && names.contains(&"players"));
        assert_eq!(
            diff(&a, &b, &["map".into(), "play*".into(), "_len".into()]),
            vec![]
        );
    }

    #[test]
    fn decode_errors_are_reported() {
        assert!(decode(b"\xff\xff\xff\xffI\x11").is_err());
        assert!(decode(b"\x01\x00\x00\x00").is_err());
        assert!(decode(b"").is_err());
    }
}
