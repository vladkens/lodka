// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! lodka: a small, fast Docker VM for Apple silicon Macs. Its own VMM, `lodka-vmm`, runs a small
//! Linux with Docker and SSH; the Mac reaches it over a vmnet network, Docker's socket over
//! vsock.

mod args;
mod cli;
mod context;
mod dirs;
mod ports;
mod vm;

use std::io;
use std::process::ExitCode;

use args::Cmd;

fn main() -> ExitCode {
  let res = Cmd::parse(std::env::args().skip(1)).and_then(run);
  match res {
    Ok(()) => ExitCode::SUCCESS,
    Err(e) => {
      eprintln!("lodka: {e}");
      ExitCode::FAILURE
    }
  }
}

fn run(cmd: Cmd) -> io::Result<()> {
  match cmd {
    Cmd::Help => println!("{}", args::HELP),
    Cmd::Version => println!("lodka {}", env!("CARGO_PKG_VERSION")),
    Cmd::Start { cpus, memory, disk } => cli::start(cpus, memory, disk)?,
    Cmd::Stop { force } => cli::stop(force)?,
    Cmd::Status => cli::status()?,
    Cmd::Ssh { cmd } => cli::ssh(&cmd)?,
    Cmd::Delete { force } => cli::delete(force)?,
    Cmd::Vm => vm::serve()?,
  }

  Ok(())
}

pub fn bad(msg: impl Into<String>) -> io::Error {
  io::Error::other(msg.into())
}
