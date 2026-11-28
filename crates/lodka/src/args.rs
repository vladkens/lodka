// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The command line.

use std::io;

use crate::bad;

pub const HELP: &str = "\
lodka: a small, fast Docker VM for Apple silicon Macs

usage:
  lodka start [--cpus <n>] [--memory <GiB>] [--disk <GiB>]
                    start the VM; the sizes are kept for later starts
  lodka stop [--force]
                    power the VM off (--force: kill it)
  lodka status      show whether the VM runs, its address and the Docker socket
  lodka ssh [-- <command>...]
                    a root shell in the VM, or run a command there
  lodka delete [--force]
                    stop the VM and delete its disk, with all images and volumes
  lodka version

Docker: `docker context use lodka`, a context `lodka start` adds, or DOCKER_HOST at the socket
`lodka status` shows. Files, by the XDG Base Directory spec: settings in
~/.config/lodka/lodka.conf (XDG_CONFIG_HOME), the disk and SSH key in ~/.local/share/lodka
(XDG_DATA_HOME), logs and sockets in ~/.local/state/lodka (XDG_STATE_HOME).";

#[derive(Debug, PartialEq)]
pub enum Cmd {
  Help,
  Version,
  Start {
    cpus: Option<u32>,
    memory: Option<u32>,
    disk: Option<u32>,
  },
  Stop {
    force: bool,
  },
  Status,
  Ssh {
    cmd: Vec<String>,
  },
  Delete {
    force: bool,
  },
  /// The process that runs the VM, started by `start`.
  Vm,
}

impl Cmd {
  pub fn parse(args: impl IntoIterator<Item = String>) -> io::Result<Cmd> {
    let mut args = args.into_iter();
    let Some(cmd) = args.next() else { return Ok(Cmd::Help) };
    let mut rest: Vec<String> = args.collect();

    if cmd == "ssh" {
      if rest.first().is_some_and(|x| x == "--") {
        rest.remove(0);
      }

      return Ok(Cmd::Ssh { cmd: rest });
    }

    let mut flags = Vec::new();
    let mut it = rest.into_iter();
    while let Some(x) = it.next() {
      match x.as_str() {
        "--cpus" | "--memory" | "--disk" => {
          let value = it.next().ok_or_else(|| bad(format!("{x}: no value given")))?;
          flags.push((x, Some(value)));
        }
        "--force" | "-f" | "-h" | "--help" => flags.push((x, None)),
        _ if x.starts_with('-') => return Err(bad(format!("unknown flag {x}"))),
        _ => return Err(bad(format!("{cmd}: unexpected argument {x}"))),
      }
    }

    let help = |x: &str| ["help", "-h", "--help"].contains(&x);
    if help(&cmd) || flags.iter().any(|(x, _)| help(x)) {
      return Ok(Cmd::Help);
    }

    let force = flags.iter().any(|(x, _)| x == "--force" || x == "-f");
    let number = |name: &str| -> io::Result<Option<u32>> {
      let Some((_, Some(x))) = flags.iter().find(|(x, _)| x == name) else { return Ok(None) };
      match x.parse() {
        Ok(0) | Err(_) => Err(bad(format!("{name}: not a positive number: {x}"))),
        Ok(n) => Ok(Some(n)),
      }
    };

    let cmd = match cmd.as_str() {
      "-V" | "--version" | "version" => Cmd::Version,
      "start" => {
        Cmd::Start { cpus: number("--cpus")?, memory: number("--memory")?, disk: number("--disk")? }
      }
      "stop" => Cmd::Stop { force },
      "status" => Cmd::Status,
      "delete" => Cmd::Delete { force },
      "_vm" => Cmd::Vm,
      _ => return Err(bad(format!("unknown command {cmd}; see lodka help"))),
    };

    Ok(cmd)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn args(line: &str) -> io::Result<Cmd> {
    Cmd::parse(line.split_whitespace().map(String::from))
  }

  #[test]
  fn commands() {
    assert_eq!(args("").unwrap(), Cmd::Help);
    assert_eq!(args("start --help").unwrap(), Cmd::Help);
    assert_eq!(args("--version").unwrap(), Cmd::Version);
    assert_eq!(args("start").unwrap(), Cmd::Start { cpus: None, memory: None, disk: None });
    assert_eq!(
      args("start --cpus 4 --memory 8 --disk 100").unwrap(),
      Cmd::Start { cpus: Some(4), memory: Some(8), disk: Some(100) }
    );
    assert_eq!(args("stop -f").unwrap(), Cmd::Stop { force: true });
    assert_eq!(args("delete").unwrap(), Cmd::Delete { force: false });
    assert_eq!(args("ssh").unwrap(), Cmd::Ssh { cmd: vec![] });
    assert_eq!(args("ssh -- ls -la").unwrap(), Cmd::Ssh { cmd: vec!["ls".into(), "-la".into()] });
    assert_eq!(args("ssh uname -a").unwrap(), Cmd::Ssh { cmd: vec!["uname".into(), "-a".into()] });
  }

  #[test]
  fn errors() {
    for x in
      ["nope", "start --cpus", "start --cpus 0", "start --memory lots", "stop now", "stop --x"]
    {
      assert!(args(x).is_err(), "{x}");
    }
  }
}
