// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! PID 1 of lodka's VM: mounts the file systems (a read-only erofs root, the data disk at
//! /var/lib) and reaps orphans.

mod sys;

use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::{Command, Stdio};
use std::time::Duration;
use std::{fs, io, thread};

use sys::mount;

const DATA_DEV: &str = "/dev/vdb";
const PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

macro_rules! log {
  ($($arg:tt)*) => { eprintln!("lodka-guest: {}", format!($($arg)*)) };
}

fn main() {
  if std::process::id() != 1 {
    log!("runs as the VM's init only");
    std::process::exit(2);
  }

  // SAFETY: setting the environment before any thread starts.
  unsafe { std::env::set_var("PATH", PATH) };
  if let Err(e) = boot() {
    log!("boot failed: {e}");
    thread::sleep(Duration::from_secs(1));
    sys::poweroff(false);
  }

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
  ("devpts", "/dev/pts", "devpts", NOSUID | NOEXEC, "newinstance,ptmxmode=0666,mode=0620,gid=5"),
  ("shm", "/dev/shm", "tmpfs", NOSUID | NODEV, "mode=1777"),
  ("tmpfs", "/run", "tmpfs", NOSUID | NODEV, "mode=0755"),
  ("tmpfs", "/tmp", "tmpfs", NOSUID | NODEV, "mode=1777"),
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

  var()
}

/// /var in memory, with the data disk at /var/lib: what the guest keeps across boots, root's
/// home among it. The disk gets a file system on first boot and grows with its device.
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

/// Whether lodka says the data disk is blank, none of it on the Mac yet: a disk only gets a file
/// system then, as one with a damaged file system may still hold data.
fn blank_disk() -> bool {
  let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
  cmdline.split_whitespace().any(|x| x == "lodka.blank")
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
  ("vm.max_map_count", "1048576"),
  ("kernel.panic", "-1"),
];

/// Waits for every process that ends, as PID 1 must.
fn reap() -> ! {
  loop {
    let mut status = 0;
    // SAFETY: waits for any child.
    let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
    if pid <= 0 {
      thread::sleep(Duration::from_millis(100));
    }
  }
}
