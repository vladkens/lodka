// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! Thin wrappers over the Linux calls PID 1 needs: mounts, the data disk, power.

use std::ffi::CString;
use std::fs;
use std::io::{self, Read};
use std::os::fd::AsRawFd;

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

pub fn sethostname(name: &str) -> io::Result<()> {
  // SAFETY: the name's bytes and length.
  check(unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) }, "sethostname")
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

/// Whether a block device holds an ext2/3/4 file system: its superblock's magic at 1080.
pub fn is_ext4(dev: &str) -> io::Result<bool> {
  let mut buf = [0u8; 1082];
  fs::File::open(dev)?.read_exact(&mut buf)?;
  Ok(buf[1080..1082] == [0x53, 0xef])
}

/// Whether nothing was ever written at the start of the device: a new disk, safe to format.
pub fn is_blank(dev: &str) -> io::Result<bool> {
  let mut buf = vec![0u8; 64 * 1024];
  fs::File::open(dev)?.read_exact(&mut buf)?;
  Ok(buf.iter().all(|&x| x == 0))
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
