//! srcds-rs: native Garry's Mod dedicated server prototype.

use std::net::IpAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use srcds_core::{steaminf, Server, ServerConfig};
use tracing::{error, info, warn};

#[derive(Debug, Parser)]
#[command(
    name = "srcds-rs",
    version,
    about = "Native Rust Garry's Mod dedicated server (prototype)"
)]
struct Args {
    /// IP address to bind. Defaults to loopback; binding a public address is an
    /// explicit opt-in.
    #[arg(long, default_value = "127.0.0.1")]
    ip: IpAddr,
    /// UDP game/query port.
    #[arg(long, default_value_t = 27015)]
    port: u16,
    /// Map name reported to clients.
    #[arg(long, default_value = "gm_construct")]
    map: String,
    #[arg(long, default_value_t = 32, value_parser = clap::value_parser!(u8).range(1..=128))]
    max_players: u8,
    #[arg(long, default_value = "srcds-rs")]
    hostname: String,
    /// Server password (only its presence is ever reported).
    #[arg(long)]
    password: Option<String>,
    /// Path to a Garry's Mod `garrysmod` directory; `steam.inf` there provides the
    /// reported version.
    #[arg(long)]
    game_dir: Option<PathBuf>,
    /// Tick rate in Hz.
    #[arg(long, default_value_t = 66.666_67)]
    tickrate: f64,
    /// Seconds between metrics log lines (0 disables).
    #[arg(long, default_value_t = 60)]
    metrics_interval: u64,
    /// Attempt the experimental, incomplete world signon after ServerInfo (string
    /// tables and beyond). Off by default; the unfinished path can disrupt the client.
    #[arg(long)]
    experimental_signon: bool,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    if !(1.0..=1000.0).contains(&args.tickrate) {
        error!(tickrate = args.tickrate, "tickrate must be within 1..=1000");
        return ExitCode::from(2);
    }
    if !args.ip.is_loopback() {
        warn!(ip = %args.ip, "binding a non-loopback address; the server is reachable from the network");
    }

    let mut cfg = ServerConfig {
        ip: args.ip,
        port: args.port,
        hostname: args.hostname,
        map: args.map,
        max_players: args.max_players,
        password: args.password,
        tick_interval: Duration::from_secs_f64(1.0 / args.tickrate),
        metrics_log_interval: (args.metrics_interval > 0)
            .then(|| Duration::from_secs(args.metrics_interval)),
        experimental_full_signon: args.experimental_signon,
        ..ServerConfig::default()
    };
    cfg.game_dir = args.game_dir.clone();
    if let Some(dir) = &args.game_dir {
        match std::fs::read_to_string(dir.join("steam.inf")) {
            Ok(text) => {
                let inf = steaminf::parse(&text);
                if let Some(v) = inf.get("PatchVersion") {
                    cfg.version = v.clone();
                }
                if let Some(id) = inf.get("appID").and_then(|s| s.parse().ok()) {
                    cfg.app_id = id;
                }
            }
            Err(e) => warn!(dir = %dir.display(), error = %e, "could not read steam.inf"),
        }
        match std::fs::read_to_string(dir.join("garrysmod.ver")) {
            Ok(text) => cfg.gmod_build = text.lines().next().and_then(|l| l.trim().parse().ok()),
            Err(e) => warn!(dir = %dir.display(), error = %e, "could not read garrysmod.ver"),
        }
    }

    let mut server = match Server::bind(cfg.clone()) {
        Ok(s) => s,
        Err(e) => {
            error!(addr = %cfg.bind_addr(), error = %e, "failed to bind UDP socket");
            return ExitCode::FAILURE;
        }
    };
    let shutdown = server.shutdown_handle();
    let console_shutdown = shutdown.clone();
    if let Err(e) = ctrlc::set_handler(move || shutdown.request()) {
        warn!(error = %e, "could not install Ctrl+C handler");
    }
    spawn_console(console_shutdown);
    info!(
        addr = %server.local_addr().map(|a| a.to_string()).unwrap_or_default(),
        map = %cfg.map,
        max_players = cfg.max_players,
        version = %cfg.version,
        "srcds-rs listening"
    );
    match server.run() {
        Ok(_) => {
            info!("clean shutdown");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "server loop failed");
            ExitCode::FAILURE
        }
    }
}

/// Minimal server console on stdin. EOF (no console attached) is not treated as
/// a quit request, so the server can run detached.
fn spawn_console(shutdown: srcds_core::ShutdownHandle) {
    let spawned = std::thread::Builder::new()
        .name("console".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut line = String::new();
            loop {
                line.clear();
                match stdin.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                match line.trim() {
                    "" => {}
                    "quit" | "exit" => {
                        info!("console: quit");
                        shutdown.request();
                        return;
                    }
                    other => warn!(command = other, "unknown console command (supported: quit)"),
                }
            }
        });
    if let Err(e) = spawned {
        warn!(error = %e, "could not start console thread");
    }
}
