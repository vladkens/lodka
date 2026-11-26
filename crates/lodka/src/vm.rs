// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The process that runs the VM (`lodka _vm`): boots the guest's kernel with its console in a
//! log file and runs it until it powers off.

use std::fs::File;
use std::sync::{Arc, Mutex};
use std::{env, io};

use crate::bad;
use crate::dirs::{self, GIB, Settings, mkdir};
use crate::machine::{Config, Machine, Stop};

pub fn serve() -> io::Result<()> {
  mkdir(&dirs::state_dir())?;
  let s = Settings::load()?;
  let guest = dirs::guest_dir()?;
  let console = Arc::new(Mutex::new(File::create(dirs::console_file())?));

  let mut cmdline = "console=ttyAMA0 panic=-1".to_string();
  match env::var_os("LODKA_DEBUG") {
    Some(_) => cmdline += " earlycon=pl011,0x09000000 loglevel=7",
    None => cmdline += " quiet",
  }

  let memory = s.memory as u64 * GIB;
  let cfg = Config { cpus: s.cpus as usize, memory, kernel: guest.join("Image"), cmdline, console };
  let m = Machine::new(cfg)?;
  eprintln!("lodka: {} cpus, {} GiB memory", s.cpus, s.memory);

  match m.run()? {
    Stop::Poweroff => eprintln!("lodka: powered off"),
    Stop::Reboot => eprintln!("lodka: the guest restarted, stopped"),
    Stop::Error(e) => return Err(bad(e)),
  }

  Ok(())
}
