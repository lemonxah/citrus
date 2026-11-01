//! `zestengine-relay`: the deployable relay server.
//!
//! ```text
//! zestengine-relay [--bind 0.0.0.0:47800] [--max-rooms 4096] [--max-members 64] [--idle 30]
//!              [--token SECRET] [--rooms-per-ip 8] [--max-pending 16] [--max-pending-total 1024]
//!              [--orphan-grace 30]
//! ```

use std::sync::atomic::AtomicBool;

use zestengine_relay::{Relay, RelayConfig, proto};

const USAGE: &str = "zestengine-relay [--bind ADDR] [--max-rooms N] [--max-members N] [--idle SECONDS]
             [--token SECRET] [--rooms-per-ip N] [--max-pending N] [--max-pending-total N]
             [--orphan-grace SECONDS]

  --token SECRET        hosts must know this operator token to open rooms
                        (also ZESTENGINE_RELAY_TOKEN); joining needs no token
  --rooms-per-ip N      rooms one IP address may hold at once (default 8)
  --max-pending N       joiners per room that haven't exchanged data yet (default 16)
  --max-pending-total N the same over the whole relay (default 1024)
  --orphan-grace S      seconds a room stays open after its host left, for a
                        successor to take it over (host migration, default 30)";

fn main() {
    let mut bind = format!("0.0.0.0:{}", proto::DEFAULT_PORT);
    let mut cfg = RelayConfig { token: std::env::var("ZESTENGINE_RELAY_TOKEN").ok().filter(|t| !t.is_empty()), ..Default::default() };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |name: &str| args.next().unwrap_or_else(|| fail(&format!("{name} needs a value")));
        match a.as_str() {
            "--bind" => bind = value("--bind"),
            "--max-rooms" => cfg.max_rooms = number(&value("--max-rooms")),
            "--max-members" => cfg.max_members = number(&value("--max-members")),
            "--idle" => cfg.idle_timeout = number::<u32>(&value("--idle")) as f64,
            "--token" => cfg.token = Some(value("--token")).filter(|t| !t.is_empty()),
            "--rooms-per-ip" => cfg.rooms_per_ip = number(&value("--rooms-per-ip")),
            "--max-pending" => cfg.max_pending = number(&value("--max-pending")),
            "--max-pending-total" => cfg.max_pending_total = number(&value("--max-pending-total")),
            "--orphan-grace" => cfg.orphan_grace = number::<u32>(&value("--orphan-grace")) as f64,
            "-h" | "--help" => {
                println!("{USAGE}");
                println!("Rendezvous + relay server for zestengine internet sessions (UDP, default port {}).", proto::DEFAULT_PORT);
                return;
            }
            other => fail(&format!("unknown argument {other:?} (try --help)")),
        }
    }
    let mut relay = match Relay::bind(&bind, cfg.clone()) {
        Ok(r) => r,
        Err(e) => fail(&format!("bind {bind}: {e}")),
    };
    println!(
        "zestengine-relay listening on {} (protocol {}{})",
        relay.local_addr().map(|a| a.to_string()).unwrap_or(bind),
        proto::RELAY_PROTOCOL,
        if cfg.token.is_some() { ", operator token required to open rooms" } else { "" }
    );
    let stop = AtomicBool::new(false);
    if let Err(e) = relay.run(&stop) {
        fail(&format!("relay stopped: {e}"));
    }
}

fn number<T: std::str::FromStr>(s: &str) -> T {
    s.parse().unwrap_or_else(|_| fail(&format!("not a number: {s:?}")))
}

fn fail(msg: &str) -> ! {
    eprintln!("zestengine-relay: {msg}");
    std::process::exit(2)
}
