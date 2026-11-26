// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The process that runs the VM (`lodka _vm`): makes the data disk on first use, boots the guest
//! from its root file system with its console in a log file, and runs it until it powers off.

use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Mutex};

use lodka_vmm::blk::Blk;
use lodka_vmm::console::{Console, Rng};
use lodka_vmm::machine::{Config, Machine, Stop};

use crate::bad;
use crate::dirs::{self, GIB, Settings, mkdir};

pub fn serve() -> io::Result<()> {
  let _lock = lock()?;
  mkdir(&dirs::data_dir())?;
  let s = Settings::load()?;
  let guest = dirs::guest_dir()?;
  let (disk, blank) = disk(&s)?;
  let console = Arc::new(Mutex::new(File::create(dirs::console_file())?));

  let mut cmdline =
    "console=hvc0 root=/dev/vda ro rootfstype=erofs init=/sbin/lodka-guest panic=-1".to_string();
  match env::var_os("LODKA_DEBUG") {
    Some(_) => cmdline += " earlycon=pl011,0x09000000 loglevel=7",
    None => cmdline += " quiet",
  }

  if blank {
    cmdline += " lodka.blank";
  }

  let memory = s.memory as u64 * GIB;
  let cfg = Config {
    cpus: s.cpus as usize,
    memory,
    kernel: guest.join("Image"),
    cmdline,
    console: console.clone(),
  };
  let mut m = Machine::new(cfg)?;
  eprintln!("lodka: {} cpus, {} GiB memory", s.cpus, s.memory);

  // In this order the guest names them: hvc0, then /dev/vda (root) and /dev/vdb (data).
  m.add(Box::new(Console::new(console)?));
  m.add(Box::new(Rng::new()?));
  let queues = s.cpus as usize;
  m.add(Box::new(Blk::new(File::open(guest.join("rootfs.img"))?, true, "lodka-root", 1)?));
  m.add(Box::new(Blk::new(disk, false, "lodka-data", queues)?));

  match m.run()? {
    Stop::Poweroff => eprintln!("lodka: powered off"),
    Stop::Reboot => eprintln!("lodka: the guest restarted, stopped"),
    Stop::Error(e) => return Err(bad(e)),
  }

  Ok(())
}

/// Takes the VM's lock, held as long as the process runs: one VM on the disk at a time. The pid
/// goes into its file for people.
fn lock() -> io::Result<File> {
  let mut f = dirs::lock()?.ok_or_else(|| bad("the VM runs already"))?;
  f.set_len(0)?;
  writeln!(f, "{}", std::process::id())?;
  Ok(f)
}

/// The data disk, sparse: it takes on the Mac only what the guest writes, and grows with the
/// setting, never shrinks. A flock keeps it this VM's alone, whatever folders another VM has.
/// Also returns whether it is blank: none of it on the Mac yet.
fn disk(s: &Settings) -> io::Result<(File, bool)> {
  let path = dirs::disk_file();
  let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
  // SAFETY: flock on an open file.
  if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
    let e = io::Error::last_os_error();
    return Err(match e.raw_os_error() {
      Some(libc::EWOULDBLOCK) => bad(format!("{}: another VM uses it", path.display())),
      _ => e,
    });
  }

  let meta = f.metadata()?;
  let (have, want, blank) = (meta.len(), s.disk as u64 * GIB, meta.blocks() == 0);
  if have < want {
    f.set_len(want)?;
  } else if have > want {
    eprintln!(
      "lodka: the disk is {} GiB, more than the {} GiB set: disks don't shrink",
      have / GIB,
      s.disk
    );
  }

  Ok((f, blank))
}
