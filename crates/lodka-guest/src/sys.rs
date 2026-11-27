// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! Thin wrappers over the Linux calls PID 1 needs: mounts, vsock sockets, the clock, addresses.

use std::ffi::CString;
use std::fs;
use std::io::{self, Read};
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

fn cstr(s: &str) -> CString {
  CString::new(s).unwrap_or_default()
}

fn check(rc: i32, what: &str) -> io::Result<()> {
  match rc {
    0 => Ok(()),
    _ => {
      let e = io::Error::last_os_error();
      Err(io::Error::new(e.kind(), format!("{what}: {e}")))
    }
  }
}

pub fn mount(src: &str, dst: &str, kind: &str, flags: libc::c_ulong, data: &str) -> io::Result<()> {
  let _ = fs::create_dir_all(dst);
  let (src_c, dst_c, kind_c, data_c) = (cstr(src), cstr(dst), cstr(kind), cstr(data));
  let data_p = if data.is_empty() { std::ptr::null() } else { data_c.as_ptr().cast() };
  // SAFETY: NUL-terminated strings that outlive the call.
  let rc = unsafe { libc::mount(src_c.as_ptr(), dst_c.as_ptr(), kind_c.as_ptr(), flags, data_p) };
  check(rc, &format!("mount {dst}"))
}

pub fn umount(dst: &str) -> io::Result<()> {
  let dst_c = cstr(dst);
  // SAFETY: a NUL-terminated path.
  check(unsafe { libc::umount2(dst_c.as_ptr(), 0) }, &format!("umount {dst}"))
}

pub fn sethostname(name: &str) -> io::Result<()> {
  // SAFETY: the name's bytes and length.
  check(unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) }, "sethostname")
}

/// Sets the clock when it is off by more than half a second.
pub fn set_time(secs: u64, nanos: u32) -> io::Result<bool> {
  let mut now = libc::timespec { tv_sec: 0, tv_nsec: 0 };
  // SAFETY: fills one timespec.
  unsafe { libc::clock_gettime(libc::CLOCK_REALTIME, &mut now) };
  let off = (now.tv_sec - secs as i64) as f64 + (now.tv_nsec - nanos as i64) as f64 / 1e9;
  if off.abs() < 0.5 {
    return Ok(false);
  }

  let ts = libc::timespec { tv_sec: secs as i64, tv_nsec: nanos as i64 };
  // SAFETY: one valid timespec.
  check(unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) }, "clock_settime")?;
  Ok(true)
}

/// Powers the VM off, or restarts it: PSCI SYSTEM_OFF or SYSTEM_RESET.
pub fn poweroff(restart: bool) -> ! {
  let how = if restart { libc::RB_AUTOBOOT } else { libc::RB_POWER_OFF };
  // SAFETY: flushes the file systems, then ends the VM.
  unsafe {
    libc::sync();
    libc::reboot(how);
  }

  std::process::exit(1)
}

/// The names of the processes other than PID 1. Kernel threads, which kill(-1, 0) counts, have
/// no command line and aren't among them.
pub fn others() -> Vec<String> {
  let Ok(dir) = fs::read_dir("/proc") else { return Vec::new() };
  let user = |x: &fs::DirEntry| {
    let pid = x.file_name().to_str().and_then(|x| x.parse::<u32>().ok());
    pid.is_some_and(|x| x != 1) && fs::read(x.path().join("cmdline")).is_ok_and(|x| !x.is_empty())
  };

  let name = |x: fs::DirEntry| fs::read_to_string(x.path().join("comm")).unwrap_or_default();
  dir.flatten().filter(user).map(|x| name(x).trim().to_string()).collect()
}

/// Whether a block device holds an ext2/3/4 file system: its superblock's magic at 1080.
pub fn is_ext4(dev: &str) -> io::Result<bool> {
  let mut buf = [0u8; 1082];
  fs::File::open(dev)?.read_exact(&mut buf)?;
  Ok(buf[1080..1082] == [0x53, 0xef])
}

/// Grows the ext4 file system mounted at `dir` to fill its device `dev`, online, as resize2fs
/// does: a no-op when it already fills it.
pub fn grow_ext4(dir: &str, dev: &str) -> io::Result<()> {
  const BLKGETSIZE64: u32 = 0x8008_1272; // _IOR(0x12, 114, u64)
  const EXT4_IOC_RESIZE_FS: u32 = 0x4008_6610; // _IOW('f', 16, u64)
  let disk = fs::File::open(dev)?;
  let mut size = 0u64;
  // SAFETY: writes the device's size in bytes to one u64.
  let rc = unsafe { libc::ioctl(disk.as_raw_fd(), BLKGETSIZE64 as libc::Ioctl, &mut size) };
  check(rc, "BLKGETSIZE64")?;

  let dir_c = cstr(dir);
  // SAFETY: an all-zero statvfs is valid; statvfs fills it.
  let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
  // SAFETY: a NUL-terminated path and one statvfs.
  check(unsafe { libc::statvfs(dir_c.as_ptr(), &mut st) }, "statvfs")?;

  let blocks = size / st.f_bsize;
  let root = fs::File::open(dir)?;
  // SAFETY: reads the new block count from one u64.
  let rc = unsafe { libc::ioctl(root.as_raw_fd(), EXT4_IOC_RESIZE_FS as libc::Ioctl, &blocks) };
  check(rc, &format!("grow {dir}"))
}

// MARK: vsock

fn vsock_addr(cid: u32, port: u32) -> libc::sockaddr_vm {
  libc::sockaddr_vm {
    svm_family: libc::AF_VSOCK as libc::sa_family_t,
    svm_reserved1: 0,
    svm_port: port,
    svm_cid: cid,
    svm_zero: [0; 4],
  }
}

fn vsock_socket() -> io::Result<OwnedFd> {
  // SAFETY: a new socket; owned below.
  let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
  if fd < 0 {
    return Err(io::Error::last_os_error());
  }

  // SAFETY: a fresh descriptor nothing else owns.
  Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Dials lodka on the Mac. The stream is a vsock socket in a `UnixStream`, which only reads,
/// writes and shuts down its descriptor.
pub fn vsock_connect(port: u32) -> io::Result<UnixStream> {
  let fd = vsock_socket()?;
  let addr = vsock_addr(libc::VMADDR_CID_HOST, port);
  let len = size_of::<libc::sockaddr_vm>() as libc::socklen_t;
  // SAFETY: a valid sockaddr_vm of `len` bytes.
  let rc = unsafe { libc::connect(fd.as_raw_fd(), (&raw const addr).cast(), len) };
  check(rc, &format!("vsock connect {port}"))?;
  Ok(UnixStream::from(fd))
}

pub struct VsockListener(OwnedFd);

impl VsockListener {
  pub fn bind(port: u32) -> io::Result<VsockListener> {
    let fd = vsock_socket()?;
    let addr = vsock_addr(libc::VMADDR_CID_ANY, port);
    let len = size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    // SAFETY: a valid sockaddr_vm of `len` bytes.
    check(unsafe { libc::bind(fd.as_raw_fd(), (&raw const addr).cast(), len) }, "vsock bind")?;
    // SAFETY: listen on a bound socket.
    check(unsafe { libc::listen(fd.as_raw_fd(), 128) }, "vsock listen")?;
    Ok(VsockListener(fd))
  }

  pub fn accept(&self) -> io::Result<UnixStream> {
    // SAFETY: accept on a listening socket; the peer address isn't wanted.
    let fd = unsafe {
      libc::accept4(
        self.0.as_raw_fd(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        libc::SOCK_CLOEXEC,
      )
    };
    if fd < 0 {
      return Err(io::Error::last_os_error());
    }

    // SAFETY: a fresh descriptor nothing else owns.
    Ok(UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) }))
  }
}

// MARK: Network

/// The IPv4 address of interface `name`, once it has one.
pub fn ipv4(name: &str) -> Option<Ipv4Addr> {
  let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
  // SAFETY: getifaddrs fills a list freed below; each entry is read while it lives.
  unsafe {
    if libc::getifaddrs(&mut list) != 0 {
      return None;
    }

    let mut found = None;
    let mut cur = list;
    while !cur.is_null() {
      let x = &*cur;
      let addr = x.ifa_addr;
      let ours = std::ffi::CStr::from_ptr(x.ifa_name).to_bytes() == name.as_bytes();
      if ours && !addr.is_null() && (*addr).sa_family as i32 == libc::AF_INET {
        let sin = &*(addr as *const libc::sockaddr_in);
        found = Some(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)));
        break;
      }

      cur = x.ifa_next;
    }

    libc::freeifaddrs(list);
    found
  }
}

/// The TCP ports listened on for any address, from /proc/net/tcp and tcp6.
pub fn listening_ports() -> Vec<u16> {
  let mut ports: Vec<u16> = ["/proc/net/tcp", "/proc/net/tcp6"]
    .into_iter()
    .filter_map(|x| fs::read_to_string(x).ok())
    .flat_map(|x| parse_listening(&x))
    .collect();
  ports.sort_unstable();
  ports.dedup();
  ports
}

fn parse_listening(text: &str) -> Vec<u16> {
  let mut ports = Vec::new();
  for line in text.lines().skip(1) {
    let mut cols = line.split_whitespace().skip(1);
    let (Some(local), Some(_), Some(state)) = (cols.next(), cols.next(), cols.next()) else {
      continue;
    };

    let Some((addr, port)) = local.split_once(':') else { continue };
    if state != "0A" || addr.bytes().any(|x| x != b'0') {
      continue; // not LISTEN, or not on every address
    }

    if let Ok(port) = u16::from_str_radix(port, 16) {
      ports.push(port);
    }
  }

  ports
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn listening() {
    let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1
   1: 0100007F:0CEA 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2 1
   2: 0F02000A:0016 0202000A:C350 01 00000000:00000000 00:00000000 00000000     0        0 3 1";
    assert_eq!(parse_listening(tcp), [8080]);

    let tcp6 = "  sl  local_address remote_address st
   0: 00000000000000000000000000000000:01BB 00000000000000000000000000000000:0000 0A 0";
    assert_eq!(parse_listening(tcp6), [443]);
  }
}
