// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The process that runs the VM (`lodka _vm`, started by `lodka start`): makes the disk, the SSH
//! key and the network on first use, boots the guest, serves Docker's socket and the control
//! channel, forwards the guest's ports to the Mac's localhost, and powers the guest off cleanly
//! on SIGTERM.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, thread};

use lodka_proto::{CONTROL_PORT, Ctl, DOCKER_PORT, FORWARD_PORT};
use lodka_vmm::balloon::Balloon;
use lodka_vmm::blk::Blk;
use lodka_vmm::console::{Console, Rng};
use lodka_vmm::fs::Fs;
use lodka_vmm::machine::{Config, Machine, Stop, Stopper};
use lodka_vmm::net::Net;
use lodka_vmm::vsock::{self, Connector, Listener, Vsock};

use crate::bad;
use crate::dirs::{self, GIB, Settings, mkdir};
use crate::ports::{Dial, Ports};

/// How long the guest gets to power off after SIGTERM before the VM is stopped.
const POWEROFF_WAIT: Duration = Duration::from_secs(15);
/// How often the guest's clock is set from the Mac's; it stands still while the Mac sleeps.
const TIME_EVERY: Duration = Duration::from_secs(30);
/// How long a control line may wait for the guest to take it.
const SEND_WAIT: Duration = Duration::from_secs(1);
/// The VM's lock, which a rebooted VM leaves open for the next one in its process.
const LOCK_FD_VAR: &str = "LODKA_LOCK_FD";

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
  let (disk, blank) = disk(&s)?;
  let key = key()?;
  let console = Arc::new(Mutex::new(File::create(dirs::console_file())?));

  let mut cmdline =
    "console=hvc0 root=/dev/vda ro rootfstype=erofs init=/sbin/lodka-guest panic=-1".to_string();
  // Freed memory reported in blocks from 16 KiB, the Mac's page, up: the 2 MiB default leaves
  // page cache freed page by page in the guest.
  cmdline += " page_reporting.page_reporting_order=2";
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

  let ctl = Arc::new(Control {
    cur: Mutex::default(),
    opened: AtomicU64::new(0),
    ports: Mutex::new(None),
    key,
    t0,
  });
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
  // The Mac user's folders at the same path, so `docker run -v "$PWD:/app"` works from them.
  m.add(Box::new(Fs::new("/Users", Path::new("/Users"), queues)?));
  // What the guest frees goes back to the Mac.
  m.add(Box::new(Balloon::new()?));

  *ctl.ports.lock().unwrap() = Some(Ports::new(dial(conn.clone())));
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
  dirs::clear_state();

  match stop {
    Stop::Poweroff => eprintln!("lodka: powered off"),
    Stop::Reboot => {
      eprintln!("lodka: rebooting");
      return Err(reboot(&lock));
    }
    Stop::Error(e) => return Err(bad(e)),
  }

  Ok(())
}

/// Runs the next VM in this process with the lock kept open across exec, so it holds on: no
/// `stop` or `delete` gets in between. Returns only when exec fails.
fn reboot(lock: &File) -> io::Error {
  let fd = lock.as_raw_fd();
  // SAFETY: clears close-on-exec on a descriptor of ours.
  if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0 {
    return io::Error::last_os_error();
  }

  let exe = match env::current_exe() {
    Ok(x) => x,
    Err(e) => return e,
  };

  let mut c = Command::new(exe);
  c.arg("_vm").env_remove(dirs::SETTINGS_VAR).env(LOCK_FD_VAR, fd.to_string()).exec()
}

// MARK: Files

/// Takes the VM's lock, held as long as the process runs, and writes the pid into its file for
/// people; lodka asks the lock.
fn lock() -> io::Result<File> {
  let mut f = match env::var(LOCK_FD_VAR).ok().and_then(|x| x.parse().ok()) {
    // SAFETY: the descriptor the VM before this one in the process left open, with its lock.
    Some(fd) => unsafe { File::from_raw_fd(fd) },
    None => dirs::lock()?.ok_or_else(|| bad("the VM runs already"))?,
  };

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
  if !dirs::lock_disk(&f)? {
    return Err(bad(format!("{}: another VM uses it", path.display())));
  }

  // `lodka delete` may have removed the file between the open and the lock.
  let meta = f.metadata()?;
  if fs::metadata(&path).map(|x| x.ino()).ok() != Some(meta.ino()) {
    return Err(bad(format!("{}: deleted as the VM started", path.display())));
  }

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

/// Each connection to a forwarded port: a vsock stream to the guest that starts with the Mac's
/// address it came to.
fn dial(conn: Connector) -> Dial {
  Arc::new(move |addr| {
    let (a, mut b) = vsock::pair()?;
    writeln!(b, "{addr}")?;
    conn.connect(FORWARD_PORT, a);
    Ok(b)
  })
}

// MARK: Control

struct Control {
  cur: Mutex<Current>,
  /// How many connections the guest opened: each one's number, in the order it opened them.
  opened: AtomicU64,
  /// The guest's ports on the Mac, kept across connections; set before the guest boots, once
  /// the vsock device that dials them is made.
  ports: Mutex<Option<Ports>>,
  key: String,
  t0: Instant,
}

/// The current control connection: its number, and the guest's end until a send to it fails.
#[derive(Default)]
struct Current {
  id: u64,
  out: Option<UnixStream>,
}

impl Control {
  /// Sends a line to the guest; false when no guest is connected. A guest that doesn't take it
  /// within SEND_WAIT loses the connection, half a line and all, and dials again.
  fn send(&self, msg: &Ctl) -> bool {
    let mut cur = self.cur.lock().unwrap();
    let Some(x) = cur.out.as_mut() else { return false };
    if writeln!(x, "{msg}").is_ok() {
      return true;
    }

    let _ = x.shutdown(Shutdown::Both);
    cur.out = None;
    false
  }

  /// Serves a connection the guest opens on a thread of its own, as the current one. The one
  /// before is shut, which ends its thread: one is open at a time, however many the guest
  /// opens. The vsock device's thread calls this, so it never waits for the lock.
  fn attach(self: &Arc<Self>, stream: UnixStream) -> io::Result<()> {
    let id = self.opened.fetch_add(1, Ordering::Relaxed) + 1;
    let c = self.clone();
    thread::Builder::new().name("control".into()).spawn(move || {
      if let Err(e) = c.serve(id, stream) {
        eprintln!("lodka: control: {e}");
      }
    })?;
    Ok(())
  }

  fn serve(&self, id: u64, stream: UnixStream) -> io::Result<()> {
    let out = stream.try_clone()?;
    out.set_write_timeout(Some(SEND_WAIT))?;
    let mut cur = self.cur.lock().unwrap();
    if id < cur.id {
      return Ok(()); // a later one got here first; this one closes as it drops
    }

    if let Some(old) = cur.out.replace(out) {
      let _ = old.shutdown(Shutdown::Both);
    }

    cur.id = id;
    drop(cur);

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
        Some(Ctl::Ports(x)) => {
          if let Some(p) = self.ports.lock().unwrap().as_mut() {
            p.update(&x);
          }
        }
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
