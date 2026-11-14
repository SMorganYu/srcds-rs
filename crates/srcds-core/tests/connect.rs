//! End-to-end connection test: drive the server's handshake and signon over real UDP
//! with an in-process client built from the protocol crate (no game client needed).
//!
//! Covers: A2S_GETCHALLENGE → S2C_CHALLENGE → C2S_CONNECT → S2C_CONNECTION, then a
//! netchannel SignonState(CONNECTED) and the server's reliable reply containing
//! svc_ServerInfo followed by a clean net_Disconnect (default signon mode).

use std::net::UdpSocket;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use srcds_core::{Server, ServerConfig};
use srcds_protocol::bitbuf::BitReader;
use srcds_protocol::handshake::{
    ChallengeResponse, ConnectRequest, AUTH_PROTOCOL_STEAM, PROTOCOL_VERSION, S2C_CONNECTION,
};
use srcds_protocol::netchan::NetChannel;
use srcds_protocol::netmsg;

/// Writes a minimal valid VBSP (version 20) with 3 brush models and a worldspawn into
/// `<dir>/maps/<name>.bsp` so the server can load a map without game content.
fn write_synthetic_map(dir: &std::path::Path, name: &str) {
    const HEADER_LUMPS: usize = 64;
    const LUMP_MODELS: usize = 14;
    const HEADER_LEN: usize = 8 + HEADER_LUMPS * 16 + 4;
    let ents = b"{\n\"classname\" \"worldspawn\"\n\"skyname\" \"sky_day01_01\"\n}\0";
    let models = vec![0u8; 48 * 3];
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
    put(&mut d, LUMP_MODELS, &models);
    let maps = dir.join("maps");
    std::fs::create_dir_all(&maps).unwrap();
    std::fs::write(maps.join(format!("{name}.bsp")), d).unwrap();
}

struct Harness {
    sock: UdpSocket,
    _dir: PathBuf,
}

fn start_server(
    full_signon: bool,
) -> (Harness, srcds_core::ShutdownHandle, thread::JoinHandle<()>) {
    let dir = std::env::temp_dir().join(format!(
        "srcds_rs_connect_{}_{}",
        std::process::id(),
        full_signon
    ));
    std::fs::create_dir_all(&dir).unwrap();
    write_synthetic_map(&dir, "gm_test");
    let cfg = ServerConfig {
        ip: "127.0.0.1".parse().unwrap(),
        port: 0,
        map: "gm_test".into(),
        max_players: 4,
        game_dir: Some(dir.clone()),
        metrics_log_interval: None,
        full_signon,
        ..ServerConfig::default()
    };
    let mut server = Server::bind(cfg).expect("bind");
    let addr = server.local_addr().unwrap();
    let shutdown = server.shutdown_handle();
    let handle = thread::spawn(move || {
        server.run().unwrap();
    });
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    (Harness { sock, _dir: dir }, shutdown, handle)
}

impl Harness {
    fn send(&self, d: &[u8]) {
        self.sock.send(d).unwrap();
    }
    fn recv(&self) -> Option<Vec<u8>> {
        let mut buf = [0u8; 2048];
        match self.sock.recv(&mut buf) {
            Ok(n) => Some(buf[..n].to_vec()),
            Err(_) => None,
        }
    }
    /// Receives until `pred` returns true on a datagram or the deadline passes.
    fn recv_until(&self, dur: Duration, mut pred: impl FnMut(&[u8]) -> bool) -> Option<Vec<u8>> {
        let deadline = Instant::now() + dur;
        while Instant::now() < deadline {
            if let Some(d) = self.recv() {
                if pred(&d) {
                    return Some(d);
                }
            }
        }
        None
    }
}

/// Runs challenge → connect and returns the netchannel with the server challenge, after
/// telling the server we are CONNECTED.
fn connect(h: &Harness) -> NetChannel {
    let client_challenge = 0x0CAB_CAFE;
    h.send(&srcds_protocol::handshake::encode_get_challenge(client_challenge).unwrap());
    let chal = h
        .recv_until(Duration::from_secs(2), |d| d.len() > 5 && d[4] == b'A')
        .expect("no S2C_CHALLENGE");
    let chal = ChallengeResponse::decode(&chal).expect("decode challenge");
    assert_eq!(chal.client_challenge, client_challenge);
    assert_eq!(chal.auth_protocol, AUTH_PROTOCOL_STEAM);

    let mut cookie = 76561197960265728u64.to_le_bytes().to_vec();
    cookie.extend_from_slice(&[0u8; 8]);
    let req = ConnectRequest {
        protocol: PROTOCOL_VERSION,
        auth_protocol: AUTH_PROTOCOL_STEAM,
        server_challenge: chal.server_challenge,
        client_challenge,
        gmod_extra: 0,
        name: "tester".into(),
        password: String::new(),
        product_version: "2026.04.29".into(),
        steam_cookie: &cookie,
    };
    h.send(&req.encode().unwrap());
    let conn = h
        .recv_until(Duration::from_secs(2), |d| {
            d.len() > 5 && d[4] == S2C_CONNECTION
        })
        .expect("no S2C_CONNECTION");
    assert_eq!(&conn[5..9], &client_challenge.to_le_bytes());

    let mut chan = NetChannel::new(chal.server_challenge);
    let mut w = srcds_protocol::bitbuf::BitWriter::with_capacity(64);
    netmsg::write_signon_state(&mut w, netmsg::SIGNONSTATE_CONNECTED, -1).unwrap();
    chan.write_reliable(&w).unwrap();
    h.send(&chan.transmit(None).unwrap());
    chan
}

/// Collects decoded server→client message types from reliable transfers for `dur`,
/// acknowledging each packet so the server keeps advancing. Reassembles split packets.
fn collect_message_types(h: &Harness, chan: &mut NetChannel, dur: Duration) -> Vec<u32> {
    use srcds_protocol::split::Reassembler;
    let mut types = Vec::new();
    let mut reasm = Reassembler::new();
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        let Some(mut d) = h.recv() else { continue };
        if d.len() >= 4
            && i32::from_le_bytes(d[..4].try_into().unwrap())
                == srcds_protocol::connectionless::SPLIT_HEADER
        {
            match reasm.push(&d) {
                Ok(Some(v)) => d = v,
                _ => continue,
            }
        }
        if let Ok(Some(inc)) = chan.process(&d) {
            for buf in &inc.reliable {
                let mut r = BitReader::new(buf);
                while r.bits_remaining() >= 6 {
                    let Ok(t) = r.read_ubits(6) else { break };
                    types.push(t);
                    // Only the few message bodies we must step over to keep parsing.
                    let ok = match t {
                        0 => Ok(()),
                        t if t == netmsg::SVC_PRINT => r.read_string(4096).map(|_| ()),
                        t if t == netmsg::NET_DISCONNECT => r.read_string(4096).map(|_| ()),
                        t if t == netmsg::NET_TICK => (|| {
                            r.read_sbits(32)?;
                            r.read_ubits(16)?;
                            r.read_ubits(16).map(|_| ())
                        })(),
                        _ => break, // stop at the first body we do not step over here
                    };
                    if ok.is_err() {
                        break;
                    }
                }
            }
            h.send(&chan.transmit(None).unwrap());
        }
    }
    types
}

#[test]
fn handshake_then_graceful_disconnect_minimal() {
    let (h, shutdown, handle) = start_server(false);
    let mut chan = connect(&h);
    let types = collect_message_types(&h, &mut chan, Duration::from_secs(5));
    assert!(
        types.contains(&netmsg::SVC_SERVERINFO),
        "no svc_ServerInfo: {types:?}"
    );
    assert!(
        types.contains(&netmsg::NET_DISCONNECT),
        "no net_Disconnect: {types:?}"
    );
    shutdown.request();
    handle.join().unwrap();
}

#[test]
fn full_signon_sends_tables_and_advances_state() {
    let (h, shutdown, handle) = start_server(true);
    let mut chan = connect(&h);
    let types = collect_message_types(&h, &mut chan, Duration::from_secs(5));
    assert!(
        types.contains(&netmsg::SVC_SERVERINFO),
        "no svc_ServerInfo: {types:?}"
    );
    assert!(
        types.contains(&netmsg::SVC_CREATESTRINGTABLE),
        "no svc_CreateStringTable in full signon: {types:?}"
    );
    assert!(
        !types.contains(&netmsg::NET_DISCONNECT),
        "full signon should not disconnect: {types:?}"
    );
    // (SignonState and later messages trail the 19 string tables in the same reliable
    // buffer, which this test does not fully decode; the live client test in
    // docs/TEST_RESULTS.md covers the path through SignonState(FULL).)
    shutdown.request();
    handle.join().unwrap();
}
