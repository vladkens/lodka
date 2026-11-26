// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The process that runs the VM (`lodka _vm`): makes the disk and the network on first use,
//! boots the guest, serves Docker's socket and the control channel, and powers the guest off
//! cleanly on SIGTERM.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, thread};

use lodka_proto::{CONTROL_PORT, Ctl, DOCKER_PORT};
use lodka_vmm::blk::Blk;
use lodka_vmm::console::{Console, Rng};
use lodka_vmm::machine::{Config, Machine, Stop, Stopper};
use lodka_vmm::net::Net;
use lodka_vmm::vsock::{self, Connector, Listener, Vsock};

use crate::bad;
use crate::dirs::{self, GIB, Settings, mkdir};

/// How long the guest gets to power off after SIGTERM before the VM is stopped.
const POWEROFF_WAIT: Duration = Duration::from_secs(15);
/// How often the guest's clock is set from the Mac's; it stands still while the Mac sleeps.
const TIME_EVERY: Duration = Duration::from_secs(30);

pub fn serve() -> io::Result<()> {
  let t0 = Instant::now();
  let signals = block_signals();
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
  let net = Net::start()?;
  let mac = net.mac.map(|x| format!("{x:02x}")).join(":");
  eprintln!("lodka: {} cpus, {} GiB memory, mac {mac}", s.cpus, s.memory);

  let ctl = Arc::new(Control { out: Mutex::new(None), t0 });
  let mut listeners: HashMap<u32, Listener> = HashMap::new();
  let c = ctl.clone();
  listeners.insert(
    CONTROL_PORT,
    Box::new(move || {
      let (a, b) = vsock::pair()?;
      c.attach(a)?;
      Ok(b)
    }),
  );

  let vsock = Vsock::new(listeners)?;
  let conn = vsock.connector();
  // In this order the guest names them: hvc0, then /dev/vda (root) and /dev/vdb (data).
  m.add(Box::new(Console::new(console)?));
  m.add(Box::new(Rng::new()?));
  let queues = s.cpus as usize;
  m.add(Box::new(Blk::new(File::open(guest.join("rootfs.img"))?, true, "lodka-root", 1)?));
  m.add(Box::new(Blk::new(disk, false, "lodka-data", queues)?));
  m.add(Box::new(net));
  m.add(Box::new(vsock));

  docker(conn)?;
  let c = ctl.clone();
  thread::Builder::new().name("time".into()).spawn(move || {
    loop {
      thread::sleep(TIME_EVERY);
      c.send(&Ctl::now());
    }
  })?;
  watch_signals(signals, ctl, m.stopper())?;

  let stop = m.run()?;
  let _ = fs::remove_file(dirs::docker_sock());

  match stop {
    Stop::Poweroff => eprintln!("lodka: powered off"),
    Stop::Reboot => eprintln!("lodka: the guest restarted, stopped"),
    Stop::Error(e) => return Err(bad(e)),
  }

  Ok(())
}

// MARK: Files

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

// MARK: Docker

/// Docker's socket on the Mac: each connection goes to dockerd's socket in the guest.
fn docker(conn: Connector) -> io::Result<()> {
  let path = dirs::docker_sock();
  let _ = fs::remove_file(&path);
  let l = UnixListener::bind(&path)?;
  thread::Builder::new().name("docker".into()).spawn(move || {
    for x in l.incoming() {
      match x {
        Ok(x) => conn.connect(DOCKER_PORT, x),
        Err(e) => eprintln!("lodka: docker socket: {e}"),
      }
    }
  })?;

  Ok(())
}

// MARK: Control

struct Control {
  /// The guest's end of the current control connection. One the guest closed stays until the
  /// next replaces it: sends to it fail.
  out: Mutex<Option<UnixStream>>,
  t0: Instant,
}

impl Control {
  /// Sends a line to the guest; false when no guest is connected.
  fn send(&self, msg: &Ctl) -> bool {
    let mut out = self.out.lock().unwrap();
    match out.as_mut() {
      Some(x) => writeln!(x, "{msg}").is_ok(),
      None => false,
    }
  }

  /// Makes a connection the guest opens the current one and serves it on a thread of its own.
  /// The one before is shut, which ends its thread: one connection is open at a time, however
  /// many the guest opens.
  fn attach(self: &Arc<Self>, stream: UnixStream) -> io::Result<()> {
    let out = stream.try_clone()?;
    if let Some(old) = self.out.lock().unwrap().replace(out) {
      let _ = old.shutdown(Shutdown::Both);
    }

    let c = self.clone();
    thread::Builder::new().name("control".into()).spawn(move || {
      if let Err(e) = c.serve(stream) {
        eprintln!("lodka: control: {e}");
      }
    })?;
    Ok(())
  }

  fn serve(&self, stream: UnixStream) -> io::Result<()> {
    self.send(&Ctl::now());

    let secs = || self.t0.elapsed().as_secs_f32();
    for line in BufReader::new(stream).lines() {
      let line = line?;
      match Ctl::parse(&line) {
        Some(Ctl::Hello { version }) => eprintln!("lodka: guest {version} up in {:.2}s", secs()),
        Some(Ctl::Addr(ip)) => eprintln!("lodka: guest address {ip}"),
        Some(Ctl::Docker) => eprintln!("lodka: docker up in {:.2}s", secs()),
        _ => eprintln!("lodka: control: unexpected {line:?}"),
      }
    }

    Ok(())
  }
}

// MARK: Signals

/// Blocks the signals that stop the VM in every thread made after it, so `watch_signals` gets
/// them all.
fn block_signals() -> libc::sigset_t {
  // SAFETY: plain signal set calls on a local set.
  unsafe {
    let mut set: libc::sigset_t = std::mem::zeroed();
    libc::sigemptyset(&mut set);
    for x in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
      libc::sigaddset(&mut set, x);
    }

    libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    set
  }
}

/// The first signal asks the guest to power off and stops the VM if it hasn't in a while; a
/// second one stops it at once.
fn watch_signals(set: libc::sigset_t, ctl: Arc<Control>, stopper: Stopper) -> io::Result<()> {
  let run = move || {
    let mut asked = false;
    loop {
      let mut sig = 0;
      // SAFETY: waits for one of the signals in `set`, blocked in every thread.
      unsafe { libc::sigwait(&set, &mut sig) };
      if asked || !ctl.send(&Ctl::Poweroff) {
        stopper.stop(Stop::Poweroff);
        continue;
      }

      asked = true;
      let stopper = stopper.clone();
      thread::spawn(move || {
        thread::sleep(POWEROFF_WAIT);
        eprintln!("lodka: the guest did not power off, stopping it");
        stopper.stop(Stop::Poweroff);
      });
    }
  };

  thread::Builder::new().name("signals".into()).spawn(run)?;
  Ok(())
}
