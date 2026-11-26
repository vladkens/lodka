// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The commands a person runs: start, stop, status, ssh, delete. The VM runs in its own process
//! (`lodka _vm`), which holds a lock on its pid file as long as it runs.

use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::bad;
use crate::dirs::{self, GIB, Settings, mkdir};

const START_WAIT: Duration = Duration::from_secs(120);
const STOP_WAIT: Duration = Duration::from_secs(30);
/// How long `ssh` waits for the guest's address: DHCP gives it a moment after Docker is up.
const ADDR_WAIT: Duration = Duration::from_secs(10);

pub fn start(cpus: Option<u32>, memory: Option<u32>, disk: Option<u32>) -> io::Result<()> {
  let mut s = Settings::load()?;
  let old = s.clone();
  s.cpus = cpus.unwrap_or(s.cpus);
  s.memory = memory.unwrap_or(s.memory);
  s.disk = disk.unwrap_or(s.disk);

  if running().is_some() {
    if s != old {
      return Err(bad("the VM runs: stop it to change its sizes"));
    }

    println!("lodka: running already");
    return Ok(());
  }

  dirs::guest_dir()?;
  s.save()?;
  mkdir(&dirs::state_dir())?;
  let exe = signed()?;
  let log = File::create(dirs::log_file())?;
  let mut child = Command::new(exe);
  child.arg("_vm").stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
  // SAFETY: setsid is async-signal-safe; the VM leaves the terminal's session, so closing the
  // terminal doesn't stop it.
  unsafe {
    child.pre_exec(|| {
      libc::setsid();
      Ok(())
    })
  };

  let mut child = child.spawn()?;
  println!("lodka: starting, {} cpus, {} GiB memory, {} GiB disk", s.cpus, s.memory, s.disk);

  let t0 = Instant::now();
  while t0.elapsed() < START_WAIT {
    if let Some(st) = child.try_wait()? {
      let log = fs::read_to_string(dirs::log_file()).unwrap_or_default();
      let tail: Vec<&str> = log.lines().rev().take(10).collect();
      let tail: Vec<&str> = tail.into_iter().rev().collect();
      return Err(bad(format!("the VM stopped ({st}):\n{}", tail.join("\n"))));
    }

    if docker_ping() {
      println!("lodka: running in {:.1}s", t0.elapsed().as_secs_f32());
      println!("export DOCKER_HOST=unix://{}", dirs::docker_sock().display());
      return Ok(());
    }

    thread::sleep(Duration::from_millis(100));
  }

  let log = dirs::log_file();
  Err(bad(format!("Docker did not answer in {}s; see {}", START_WAIT.as_secs(), log.display())))
}

/// Asks the guest to power off; `force` kills the VM, which leaves its address and Docker socket
/// for the next start to remove.
pub fn stop(force: bool) -> io::Result<()> {
  let Some(pid) = running() else {
    println!("lodka: not running");
    return Ok(());
  };

  let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
  // SAFETY: a signal to the VM's process, whose lock proves it is ours and alive.
  if unsafe { libc::kill(pid, sig) } != 0 {
    return Err(io::Error::last_os_error());
  }

  let t0 = Instant::now();
  while running().is_some() {
    if t0.elapsed() > STOP_WAIT {
      return Err(bad(format!("the VM did not stop in {}s; try --force", STOP_WAIT.as_secs())));
    }

    thread::sleep(Duration::from_millis(50));
  }

  println!("lodka: stopped");
  Ok(())
}

pub fn status() -> io::Result<()> {
  let s = Settings::load()?;
  let disk = fs::metadata(dirs::disk_file()).map(|x| x.blocks() * 512).unwrap_or(0);
  let Some(pid) = running() else {
    println!("stopped");
    println!("sizes: {} cpus, {} GiB memory, {} GiB disk", s.cpus, s.memory, s.disk);
    println!("disk: {:.1} GiB used", disk as f64 / GIB as f64);
    return Ok(());
  };

  let ip = fs::read_to_string(dirs::ip_file()).unwrap_or_else(|_| "unknown yet".into());
  println!("running, pid {pid}, docker {}", if docker_ping() { "up" } else { "not up yet" });
  println!("sizes: {} cpus, {} GiB memory, {} GiB disk", s.cpus, s.memory, s.disk);
  println!("disk: {:.1} GiB used", disk as f64 / GIB as f64);
  println!("address: {ip}");
  println!("docker: unix://{}", dirs::docker_sock().display());
  Ok(())
}

pub fn ssh(cmd: &[String]) -> io::Result<()> {
  if running().is_none() {
    return Err(bad("not running: lodka start"));
  }

  let t0 = Instant::now();
  let ip = loop {
    match fs::read_to_string(dirs::ip_file()) {
      Ok(x) => break x,
      Err(_) if t0.elapsed() < ADDR_WAIT => thread::sleep(Duration::from_millis(100)),
      Err(_) => return Err(bad("the VM has no address on the network")),
    }
  };

  let mut c = Command::new("ssh");
  c.arg("-i").arg(dirs::key_file());
  for x in [
    "StrictHostKeyChecking=no",
    "UserKnownHostsFile=/dev/null",
    "LogLevel=ERROR",
    "IdentitiesOnly=yes",
  ] {
    c.args(["-o", x]);
  }

  if cmd.is_empty() {
    c.arg("-t");
  }

  c.arg(format!("root@{}", ip.trim()));
  if !cmd.is_empty() {
    c.arg("--").args(cmd);
  }

  Err(c.exec())
}

pub fn delete(force: bool) -> io::Result<()> {
  if !force {
    if !io::stdin().is_terminal() {
      return Err(bad("delete: this deletes all images, containers and volumes; add --force"));
    }

    print!("Delete the VM's disk with all images, containers and volumes? [y/N] ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    if !matches!(line.trim(), "y" | "Y" | "yes") {
      return Ok(());
    }
  }

  if running().is_some() {
    stop(false)?;
  }

  match fs::remove_dir_all(dirs::data_dir()) {
    Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
    _ => {}
  }

  println!("lodka: deleted");
  Ok(())
}

// MARK: Helpers

const ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>com.apple.security.hypervisor</key><true/>
<key>com.apple.security.virtualization</key><true/>
</dict></plist>
"#;

/// The VM's process needs Hypervisor.framework (the hypervisor entitlement) and a vmnet
/// network without root (the virtualization one): lodka signs itself ad hoc with both the first
/// time, a copy signed and then renamed over the binary. Returns the binary's path.
fn signed() -> io::Result<PathBuf> {
  let exe = std::env::current_exe()?.canonicalize()?;
  let out =
    Command::new("codesign").args(["-d", "--entitlements", "-", "--xml"]).arg(&exe).output()?;
  let have = String::from_utf8_lossy(&out.stdout);
  if have.contains("com.apple.security.hypervisor")
    && have.contains("com.apple.security.virtualization")
  {
    return Ok(exe);
  }

  let plist = dirs::state_dir().join("entitlements.plist");
  fs::write(&plist, ENTITLEMENTS)?;
  let copy = exe.with_extension(format!("signing{}", std::process::id()));
  fs::copy(&exe, &copy)?;
  let st = Command::new("codesign")
    .args(["--force", "--sign", "-", "--entitlements"])
    .arg(&plist)
    .arg(&copy)
    .stdout(Stdio::null())
    .status()?;
  let _ = fs::remove_file(&plist);
  if !st.success() {
    let _ = fs::remove_file(&copy);
    return Err(bad(format!("codesign: {st}")));
  }

  fs::rename(&copy, &exe)?;
  eprintln!("lodka: signed {} with the hypervisor and virtualization entitlements", exe.display());
  Ok(exe)
}

/// The VM's pid when its process runs: it holds the lock on the pid file.
fn running() -> Option<i32> {
  let mut f = OpenOptions::new().read(true).open(dirs::pid_file()).ok()?;
  // SAFETY: flock on an open file; a shared lock we get is released when `f` closes.
  if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
    return None;
  }

  let mut pid = String::new();
  f.read_to_string(&mut pid).ok()?;
  pid.trim().parse().ok()
}

/// Whether Docker answers on its socket on the Mac.
fn docker_ping() -> bool {
  let ping = || -> io::Result<bool> {
    let mut s = UnixStream::connect(dirs::docker_sock())?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(b"GET /_ping HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n")?;
    let mut rep = Vec::new();
    s.read_to_end(&mut rep)?;
    Ok(rep.starts_with(b"HTTP/1.1 200"))
  };

  ping().unwrap_or(false)
}
