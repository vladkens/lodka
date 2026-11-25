// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! lodka's folders, by the XDG Base Directory spec, the files in them, and the VM's settings.

use std::ffi::OsString;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::{env, fmt, fs, io};

use crate::bad;

pub const GIB: u64 = 1 << 30;

/// The VM's settings: ~/.config/lodka/lodka.conf.
pub fn config_dir() -> PathBuf {
  xdg("XDG_CONFIG_HOME", ".config")
}

/// The VM's disk and lodka's SSH key.
pub fn data_dir() -> PathBuf {
  xdg("XDG_DATA_HOME", ".local/share")
}

/// What the running VM writes: logs, its lock, its address, the Docker socket.
pub fn state_dir() -> PathBuf {
  xdg("XDG_STATE_HOME", ".local/state")
}

fn xdg(var: &str, default: &str) -> PathBuf {
  let home = PathBuf::from(env::var_os("HOME").unwrap_or_default());
  base(env::var_os(var), &home, default).join("lodka")
}

/// The folder an XDG variable names, or its default in the home folder when it is unset, empty
/// or not absolute, which the spec says to ignore.
fn base(var: Option<OsString>, home: &Path, default: &str) -> PathBuf {
  match var.map(PathBuf::from) {
    Some(x) if x.is_absolute() => x,
    _ => home.join(default),
  }
}

/// Makes a folder of lodka's, readable by its owner only.
pub fn mkdir(dir: &Path) -> io::Result<()> {
  fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

pub fn pid_file() -> PathBuf {
  state_dir().join("lodka.pid")
}

pub fn log_file() -> PathBuf {
  state_dir().join("lodka.log")
}

pub fn console_file() -> PathBuf {
  state_dir().join("console.log")
}

/// Removes what a VM that no longer runs leaves behind: its address, the Docker socket, its pid.
pub fn clear_state() {
  for x in [ip_file(), docker_sock(), pid_file()] {
    let _ = fs::remove_file(x);
  }
}

/// The guest's address, written once its DHCP lease is known.
pub fn ip_file() -> PathBuf {
  state_dir().join("ip")
}

pub fn docker_sock() -> PathBuf {
  state_dir().join("docker.sock")
}

pub fn disk_file() -> PathBuf {
  data_dir().join("data.img")
}

pub fn key_file() -> PathBuf {
  data_dir().join("id_ed25519")
}

/// The guest's kernel (`Image`) and root file system (`rootfs.img`): from $LODKA_GUEST, next
/// to an installed binary (../share/lodka), or in a checkout (target/<profile>/../../guest/out).
pub fn guest_dir() -> io::Result<PathBuf> {
  if let Some(x) = env::var_os("LODKA_GUEST") {
    return Ok(PathBuf::from(x));
  }

  let exe = env::current_exe()?.canonicalize()?;
  let bin = exe.parent().unwrap_or(Path::new("/"));
  [bin.join("../share/lodka"), bin.join("../../guest/out")]
    .into_iter()
    .find(|x| x.join("Image").exists() && x.join("rootfs.img").exists())
    .ok_or_else(|| {
      bad("no guest files (Image, rootfs.img): build them with make images or set LODKA_GUEST")
    })
}

// MARK: Settings

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
  pub cpus: u32,
  /// GiB
  pub memory: u32,
  /// GiB, the most the data disk grows to; it takes only what the guest writes.
  pub disk: u32,
}

impl Default for Settings {
  /// All the Mac's CPUs (an idle vCPU costs nothing), half its memory up to 8 GiB.
  fn default() -> Settings {
    let cpus = sysctl_u64(c"hw.ncpu").unwrap_or(4) as u32;
    let memory = (sysctl_u64(c"hw.memsize").unwrap_or(8 * GIB) / GIB / 2).clamp(2, 8) as u32;
    Settings { cpus, memory, disk: 64 }
  }
}

impl Settings {
  fn path() -> PathBuf {
    config_dir().join("lodka.conf")
  }

  /// The saved settings, defaults for what is not saved.
  pub fn load() -> io::Result<Settings> {
    match fs::read_to_string(Settings::path()) {
      Ok(x) => Settings::parse(&x),
      Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
      Err(e) => Err(e),
    }
  }

  fn parse(text: &str) -> io::Result<Settings> {
    let mut s = Settings::default();
    for line in text.lines().map(str::trim).filter(|x| !x.is_empty() && !x.starts_with('#')) {
      let (key, value) = line.split_once('=').ok_or_else(|| bad(format!("lodka.conf: {line}")))?;
      let value: u32 = match value.trim().parse() {
        Ok(0) | Err(_) => return Err(bad(format!("lodka.conf: not a positive number: {line}"))),
        Ok(x) => x,
      };

      match key.trim() {
        "cpus" => s.cpus = value,
        "memory" => s.memory = value,
        "disk" => s.disk = value,
        x => return Err(bad(format!("lodka.conf: unknown setting {x}"))),
      }
    }

    Ok(s)
  }

  pub fn save(&self) -> io::Result<()> {
    mkdir(&config_dir())?;
    fs::write(Settings::path(), self.to_string())
  }
}

impl fmt::Display for Settings {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    writeln!(f, "# lodka's VM; memory and disk in GiB")?;
    writeln!(f, "cpus = {}\nmemory = {}\ndisk = {}", self.cpus, self.memory, self.disk)
  }
}

fn sysctl_u64(name: &std::ffi::CStr) -> Option<u64> {
  let mut buf = [0u8; 8];
  let mut len = buf.len();
  // SAFETY: the buffer holds `len` bytes; sysctl writes at most that.
  let rc = unsafe {
    libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0)
  };
  match (rc, len) {
    (0, 4) => Some(u32::from_ne_bytes(buf[..4].try_into().unwrap()) as u64),
    (0, 8) => Some(u64::from_ne_bytes(buf)),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn xdg_base() {
    let home = Path::new("/Users/me");
    assert_eq!(base(None, home, ".config"), Path::new("/Users/me/.config"));
    assert_eq!(base(Some("".into()), home, ".config"), Path::new("/Users/me/.config"));
    assert_eq!(base(Some("rel".into()), home, ".config"), Path::new("/Users/me/.config"));
    assert_eq!(base(Some("/x".into()), home, ".config"), Path::new("/x"));
  }

  #[test]
  fn settings() {
    let s = Settings::parse("# c\ncpus = 3\n\nmemory=6\n").unwrap();
    assert_eq!((s.cpus, s.memory, s.disk), (3, 6, 64));
    for x in ["cpus", "cpus = 0", "cpus = x", "swap = 1"] {
      assert!(Settings::parse(x).is_err(), "{x}");
    }

    let s = Settings { cpus: 2, memory: 6, disk: 20 };
    assert_eq!(Settings::parse(&s.to_string()).unwrap(), s);
  }

  #[test]
  fn host_defaults() {
    let s = Settings::default();
    assert!(s.cpus >= 1 && (2..=8).contains(&s.memory));
  }
}
