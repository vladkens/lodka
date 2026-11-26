// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! lodka: a small, fast Docker VM for Apple silicon Macs. Its own VMM on Hypervisor.framework
//! boots the guest's Linux kernel.

mod dirs;
mod fdt;
mod hv;
mod machine;
mod mem;
mod vm;

use std::io;
use std::process::ExitCode;

fn main() -> ExitCode {
  let res = match std::env::args().nth(1).as_deref() {
    Some("_vm") => vm::serve(),
    _ => Err(bad("usage: lodka _vm, which runs the VM in this process")),
  };

  match res {
    Ok(()) => ExitCode::SUCCESS,
    Err(e) => {
      eprintln!("lodka: {e}");
      ExitCode::FAILURE
    }
  }
}

pub fn bad(msg: impl Into<String>) -> io::Error {
  io::Error::other(msg.into())
}
