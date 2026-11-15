//! `status` / `ping` output, for the server console and for players' consoles.
//!
//! `status` is a fastfetch-style card: the srcds-rs logo (a "g" cut out of a square,
//! drawn in orange on terminals) next to server facts, followed by the familiar
//! player table.

use std::fmt::Write as _;
use std::time::Duration;

/// The srcds-rs logo: a lower-case "g" cut out of a block.
const LOGO: &[&str] = &[
    "####################",
    "#######      #######",
    "#####  ######  #####",
    "####  ########  ####",
    "####  ########  ####",
    "#####  ######   ####",
    "#######      #  ####",
    "##############  ####",
    "#####  ####    #####",
    "#######      #######",
    "####################",
];

const ORANGE: &str = "\x1b[38;5;208m";
const BOLD_ORANGE: &str = "\x1b[1;38;5;208m";
const RESET: &str = "\x1b[0m";

#[derive(Debug, Clone)]
pub struct PlayerLine {
    pub user_id: i32,
    pub name: String,
    pub unique_id: String,
    pub connected: Duration,
    pub ping_ms: i32,
    pub state: &'static str,
    pub addr: String,
}

#[derive(Debug, Clone)]
pub struct StatusInfo {
    pub host_name: String,
    pub version: String,
    pub game_version: String,
    pub map: String,
    pub gamemode: String,
    pub max_players: u8,
    pub tick_rate: f64,
    pub fps: f64,
    pub tick_p99_us: u64,
    pub uptime: Duration,
    pub address: String,
    pub lan: bool,
    pub brushes: usize,
    pub players: Vec<PlayerLine>,
}

fn hms(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

fn uptime(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, s / 60 % 60),
    }
}

/// Renders the status card. `color` adds ANSI colors (terminals only; the game
/// console shows plain text). `show_addresses` includes player IP addresses (server
/// operator only).
pub fn render(i: &StatusInfo, color: bool, show_addresses: bool) -> String {
    let (logo_c, key_c, reset) = if color {
        (ORANGE, BOLD_ORANGE, RESET)
    } else {
        ("", "", "")
    };
    let title = format!("srcds-rs@{}", i.host_name);
    let auth = if i.lan {
        "LAN, Steam tickets not validated"
    } else {
        "Steam"
    };
    let facts: Vec<(String, String)> = vec![
        ("".into(), title.clone()),
        ("".into(), "-".repeat(title.len())),
        (
            "Engine".into(),
            format!("srcds-rs {} (native Rust)", i.version),
        ),
        (
            "Game".into(),
            format!("Garry's Mod {} - {}", i.game_version, i.gamemode),
        ),
        ("Map".into(), i.map.clone()),
        (
            "Players".into(),
            format!("{}/{}", i.players.len(), i.max_players),
        ),
        (
            "Tick".into(),
            format!(
                "{:.1} Hz, fps {:.1}, p99 {} us",
                i.tick_rate, i.fps, i.tick_p99_us
            ),
        ),
        ("Uptime".into(), uptime(i.uptime)),
        ("Address".into(), format!("{} ({auth})", i.address)),
        (
            "OS".into(),
            format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        ),
        ("World".into(), format!("{} brushes", i.brushes)),
    ];
    let mut out = String::from("\n");
    for row in 0..LOGO.len().max(facts.len()) {
        let logo = LOGO.get(row).copied().unwrap_or("");
        let _ = write!(out, " {logo_c}{logo:<20}{reset}   ");
        if let Some((k, v)) = facts.get(row) {
            if k.is_empty() {
                let _ = write!(out, "{key_c}{v}{reset}");
            } else {
                let _ = write!(out, "{key_c}{k}:{reset} {v}");
            }
        }
        out.push('\n');
    }
    out.push('\n');
    let _ = writeln!(
        out,
        "# userid name                     uniqueid              connected  ping  state{}",
        if show_addresses { "   adr" } else { "" }
    );
    for p in &i.players {
        let name = format!("\"{}\"", p.name);
        let _ = writeln!(
            out,
            "# {:>6} {:<24} {:<21} {:>9} {:>5}  {:<7}{}",
            p.user_id,
            name,
            p.unique_id,
            hms(p.connected),
            p.ping_ms,
            p.state,
            if show_addresses {
                format!(" {}", p.addr)
            } else {
                String::new()
            }
        );
    }
    out
}

/// Renders the `ping` command output.
pub fn render_ping(players: &[PlayerLine]) -> String {
    let mut out = String::from("Client ping times:\n");
    for p in players {
        let _ = writeln!(out, " {:>4} ms : {}", p.ping_ms, p.name);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> StatusInfo {
        StatusInfo {
            host_name: "test".into(),
            version: "0.1.0".into(),
            game_version: "2026.04.29".into(),
            map: "gm_construct".into(),
            gamemode: "sandbox".into(),
            max_players: 32,
            tick_rate: 66.7,
            fps: 66.0,
            tick_p99_us: 90,
            uptime: Duration::from_secs(125),
            address: "192.168.0.2:27015".into(),
            lan: true,
            brushes: 1000,
            players: vec![PlayerLine {
                user_id: 2,
                name: "player".into(),
                unique_id: "STEAM_0:0:1".into(),
                connected: Duration::from_secs(61),
                ping_ms: 12,
                state: "active",
                addr: "192.168.0.3:27005".into(),
            }],
        }
    }

    #[test]
    fn plain_card_has_logo_facts_and_players() {
        let s = render(&info(), false, false);
        assert!(!s.contains('\x1b'));
        assert!(s.contains("####################   srcds-rs@test"));
        assert!(s.contains("Map: gm_construct"));
        assert!(s.contains("Players: 1/32"));
        assert!(s.contains("Uptime: 2m 5s"));
        assert!(s.contains("\"player\""));
        assert!(!s.contains("192.168.0.3"));
        assert!(s.lines().all(|l| l.is_ascii()));
    }

    #[test]
    fn colored_card_and_addresses_for_operator() {
        let s = render(&info(), true, true);
        assert!(s.contains(ORANGE));
        assert!(s.contains("192.168.0.3:27005"));
        assert!(render_ping(&info().players).contains("12 ms : player"));
    }
}
