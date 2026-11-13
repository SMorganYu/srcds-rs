//! Golden tests against datagrams captured from the reference Garry's Mod
//! dedicated server (PatchVersion 2026.04.29, garrysmod.ver 261104, win64),
//! captured with `scripts/a2s_probe.py`. See docs/TEST_RESULTS.md.

use srcds_protocol::a2s::{decode_challenge, InfoResponse, PlayerResponse, RulesResponse};
use srcds_protocol::bytes::ByteWriter;
use srcds_protocol::connectionless::{classify, Framing};
use srcds_protocol::split::{self, parse_fragment, Reassembler};

macro_rules! fixture {
    ($name:literal) => {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/a2s/ref_gmod_2026.04.29/",
            $name
        ))
        .as_slice()
    };
}

fn payload(d: &[u8]) -> &[u8] {
    match classify(d).unwrap() {
        Framing::Connectionless(p) => p,
        other => panic!("unexpected framing {other:?}"),
    }
}

#[test]
fn challenge_responses() {
    for f in [
        fixture!("info_0.bin"),
        fixture!("player_0.bin"),
        fixture!("rules_0.bin"),
    ] {
        assert_eq!(f.len(), 9);
        decode_challenge(payload(f)).unwrap();
    }
    // Observed: the reference issues the same challenge to one address for all
    // three query types within the capture window.
    assert_eq!(fixture!("info_0.bin"), fixture!("player_0.bin"));
    assert_eq!(fixture!("info_0.bin"), fixture!("rules_0.bin"));
}

#[test]
fn info_decodes_and_reencodes_byte_identical() {
    let raw = fixture!("info_1.bin");
    let info = InfoResponse::decode(payload(raw)).unwrap();
    assert_eq!(info.protocol, 17);
    assert_eq!(info.map, "gm_construct");
    assert_eq!(info.folder, "garrysmod");
    assert_eq!(info.game, "Sandbox");
    assert_eq!(info.app_id, 4000);
    assert_eq!(info.server_type, b'd');
    assert_eq!(info.environment, b'w');
    assert_eq!(info.version, "2026.04.29");
    assert_eq!(info.edf(), 0xB1);
    assert_eq!(info.port, Some(27115));
    assert!(info.steam_id.is_some());
    assert_eq!(
        info.keywords.as_deref(),
        Some(" gm:sandbox gmc:other ver:261104")
    );
    assert_eq!(info.game_id, Some(4000));

    let mut w = ByteWriter::with_capacity(1400);
    info.encode(&mut w).unwrap();
    assert_eq!(w.as_slice(), raw, "encoder must reproduce reference bytes");
}

#[test]
fn empty_player_list() {
    let raw = fixture!("player_1.bin");
    let p = PlayerResponse::decode(payload(raw)).unwrap();
    assert!(p.players.is_empty());
    let mut w = ByteWriter::with_capacity(64);
    p.encode(&mut w).unwrap();
    assert_eq!(w.as_slice(), raw);
}

#[test]
fn rules_split_reassemble_and_resplit_identical() {
    let frags = [
        fixture!("rules_1.bin"),
        fixture!("rules_2.bin"),
        fixture!("rules_3.bin"),
    ];
    for (i, f) in frags.iter().enumerate() {
        let (h, _) = parse_fragment(f).unwrap();
        assert_eq!(h.total, 3);
        assert_eq!(h.number as usize, i);
        assert_eq!(h.size, 1248);
        assert!(!h.compressed());
    }
    let mut r = Reassembler::new();
    let mut full = None;
    for f in frags {
        full = r.push(f).unwrap();
    }
    let full = full.unwrap();
    assert_eq!(full, fixture!("rules_full.bin"));

    let rules = RulesResponse::decode(payload(&full)).unwrap();
    assert_eq!(rules.rules.len(), 148);
    assert!(rules
        .rules
        .iter()
        .any(|(k, v)| k == "sv_gravity" && v == "600"));
    // Not byte order ("ttt_detective_pct" precedes "ttt_det_credits_starting"),
    // but consistent with comparing ASCII-uppercased names.
    assert!(rules.rules.windows(2).any(|w| w[0].0 > w[1].0));
    assert!(
        rules
            .rules
            .windows(2)
            .all(|w| srcds_protocol::a2s::rule_name_cmp(&w[0].0, &w[1].0).is_le()),
        "reference rules are sorted case-insensitively by uppercased name"
    );

    // Our encoder + splitter reproduce the reference datagrams exactly.
    let mut w = ByteWriter::with_capacity(8192);
    rules.encode(&mut w).unwrap();
    assert_eq!(w.as_slice(), full.as_slice());
    let (h0, _) = parse_fragment(frags[0]).unwrap();
    let ours = split::split(w.as_slice(), h0.id, 1248).unwrap();
    assert_eq!(ours.len(), 3);
    for (a, b) in ours.iter().zip(frags) {
        assert_eq!(a.as_slice(), b);
    }
}

#[test]
fn every_truncation_of_reference_packets_is_rejected_without_panic() {
    let info = fixture!("info_1.bin");
    for cut in 0..info.len() {
        let _ = classify(&info[..cut]).map(|f| {
            if let Framing::Connectionless(p) = f {
                let _ = InfoResponse::decode(p);
            }
        });
    }
    let rules = fixture!("rules_full.bin");
    for cut in 4..rules.len() - 1 {
        assert!(RulesResponse::decode(&rules[4..cut]).is_err());
    }
}
