// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The wire between lodka on the Mac and lodka-guest in its VM: vsock ports and control lines.

use std::time::{SystemTime, UNIX_EPOCH};

/// The guest dials this at boot; control lines go both ways over it.
pub const CONTROL_PORT: u32 = 1024;
/// The host dials this for each Docker API connection; the guest joins it to dockerd's socket.
pub const DOCKER_PORT: u32 = 1025;
/// The host dials this for each connection to a forwarded port, and first sends the TCP port
/// as a line; the guest joins it to that port on its loopback.
pub const FORWARD_PORT: u32 = 1026;

/// One line on the control channel. Host to guest: `Time`, `Key`, `Poweroff`; guest to
/// host: `Hello`, `Addr`, `Docker`, `Ports`.
#[derive(Debug, Clone, PartialEq)]
pub enum Ctl {
  /// The Mac's clock, as seconds and nanoseconds since the epoch.
  Time {
    secs: u64,
    nanos: u32,
  },
  /// An `authorized_keys` line for root's SSH login.
  Key(String),
  Poweroff,
  Hello {
    version: String,
  },
  /// The guest's IPv4 address on the shared network, once DHCP gave it one.
  Addr(String),
  /// dockerd answers on its socket.
  Docker,
  /// The TCP ports the guest listens on for every address, sorted.
  Ports(Vec<u16>),
}

impl Ctl {
  /// A `Time` with the clock now.
  pub fn now() -> Ctl {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    Ctl::Time { secs: t.as_secs(), nanos: t.subsec_nanos() }
  }

  pub fn parse(line: &str) -> Option<Ctl> {
    let line = line.trim_end();
    let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
    let rep = match cmd {
      "time" => {
        let (secs, nanos) = arg.split_once('.')?;
        Ctl::Time { secs: secs.parse().ok()?, nanos: nanos.parse().ok()? }
      }
      "key" => Ctl::Key(arg.to_string()),
      "poweroff" => Ctl::Poweroff,
      "hello" => Ctl::Hello { version: arg.to_string() },
      "addr" if !arg.is_empty() => Ctl::Addr(arg.to_string()),
      "docker" => Ctl::Docker,
      "ports" => Ctl::Ports(arg.split_whitespace().map(|x| x.parse().ok()).collect::<Option<_>>()?),
      _ => return None,
    };

    Some(rep)
  }
}

impl std::fmt::Display for Ctl {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Ctl::Time { secs, nanos } => write!(f, "time {secs}.{nanos:09}"),
      Ctl::Key(key) => write!(f, "key {key}"),
      Ctl::Poweroff => write!(f, "poweroff"),
      Ctl::Hello { version } => write!(f, "hello {version}"),
      Ctl::Addr(addr) => write!(f, "addr {addr}"),
      Ctl::Docker => write!(f, "docker"),
      Ctl::Ports(ports) => {
        let ports: Vec<_> = ports.iter().map(|x| x.to_string()).collect();
        write!(f, "ports {}", ports.join(" "))
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ctl_lines_round_trip() {
    let cases = [
      Ctl::Time { secs: 1_700_000_000, nanos: 5 },
      Ctl::Key("ssh-ed25519 AAAA lodka".into()),
      Ctl::Poweroff,
      Ctl::Hello { version: "0.1.0".into() },
      Ctl::Addr("192.168.64.2".into()),
      Ctl::Docker,
      Ctl::Ports(vec![80, 443, 8080]),
      Ctl::Ports(vec![]),
    ];
    for x in cases {
      assert_eq!(Ctl::parse(&format!("{x}\n")), Some(x));
    }
  }

  #[test]
  fn ctl_rejects_garbage() {
    for x in ["", "nope", "addr", "time 12", "ports 80 http"] {
      assert_eq!(Ctl::parse(x), None, "{x}");
    }
  }
}
