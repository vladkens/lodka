// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The process that runs the VM (`lodka _vm`, started by `lodka start`): makes the disk, the SSH
//! key and the network on first use, boots the guest, serves Docker's socket and the control
//! channel, forwards the guest's ports to the Mac's localhost, and powers the guest off cleanly
//! on SIGTERM.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use std::{env, thread};

use lodka_proto::{CONTROL_PORT, Ctl, DOCKER_PORT, FORWARD_PORT};

use crate::bad;
use crate::blk::Blk;
use crate::console::{Console, Rng};
use crate::dirs::{self, GIB, Settings, mkdir};
use crate::machine::{Config, Machine, Stop, Stopper};
use crate::net::Net;
use crate::ports::{Dial, Ports};
use crate::vsock::{self, Connector, Listener, Vsock};

/// How long the guest gets to power off after SIGTERM before the VM is stopped.
const POWEROFF_WAIT: Duration = Duration::from_secs(15);
/// How often the guest's clock is set from the Mac's; it stands still while the Mac sleeps.
const TIME_EVERY: Duration = Duration::from_secs(30);

pub fn serve() -> io::Result<()> {
  let t0 = Instant::now();
  let signals = block_signals();
  let lock = lock()?;
  // A VM that didn't stop cleanly left its address behind; this one gets a new network.
  dirs::clear_state();
  mkdir(&dirs::data_dir())?;

  // Saved only by the VM that got the lock; a reboot's VM loads them.
  let s = match env::var(dirs::SETTINGS_VAR) {
    Ok(x) => {
      let s = Settings::parse(&x)?;
      s.save()?;
      s
    }
    Err(_) => Settings::load()?,
  };

  let guest = dirs::guest_dir()?;
  let disk = disk(&s)?;
  let key = key()?;
  let console = Arc::new(Mutex::new(File::create(dirs::console_file())?));

  let mut cmdline =
    "console=hvc0 root=/dev/vda ro rootfstype=erofs init=/sbin/lodka-guest panic=-1".to_string();
  match env::var_os("LODKA_DEBUG") {
    Some(_) => cmdline += " earlycon=pl011,0x09000000 loglevel=7",
    None => cmdline += " quiet",
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

  let (ctl_tx, ctl_rx) = mpsc::channel();
  let mut listeners: HashMap<u32, Listener> = HashMap::new();
  listeners.insert(
    CONTROL_PORT,
    Box::new(move || {
      let (a, b) = vsock::pair()?;
      ctl_tx.send(a).map_err(|_| bad("control channel closed"))?;
      Ok(b)
    }),
  );

  let vsock = Vsock::new(listeners)?;
  let conn = vsock.connector();
  // In this order the guest names them: hvc0, then /dev/vda (root) and /dev/vdb (data).
  m.add(Box::new(Console::new(console)?));
  m.add(Box::new(Rng::new()?));
  let queues = s.cpus as usize;
  m.add(Box::new(Blk::open(&guest.join("rootfs.img"), true, "lodka-root", 1)?));
  m.add(Box::new(Blk::open(&disk, false, "lodka-data", queues)?));
  m.add(Box::new(net));
  m.add(Box::new(vsock));

  let ctl = Arc::new(Control { out: Mutex::new(None), key, t0 });
  docker(conn.clone())?;
  let (c, dial) = (ctl.clone(), dial(conn));
  thread::Builder::new().name("control".into()).spawn(move || c.run(ctl_rx, dial))?;
  let c = ctl.clone();
  thread::Builder::new().name("time".into()).spawn(move || {
    loop {
      thread::sleep(TIME_EVERY);
      c.send(&Ctl::now());
    }
  })?;
  watch_signals(signals, ctl, m.stopper())?;

  let stop = m.run()?;
  dirs::clear_state();

  drop(lock);
  match stop {
    Stop::Poweroff => eprintln!("lodka: powered off"),
    Stop::Reboot => {
      eprintln!("lodka: rebooting");
      let mut c = Command::new(env::current_exe()?);
      c.arg("_vm").env_remove(dirs::SETTINGS_VAR).stdin(Stdio::null()).spawn()?;
    }
    Stop::Error(e) => return Err(bad(e)),
  }

  Ok(())
}

// MARK: Files

/// Takes the VM's lock, held as long as the process runs, and writes the pid into its file for
/// people; lodka asks the lock.
fn lock() -> io::Result<File> {
  let mut f = dirs::lock()?.ok_or_else(|| bad("the VM runs already"))?;
  f.set_len(0)?;
  writeln!(f, "{}", std::process::id())?;
  Ok(f)
}

/// The data disk, sparse: it takes on the Mac only what the guest writes. It grows when the
/// setting does (the guest grows its file system at boot), never shrinks.
fn disk(s: &Settings) -> io::Result<PathBuf> {
  let path = dirs::disk_file();
  let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
  let (have, want) = (f.metadata()?.len(), s.disk as u64 * GIB);
  if have < want {
    f.set_len(want)?;
  } else if have > want {
    eprintln!(
      "lodka: the disk is {} GiB, more than the {} GiB set: disks don't shrink",
      have / GIB,
      s.disk
    );
  }

  Ok(path)
}

/// lodka's SSH key into the guest, made on first use; returns the public half.
fn key() -> io::Result<String> {
  let path = dirs::key_file();
  let public = path.with_extension("pub");
  if !public.exists() {
    let _ = fs::remove_file(&path);
    let st = Command::new("ssh-keygen")
      .args(["-q", "-t", "ed25519", "-N", "", "-C", "lodka", "-f"])
      .arg(&path)
      .stdin(Stdio::null())
      .status()?;
    if !st.success() {
      return Err(bad(format!("ssh-keygen: {st}")));
    }
  }

  Ok(fs::read_to_string(public)?.trim().to_string())
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

/// Each connection to a forwarded port: a vsock stream to the guest that starts with the port.
fn dial(conn: Connector) -> Dial {
  Arc::new(move |port| {
    let (a, mut b) = vsock::pair()?;
    writeln!(b, "{port}")?;
    conn.connect(FORWARD_PORT, a);
    Ok(b)
  })
}

// MARK: Control

struct Control {
  /// The guest's end of the current control connection.
  out: Mutex<Option<UnixStream>>,
  key: String,
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

  /// Serves each control connection the guest opens, one at a time.
  fn run(&self, conns: mpsc::Receiver<UnixStream>, dial: Dial) {
    let mut ports = Ports::new(dial);
    for x in conns {
      if let Err(e) = self.serve(x, &mut ports) {
        eprintln!("lodka: control: {e}");
      }

      *self.out.lock().unwrap() = None;
    }
  }

  fn serve(&self, stream: UnixStream, ports: &mut Ports) -> io::Result<()> {
    *self.out.lock().unwrap() = Some(stream.try_clone()?);
    self.send(&Ctl::now());
    self.send(&Ctl::Key(self.key.clone()));

    let secs = || self.t0.elapsed().as_secs_f32();
    for line in BufReader::new(stream).lines() {
      let line = line?;
      match Ctl::parse(&line) {
        Some(Ctl::Hello { version }) => eprintln!("lodka: guest {version} up in {:.2}s", secs()),
        Some(Ctl::Addr(ip)) => {
          eprintln!("lodka: guest address {ip}");
          fs::write(dirs::ip_file(), &ip)?;
        }
        Some(Ctl::Docker) => eprintln!("lodka: docker up in {:.2}s", secs()),
        Some(Ctl::Ports(x)) => ports.update(&x),
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
