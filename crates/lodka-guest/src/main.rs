// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! PID 1 of lodka's VM: mounts the file systems (a read-only erofs root, the data disk at
//! /var/lib), starts the network, sshd and dockerd and keeps them running, reaps orphans, and
//! talks to lodka on the Mac over vsock. Trims its page cache when idle, and powers the VM off
//! when lodka asks.

mod agent;
mod sys;
mod trim;

use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use std::{fs, io, thread};

use sys::mount;

const DATA_DEV: &str = "/dev/vdb";
const HOST_KEY: &str = "/var/lib/lodka/ssh/ssh_host_ed25519_key";
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Set once the VM powers off: services that exit are no longer started again.
pub static STOPPING: AtomicBool = AtomicBool::new(false);
/// How long containers get to stop when the VM powers off; lodka waits a bit longer.
const STOP_WAIT: Duration = Duration::from_secs(12);

macro_rules! log {
  ($($arg:tt)*) => { eprintln!("lodka-guest: {}", format!($($arg)*)) };
}
pub(crate) use log;

fn main() {
  if std::process::id() != 1 {
    log!("runs as the VM's init only");
    std::process::exit(2);
  }

  // SAFETY: setting the environment before any thread starts.
  unsafe { std::env::set_var("PATH", PATH) };
  watch_signals();
  if let Err(e) = boot() {
    log!("boot failed: {e}");
    thread::sleep(Duration::from_secs(1));
    sys::poweroff(false);
  }

  for &(name, cmd, files) in SERVICES {
    start(name, cmd, files);
  }

  agent::start();
  trim::start();
  reap();
}

// MARK: Boot

const NOSUID: libc::c_ulong = libc::MS_NOSUID;
const NODEV: libc::c_ulong = libc::MS_NODEV;
const NOEXEC: libc::c_ulong = libc::MS_NOEXEC;

/// The kernel's file systems and the ones in memory: source, target, type, flags, options.
const MOUNTS: &[(&str, &str, &str, libc::c_ulong, &str)] = &[
  ("proc", "/proc", "proc", NOSUID | NODEV | NOEXEC, ""),
  ("sysfs", "/sys", "sysfs", NOSUID | NODEV | NOEXEC, ""),
  ("cgroup2", "/sys/fs/cgroup", "cgroup2", NOSUID | NODEV | NOEXEC, "nsdelegate"),
  ("devpts", "/dev/pts", "devpts", NOSUID | NOEXEC, "newinstance,ptmxmode=0666,mode=0620,gid=5"),
  ("shm", "/dev/shm", "tmpfs", NOSUID | NODEV, "mode=1777"),
  ("tmpfs", "/run", "tmpfs", NOSUID | NODEV, "mode=0755"),
  ("tmpfs", "/tmp", "tmpfs", NOSUID | NODEV, "mode=1777"),
  ("tmpfs", "/opt", "tmpfs", NOSUID | NODEV, "mode=0755"), // containerd wants /opt/containerd
];

fn boot() -> io::Result<()> {
  let _ = mount("devtmpfs", "/dev", "devtmpfs", NOSUID, "mode=0755"); // the kernel may have mounted it
  for &(src, dst, kind, flags, data) in MOUNTS {
    mount(src, dst, kind, flags, data)?;
  }

  for (link, to) in [("fd", ""), ("stdin", "/0"), ("stdout", "/1"), ("stderr", "/2")] {
    let _ = symlink(format!("/proc/self/fd{to}"), format!("/dev/{link}"));
  }

  // /etc stays writable over the read-only root, in memory: resolv.conf, hosts, keys.
  for x in ["/run/lodka/etc/upper", "/run/lodka/etc/work"] {
    fs::create_dir_all(x)?;
  }

  mount(
    "overlay",
    "/etc",
    "overlay",
    0,
    "lowerdir=/etc,upperdir=/run/lodka/etc/upper,workdir=/run/lodka/etc/work",
  )?;

  sys::sethostname("lodka")?;
  fs::write("/etc/hostname", "lodka\n")?;
  let hosts = fs::read_to_string("/etc/hosts").unwrap_or_default();
  fs::write("/etc/hosts", format!("{hosts}127.0.1.1\tlodka\n"))?;

  for (key, value) in SYSCTLS {
    if let Err(e) = fs::write(format!("/proc/sys/{}", key.replace('.', "/")), value) {
      log!("sysctl {key}: {e}");
    }
  }

  var()?;
  shares();
  host_key()?;
  run("ip", &["link", "set", "lo", "up"])?;
  run("ip", &["link", "set", "eth0", "up"])?;
  Ok(())
}

/// /var in memory, with the data disk at /var/lib: Docker's images, containers and volumes,
/// the SSH host key and root's home. The disk gets a file system on first boot and grows with
/// its device.
fn var() -> io::Result<()> {
  mount("tmpfs", "/var", "tmpfs", NOSUID | NODEV, "mode=0755")?;
  for x in ["lib", "log", "cache", "spool", "empty"] {
    fs::create_dir_all(format!("/var/{x}"))?;
  }

  fs::create_dir_all("/var/tmp")?;
  fs::set_permissions("/var/tmp", fs::Permissions::from_mode(0o1777))?;
  fs::create_dir_all("/run/lock")?;
  symlink("/run", "/var/run")?;
  symlink("/run/lock", "/var/lock")?;

  if !sys::is_ext4(DATA_DEV)? {
    if !blank_disk() {
      let msg = "the data disk has data but no ext4 file system; lodka delete makes a new disk";
      return Err(io::Error::other(msg));
    }

    log!("making the data disk's file system");
    // Lazy init: the disk is a sparse file, zeroes cost nothing to read back.
    let opts = "lazy_itable_init=1,lazy_journal_init=1,nodiscard";
    run("mkfs.ext4", &["-q", "-F", "-m", "0", "-L", "lodka-data", "-E", opts, DATA_DEV])?;
  }

  // discard: space Docker frees goes back to the Mac at once.
  mount(DATA_DEV, "/var/lib", "ext4", libc::MS_NOATIME, "discard")?;
  if let Err(e) = sys::grow_ext4("/var/lib", DATA_DEV) {
    log!("{e}");
  }

  // root's home keeps what a person leaves there.
  fs::create_dir_all("/var/lib/lodka/root")?;
  mount("/var/lib/lodka/root", "/root", "", libc::MS_BIND, "")
}

/// The Mac's folders lodka shares, each at the path its tag names (`/Users`): virtio-fs devices
/// list their tags in sysfs. Each file shows whoever asks as its owner, and macOS checks access
/// as the Mac user (`caller_owns`, guest/patches); no setuid file there runs as root.
/// Readahead goes past the kernel's 128 KiB, which halves a sequential read's speed: up to
/// four requests of the largest size, 1 MiB, in flight.
fn shares() {
  let Ok(dirs) = fs::read_dir("/sys/fs/virtiofs") else { return };
  for x in dirs.flatten() {
    let Ok(tag) = fs::read_to_string(x.path().join("tag")) else { continue };
    let tag = tag.trim();
    if !tag.starts_with('/') {
      continue;
    }

    if let Err(e) = mount(tag, tag, "virtiofs", NOSUID | NODEV, "caller_owns") {
      log!("{tag}: {e}");
      continue;
    }

    let Ok(dev) = fs::metadata(tag).map(|x| x.dev()) else { continue };
    let bdi = format!("/sys/class/bdi/{}:{}/read_ahead_kb", libc::major(dev), libc::minor(dev));
    if let Err(e) = fs::write(&bdi, "4096") {
      log!("{bdi}: {e}");
    }
  }
}

/// Whether lodka says the data disk is blank, none of it on the Mac yet: a disk only gets a file
/// system then, as one with a damaged file system may still hold data.
fn blank_disk() -> bool {
  let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
  cmdline.split_whitespace().any(|x| x == "lodka.blank")
}

/// sshd's host key, made on the first boot and synced at once: a VM stopped before the guest
/// wrote it back left it empty, and sshd down on every boot after.
fn host_key() -> io::Result<()> {
  if fs::metadata(HOST_KEY).is_ok_and(|x| x.len() > 0) {
    return Ok(());
  }

  let public = format!("{HOST_KEY}.pub");
  fs::create_dir_all("/var/lib/lodka/ssh")?;
  let _ = fs::remove_file(HOST_KEY);
  let _ = fs::remove_file(&public);
  run("ssh-keygen", &["-q", "-t", "ed25519", "-N", "", "-f", HOST_KEY])?;
  for x in [HOST_KEY, &public] {
    fs::File::open(x)?.sync_all()?;
  }

  Ok(())
}

/// Runs a command to its end; only while booting, before the reaper takes every exit.
fn run(cmd: &str, args: &[&str]) -> io::Result<()> {
  let st = Command::new(cmd).args(args).stdin(Stdio::null()).status()?;
  match st.success() {
    true => Ok(()),
    false => Err(io::Error::other(format!("{cmd}: {st}"))),
  }
}

const SYSCTLS: &[(&str, &str)] = &[
  ("fs.inotify.max_user_watches", "1048576"),
  ("fs.inotify.max_user_instances", "8192"),
  ("net.core.somaxconn", "4096"),
  ("net.ipv4.ip_forward", "1"),
  // Some networks drop full-size packets without telling (seen on a link with a 1420 MTU):
  // TCP lowers its segment size once it sees them lost.
  ("net.ipv4.tcp_mtu_probing", "1"),
  ("vm.max_map_count", "1048576"),
  // Compaction moves pages into memory given back to the Mac, which takes it again; it runs
  // once after a trim instead (trim.rs).
  ("vm.compaction_proactiveness", "0"),
  ("kernel.panic", "-1"),
];

// MARK: Services

/// The services running now: pid and name.
static RUNNING: Mutex<Vec<(u32, &str)>> = Mutex::new(Vec::new());

/// Name, command line, and the open files it may have when more than the default.
const SERVICES: &[(&str, &[&str], Option<u64>)] = &[
  (
    "udhcpc",
    &["udhcpc", "-f", "-i", "eth0", "-s", "/usr/lib/lodka/udhcpc.sh", "-x", "hostname:lodka"],
    None,
  ),
  ("sshd", &["/usr/sbin/sshd", "-D", "-e"], None),
  ("dockerd", &["dockerd", "--log-level", "warn"], Some(1 << 20)),
];

fn start(name: &'static str, cmd: &[&str], files: Option<u64>) {
  let mut c = Command::new(cmd[0]);
  c.args(&cmd[1..]).stdin(Stdio::null());
  // SAFETY: between fork and exec, calls that touch only the child: it gets no signals blocked
  // (PID 1 blocks those it waits for, and a child keeps the mask) and its open files' limit.
  unsafe {
    c.pre_exec(move || {
      let mut set: libc::sigset_t = std::mem::zeroed();
      libc::sigemptyset(&mut set);
      libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
      if let Some(n) = files {
        let lim = libc::rlimit { rlim_cur: n, rlim_max: n };
        libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
      }

      Ok(())
    })
  };

  // Under the lock, so the reaper can't take the child for a stranger before it is listed, and
  // a shutdown, which sets STOPPING before it looks, sees every service started.
  let mut list = RUNNING.lock().unwrap();
  if STOPPING.load(Ordering::Acquire) {
    return;
  }

  match c.spawn() {
    Ok(x) => list.push((x.id(), name)),
    Err(e) => {
      log!("{name}: {e}, starting it again");
      again(name);
    }
  }
}

/// Starts a service again in a second.
fn again(name: &'static str) {
  thread::spawn(move || {
    thread::sleep(Duration::from_secs(1));
    if let Some(&(_, cmd, files)) = SERVICES.iter().find(|x| x.0 == name) {
      start(name, cmd, files);
    }
  });
}

/// Waits for every process that ends, as PID 1 must; starts a service again a second after it
/// exits.
fn reap() -> ! {
  loop {
    let mut status = 0;
    // SAFETY: waits for any child.
    let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
    if pid <= 0 {
      thread::sleep(Duration::from_millis(100));
      continue;
    }

    let mut list = RUNNING.lock().unwrap();
    let Some(i) = list.iter().position(|x| x.0 == pid as u32) else { continue };
    let (_, name) = list.remove(i);
    drop(list);
    if STOPPING.load(Ordering::Acquire) {
      continue;
    }

    log!("{name} exited ({status:#x}), starting it again");
    again(name);
  }
}

/// busybox's reboot, halt and poweroff ask init with SIGTERM, SIGUSR1 and SIGUSR2. Blocked here
/// before any other thread starts, so in all of them: the kernel then queues them for PID 1,
/// which it otherwise spares the signals it has no handler for.
fn watch_signals() {
  // SAFETY: plain signal set calls on a local set.
  let set = unsafe {
    let mut set: libc::sigset_t = std::mem::zeroed();
    libc::sigemptyset(&mut set);
    for x in [libc::SIGTERM, libc::SIGUSR1, libc::SIGUSR2] {
      libc::sigaddset(&mut set, x);
    }

    libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    set
  };

  thread::spawn(move || {
    let mut sig = 0;
    // SAFETY: waits for one of the signals in `set`, blocked in every thread.
    unsafe { libc::sigwait(&set, &mut sig) };
    shutdown(sig == libc::SIGTERM);
  });
}

/// Stops every process, gives them STOP_WAIT to exit, then powers the VM off or restarts it.
/// dockerd goes first, alone: it stops the containers, then containerd and its shims, which
/// outlive a SIGTERM of their own.
pub fn shutdown(restart: bool) -> ! {
  STOPPING.store(true, Ordering::Release);
  log!("{}", if restart { "rebooting" } else { "powering off" });
  let t0 = Instant::now();
  let running = |name| RUNNING.lock().unwrap().iter().find(|x| x.1 == name).map(|x| x.0);
  if let Some(pid) = running("dockerd") {
    // SAFETY: a signal to a child of ours, not reaped yet.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
  }

  while t0.elapsed() < STOP_WAIT && running("dockerd").is_some() {
    thread::sleep(Duration::from_millis(50));
  }

  // SAFETY: signals every process but PID 1.
  unsafe { libc::kill(-1, libc::SIGTERM) };
  while t0.elapsed() < STOP_WAIT && !sys::others().is_empty() {
    thread::sleep(Duration::from_millis(50));
  }

  let left = sys::others();
  if !left.is_empty() {
    log!("still running after {}s, killed: {}", STOP_WAIT.as_secs(), left.join(", "));
  }

  // SAFETY: as above.
  unsafe { libc::kill(-1, libc::SIGKILL) };
  thread::sleep(Duration::from_millis(50));
  if let Err(e) = sys::umount("/var/lib") {
    log!("{e}");
  }

  sys::poweroff(restart)
}
