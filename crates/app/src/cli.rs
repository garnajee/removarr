//! Command line: `serve` (default) and `healthcheck`.
//!
//! `healthcheck` is for the container healthcheck: it does not load the configuration (which
//! would `stat` the roots and could wake disks); it reads only `REMOVARR_BIND` and asks
//! `GET /healthz` on the loopback address with a short timeout.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use removarr_config::{Config, EXIT_CODE_CONFIG};

/// Default port when `REMOVARR_BIND` is unset (architecture 11).
pub const DEFAULT_PORT: u16 = 8080;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Serve,
    Healthcheck,
    Help,
    Version,
}

/// Parses the arguments (program name excluded).
pub fn parse(args: &[String]) -> Result<Command, String> {
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] | ["serve"] => Ok(Command::Serve),
        ["healthcheck"] => Ok(Command::Healthcheck),
        ["-h" | "--help" | "help"] => Ok(Command::Help),
        ["-V" | "--version"] => Ok(Command::Version),
        _ => Err("unknown arguments; use `removarr --help`".to_string()),
    }
}

pub const USAGE: &str = "removarr [serve | healthcheck | --help | --version]\n\
  serve        run the web server and the job runtime (default)\n\
  healthcheck  exit 0 when GET /healthz answers 200 on the local port of REMOVARR_BIND\n";

/// Loopback address of the configured bind (`0.0.0.0:p` and `[::]:p` become loopback).
#[must_use]
pub fn health_target(bind: Option<&str>) -> SocketAddr {
    let parsed = bind.and_then(|b| b.trim().parse::<SocketAddr>().ok());
    let port = parsed.map_or(DEFAULT_PORT, |a| a.port());
    match parsed {
        Some(a) if a.ip().is_loopback() => a,
        Some(SocketAddr::V6(_)) => SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
        _ => SocketAddr::from(([127, 0, 0, 1], port)),
    }
}

/// One `GET /healthz` over plain TCP. True on a `200` status line.
#[must_use]
pub fn healthcheck(target: SocketAddr) -> bool {
    let Ok(mut s) = TcpStream::connect_timeout(&target, HEALTH_TIMEOUT) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(HEALTH_TIMEOUT));
    let _ = s.set_write_timeout(Some(HEALTH_TIMEOUT));
    let req = format!("GET /healthz HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    let mut n = 0;
    while n < 12 {
        match s.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
}

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get().min(4));
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(8)
        .enable_all()
        .build()
}

/// Entry point. Returns the process exit code: 0 ok, 1 startup or health failure, 2 invalid
/// configuration (diagnostics list key names only, never values), 64 bad usage.
#[must_use]
pub fn main(args: &[String]) -> u8 {
    let cmd = match parse(args) {
        Ok(c) => c,
        Err(m) => {
            eprintln!("{m}");
            return 64;
        }
    };
    match cmd {
        Command::Help => {
            eprint!("{USAGE}");
            0
        }
        Command::Version => {
            eprintln!("removarr {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Command::Healthcheck => {
            let bind = std::env::var("REMOVARR_BIND").ok();
            u8::from(!healthcheck(health_target(bind.as_deref())))
        }
        Command::Serve => {
            let cfg = match Config::load_from_process() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return u8::try_from(e.exit_code()).unwrap_or(EXIT_CODE_CONFIG as u8);
                }
            };
            match runtime() {
                Ok(rt) => rt.block_on(crate::serve::run(cfg)),
                Err(_) => {
                    eprintln!("cannot start the async runtime");
                    crate::serve::EXIT_STARTUP
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn arguments_parse_strictly() {
        assert_eq!(parse(&a(&[])), Ok(Command::Serve));
        assert_eq!(parse(&a(&["serve"])), Ok(Command::Serve));
        assert_eq!(parse(&a(&["healthcheck"])), Ok(Command::Healthcheck));
        assert!(parse(&a(&["serve", "--dry-run=false"])).is_err());
        assert!(parse(&a(&["delete"])).is_err());
    }

    #[test]
    fn health_target_is_always_loopback() {
        assert_eq!(health_target(None).to_string(), "127.0.0.1:8080");
        assert_eq!(
            health_target(Some("0.0.0.0:9000")).to_string(),
            "127.0.0.1:9000"
        );
        assert_eq!(health_target(Some("[::]:9001")).to_string(), "[::1]:9001");
        assert_eq!(
            health_target(Some("10.0.0.5:81")).to_string(),
            "127.0.0.1:81"
        );
        assert_eq!(health_target(Some("garbage")).to_string(), "127.0.0.1:8080");
    }

    #[test]
    fn healthcheck_fails_without_a_server() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        assert!(!healthcheck(addr));
    }
}
