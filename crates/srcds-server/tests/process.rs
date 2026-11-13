//! Integration tests that launch the real `srcds-rs` executable and talk to it over
//! loopback UDP like an external query client would.

use std::io::Write;
use std::net::UdpSocket;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use srcds_protocol::a2s::{decode_challenge, InfoResponse, PlayerResponse, RulesResponse};

struct ServerProc {
    child: Child,
    port: u16,
}

impl ServerProc {
    fn start(extra: &[&str]) -> Self {
        // Reserve a free port, release it, and hand it to the server.
        let port = UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new(env!("CARGO_BIN_EXE_srcds-rs"))
            .args(["--port", &port.to_string(), "--metrics-interval", "0"])
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn srcds-rs");
        let p = Self { child, port };
        p.wait_ready();
        p
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let s = client();
        while Instant::now() < deadline {
            if exchange(&s, self.port, &info_request(None)).is_some() {
                return;
            }
        }
        panic!("server did not answer within 10s");
    }

    fn quit(mut self) -> std::process::ExitStatus {
        let mut stdin = self.child.stdin.take().unwrap();
        stdin.write_all(b"quit\n").unwrap();
        drop(stdin);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                return st;
            }
            assert!(Instant::now() < deadline, "server did not exit after quit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for ServerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn client() -> UdpSocket {
    let s = UdpSocket::bind("127.0.0.1:0").unwrap();
    s.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    s
}

fn exchange(s: &UdpSocket, port: u16, req: &[u8]) -> Option<Vec<u8>> {
    s.send_to(req, ("127.0.0.1", port)).ok()?;
    let mut buf = [0u8; 2048];
    let (n, _) = s.recv_from(&mut buf).ok()?;
    Some(buf[..n].to_vec())
}

fn info_request(challenge: Option<i32>) -> Vec<u8> {
    let mut p = b"\xff\xff\xff\xffTSource Engine Query\0".to_vec();
    if let Some(c) = challenge {
        p.extend_from_slice(&c.to_le_bytes());
    }
    p
}

#[test]
fn startup_query_and_graceful_shutdown() {
    let srv = ServerProc::start(&[
        "--map",
        "gm_flatgrass",
        "--max-players",
        "12",
        "--hostname",
        "it-test",
    ]);
    let s = client();

    // A2S_INFO: challenge first, then info.
    let r = exchange(&s, srv.port, &info_request(None)).unwrap();
    assert_eq!(&r[..5], b"\xff\xff\xff\xffA");
    let ch = decode_challenge(&r[4..]).unwrap();
    let r = exchange(&s, srv.port, &info_request(Some(ch))).unwrap();
    let info = InfoResponse::decode(&r[4..]).unwrap();
    assert_eq!(info.name, "it-test");
    assert_eq!(info.map, "gm_flatgrass");
    assert_eq!(info.folder, "garrysmod");
    assert_eq!(info.max_players, 12);
    assert_eq!(info.players, 0);
    assert_eq!(info.port, Some(srv.port));
    assert_eq!(info.game_id, Some(4000));

    // A2S_PLAYER / A2S_RULES.
    let r = exchange(&s, srv.port, b"\xff\xff\xff\xffU\xff\xff\xff\xff").unwrap();
    let ch = decode_challenge(&r[4..]).unwrap();
    let mut req = b"\xff\xff\xff\xffU".to_vec();
    req.extend_from_slice(&ch.to_le_bytes());
    let r = exchange(&s, srv.port, &req).unwrap();
    assert!(PlayerResponse::decode(&r[4..]).unwrap().players.is_empty());
    req[4] = b'V';
    let r = exchange(&s, srv.port, &req).unwrap();
    let rules = RulesResponse::decode(&r[4..]).unwrap();
    assert!(rules.rules.iter().any(|(k, _)| k == "sv_password"));

    // Garbage must not crash the server or elicit a response.
    for junk in [
        &b""[..],
        b"\xff",
        b"\xff\xff\xff\xff",
        b"\xff\xff\xff\xffT",
        b"\x01\x00\x00\x00junk",
        b"\xfe\xff\xff\xff\x00",
    ] {
        assert!(
            exchange(&s, srv.port, junk).is_none(),
            "junk {junk:?} answered"
        );
    }
    // Still alive afterwards.
    assert!(exchange(&s, srv.port, &info_request(None)).is_some());

    let status = srv.quit();
    assert!(status.success(), "exit status {status:?}");
}

#[test]
fn password_flag_is_reported_not_leaked() {
    let srv = ServerProc::start(&["--password", "hunter2"]);
    let s = client();
    let ch = decode_challenge(&exchange(&s, srv.port, &info_request(None)).unwrap()[4..]).unwrap();
    let r = exchange(&s, srv.port, &info_request(Some(ch))).unwrap();
    assert_eq!(InfoResponse::decode(&r[4..]).unwrap().visibility, 1);
    let mut req = b"\xff\xff\xff\xffV".to_vec();
    req.extend_from_slice(&ch.to_le_bytes());
    let r = exchange(&s, srv.port, &req).unwrap();
    assert!(!r.windows(7).any(|w| w == b"hunter2"));
    srv.quit();
}

#[test]
fn bind_conflict_fails_cleanly() {
    let holder = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    let out = Command::new(env!("CARGO_BIN_EXE_srcds-rs"))
        .args(["--port", &port.to_string()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
}
