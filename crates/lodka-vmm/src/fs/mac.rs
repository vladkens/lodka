// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The macOS side of a shared folder: a file's attributes and its name as stored, from
//! `getattrlist`, a directory's entries with theirs from `getattrlistbulk`, a file's path from
//! its inode, and Linux's numbers for macOS's errors and flags.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::RawFd;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFLNK: u32 = 0o120000;
pub const S_IFREG: u32 = 0o100000;

/// A file's attributes: macOS and Linux share the values of the type and permission bits.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stat {
  pub dev: i32,
  pub ino: u64,
  pub mode: u32,
  pub nlink: u32,
  pub uid: u32,
  pub gid: u32,
  pub rdev: u32,
  pub size: u64,
  pub blocks: u64,
  pub blksize: u32,
  pub atime: (i64, u32),
  pub mtime: (i64, u32),
  pub ctime: (i64, u32),
}

impl Stat {
  pub fn kind(&self) -> u32 {
    self.mode & S_IFMT
  }

  pub fn is_dir(&self) -> bool {
    self.kind() == S_IFDIR
  }
}

impl From<&libc::stat> for Stat {
  fn from(st: &libc::stat) -> Stat {
    Stat {
      dev: st.st_dev,
      ino: st.st_ino,
      mode: st.st_mode as u32,
      nlink: st.st_nlink as u32,
      uid: st.st_uid,
      gid: st.st_gid,
      rdev: st.st_rdev as u32,
      size: st.st_size as u64,
      blocks: st.st_blocks as u64,
      blksize: st.st_blksize as u32,
      atime: (st.st_atime, st.st_atime_nsec as u32),
      mtime: (st.st_mtime, st.st_mtime_nsec as u32),
      ctime: (st.st_ctime, st.st_ctime_nsec as u32),
    }
  }
}

pub fn fstat(fd: RawFd) -> io::Result<Stat> {
  // SAFETY: a plain C struct, valid zeroed.
  let mut st: libc::stat = unsafe { std::mem::zeroed() };
  // SAFETY: fstat fills one stat.
  if unsafe { libc::fstat(fd, &mut st) } != 0 {
    return Err(io::Error::last_os_error());
  }

  Ok(Stat::from(&st))
}

/// The attributes of a symlink itself, not of what it points to.
pub fn lstat(path: &CStr) -> io::Result<Stat> {
  // SAFETY: as in `fstat`.
  let mut st: libc::stat = unsafe { std::mem::zeroed() };
  // SAFETY: lstat fills one stat from a NUL-terminated path.
  if unsafe { libc::lstat(path.as_ptr(), &mut st) } != 0 {
    return Err(io::Error::last_os_error());
  }

  Ok(Stat::from(&st))
}

// MARK: Attributes

/// sys/attr.h: a bulk listing's per-entry error, which libc lacks.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const CMN: u32 = libc::ATTR_CMN_RETURNED_ATTRS
  | libc::ATTR_CMN_NAME
  | libc::ATTR_CMN_DEVID
  | libc::ATTR_CMN_OBJTYPE
  | libc::ATTR_CMN_MODTIME
  | libc::ATTR_CMN_CHGTIME
  | libc::ATTR_CMN_ACCTIME
  | libc::ATTR_CMN_OWNERID
  | libc::ATTR_CMN_GRPID
  | libc::ATTR_CMN_ACCESSMASK
  | libc::ATTR_CMN_FILEID;
const DIR: u32 = libc::ATTR_DIR_ENTRYCOUNT
  | libc::ATTR_DIR_ALLOCSIZE
  | libc::ATTR_DIR_IOBLOCKSIZE
  | libc::ATTR_DIR_DATALENGTH;
const FILE: u32 = libc::ATTR_FILE_LINKCOUNT
  | libc::ATTR_FILE_ALLOCSIZE
  | libc::ATTR_FILE_IOBLOCKSIZE
  | libc::ATTR_FILE_DEVTYPE
  | libc::ATTR_FILE_DATALENGTH;

/// A name with its attributes, as `getattrlist` returns them.
pub type Entry = (Vec<u8>, Stat);

fn attrlist(bulk: bool) -> libc::attrlist {
  let err = if bulk { ATTR_CMN_ERROR } else { 0 };
  libc::attrlist {
    bitmapcount: libc::ATTR_BIT_MAP_COUNT,
    reserved: 0,
    commonattr: CMN | err,
    volattr: 0,
    dirattr: DIR,
    fileattr: FILE,
    forkattr: 0,
  }
}

/// The attributes of `name` in the directory `dir`, a symlink itself and not what it points
/// to, with the name as stored: on a case-insensitive volume `readme.md` finds `README.md`.
pub fn stat_at(dir: RawFd, name: &CStr) -> io::Result<Entry> {
  let al = attrlist(false);
  // Aligned, and room for the fixed attributes and the longest name: 255 UTF-16 units, each
  // up to 3 bytes of UTF-8.
  let mut buf = [0u64; 256];
  let size = size_of_val(&buf);
  let opts = libc::FSOPT_NOFOLLOW as u64;
  // SAFETY: getattrlistat writes at most the buffer's size into it.
  let r = unsafe {
    libc::getattrlistat(
      dir,
      name.as_ptr(),
      (&al as *const libc::attrlist).cast_mut().cast(),
      buf.as_mut_ptr().cast(),
      size,
      opts,
    )
  };
  if r != 0 {
    return Err(io::Error::last_os_error());
  }

  // SAFETY: a u64 buffer seen as its bytes.
  let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), size) };
  parse(bytes)
}

/// The entries of the directory open as `fd`, without `.` and `..`, from its start.
pub fn list(fd: RawFd) -> io::Result<Vec<Entry>> {
  // SAFETY: lseek on an open directory: getattrlistbulk starts again only from 0.
  if unsafe { libc::lseek(fd, 0, libc::SEEK_SET) } < 0 {
    return Err(io::Error::last_os_error());
  }

  let al = attrlist(true);
  let mut buf = vec![0u64; 16 * 1024]; // 128 KiB: about 800 entries a call
  let size = buf.len() * 8;
  let mut out = Vec::new();
  loop {
    // SAFETY: getattrlistbulk writes at most `size` bytes into the buffer.
    let n = unsafe {
      libc::getattrlistbulk(
        fd,
        (&al as *const libc::attrlist).cast_mut().cast(),
        buf.as_mut_ptr().cast(),
        size,
        0,
      )
    };
    if n < 0 {
      return Err(io::Error::last_os_error());
    }

    if n == 0 {
      return Ok(out);
    }

    // SAFETY: as in stat_at.
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), size) };
    let mut at = 0;
    for _ in 0..n {
      let len = Reader { rec: &bytes[at..], at: 0 }.u32() as usize;
      if len < 4 || at + len > size {
        return Err(io::Error::other("getattrlistbulk: a record past the buffer"));
      }

      match parse(&bytes[at..at + len]) {
        Ok(x) => out.push(x),
        // Gone between the listing and its attributes.
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {}
        Err(e) => return Err(e),
      }

      at += len;
    }
  }
}

/// Reads a record's fields in the order of their bits, each present only when the returned
/// bitmap says so (man getattrlist).
fn parse(rec: &[u8]) -> io::Result<Entry> {
  let mut r = Reader { rec, at: 4 };
  let set: [u32; 5] = std::array::from_fn(|_| r.u32());
  let (cmn, dir, file) = (set[0], set[2], set[3]);
  let has = |bits: u32, bit: u32| bits & bit != 0;
  // ATTR_CMN_ERROR comes right after the bitmap, out of its bit's order.
  if has(cmn, ATTR_CMN_ERROR) {
    let e = r.u32();
    if e != 0 {
      return Err(io::Error::from_raw_os_error(e as i32));
    }
  }

  let mut name = Vec::new();
  let mut st = Stat::default();
  if has(cmn, libc::ATTR_CMN_NAME) {
    let at = r.at;
    let (off, len) = (r.u32() as i32 as isize, r.u32() as usize);
    let from = (at as isize + off) as usize;
    let bytes = rec.get(from..from + len).unwrap_or_default();
    name = bytes.strip_suffix(&[0]).unwrap_or(bytes).to_vec();
  }

  if has(cmn, libc::ATTR_CMN_DEVID) {
    st.dev = r.u32() as i32;
  }

  let mut kind = 0;
  if has(cmn, libc::ATTR_CMN_OBJTYPE) {
    kind = vtype(r.u32());
  }

  if has(cmn, libc::ATTR_CMN_MODTIME) {
    st.mtime = r.time();
  }

  if has(cmn, libc::ATTR_CMN_CHGTIME) {
    st.ctime = r.time();
  }

  if has(cmn, libc::ATTR_CMN_ACCTIME) {
    st.atime = r.time();
  }

  if has(cmn, libc::ATTR_CMN_OWNERID) {
    st.uid = r.u32();
  }

  if has(cmn, libc::ATTR_CMN_GRPID) {
    st.gid = r.u32();
  }

  if has(cmn, libc::ATTR_CMN_ACCESSMASK) {
    st.mode = kind | (r.u32() & 0o7777);
  }

  if has(cmn, libc::ATTR_CMN_FILEID) {
    st.ino = r.u64();
  }

  // Directory attributes come for directories only, file attributes for the rest.
  st.blksize = 4096;
  if has(dir, libc::ATTR_DIR_ENTRYCOUNT) {
    // As lstat counts on APFS: the entries, `.` and `..`; ATTR_DIR_LINKCOUNT is always 1.
    st.nlink = r.u32() + 2;
  }

  if has(dir, libc::ATTR_DIR_ALLOCSIZE) {
    st.blocks = r.u64() / 512;
  }

  if has(dir, libc::ATTR_DIR_IOBLOCKSIZE) {
    st.blksize = r.u32();
  }

  if has(dir, libc::ATTR_DIR_DATALENGTH) {
    st.size = r.u64();
  }

  if has(file, libc::ATTR_FILE_LINKCOUNT) {
    st.nlink = r.u32();
  }

  if has(file, libc::ATTR_FILE_ALLOCSIZE) {
    st.blocks = r.u64() / 512;
  }

  if has(file, libc::ATTR_FILE_IOBLOCKSIZE) {
    st.blksize = r.u32();
  }

  if has(file, libc::ATTR_FILE_DEVTYPE) {
    st.rdev = r.u32();
  }

  if has(file, libc::ATTR_FILE_DATALENGTH) {
    st.size = r.u64();
  }

  if r.at > rec.len() {
    return Err(io::Error::other("getattrlist: a short record"));
  }

  Ok((name, st))
}

/// A vnode type (`enum vtype` in sys/vnode.h) as Linux's file type bits.
fn vtype(x: u32) -> u32 {
  match x {
    1 => S_IFREG,
    2 => S_IFDIR,
    3 => 0o060000, // VBLK
    4 => 0o020000, // VCHR
    5 => S_IFLNK,
    6 => 0o140000, // VSOCK
    7 => 0o010000, // VFIFO
    _ => 0,
  }
}

/// Fields of a record; reads past its end give zeros, which `parse` then refuses.
struct Reader<'a> {
  rec: &'a [u8],
  at: usize,
}

impl Reader<'_> {
  fn take<const N: usize>(&mut self) -> [u8; N] {
    let mut out = [0u8; N];
    if let Some(x) = self.rec.get(self.at..self.at + N) {
      out.copy_from_slice(x);
    }

    self.at += N;
    out
  }

  fn u32(&mut self) -> u32 {
    u32::from_ne_bytes(self.take())
  }

  fn u64(&mut self) -> u64 {
    u64::from_ne_bytes(self.take())
  }

  /// A struct timespec: seconds and nanoseconds, both 64-bit.
  fn time(&mut self) -> (i64, u32) {
    (self.u64() as i64, self.u64() as u32)
  }
}

// MARK: Paths

unsafe extern "C" {
  /// sys/fsgetpath.h: a path of the file with inode `obj_id` on the volume `fsid`.
  fn fsgetpath(buf: *mut libc::c_char, size: usize, fsid: *mut [i32; 2], obj_id: u64) -> isize;
  /// sys/resource.h.
  fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
}

/// Has the calling thread fail with EDEADLK where it would wait for a dataless file, one
/// evicted to iCloud, to download; other threads still wait.
pub fn no_downloads() {
  // IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_THREAD,
  // IOPOL_MATERIALIZE_DATALESS_FILES_OFF.
  // SAFETY: sets a policy of this thread.
  unsafe { setiopolicy_np(3, 1, 1) };
}

/// A path to the file `ino` on the volume `dev`: of a hard-linked file, any of its names.
pub fn path(dev: i32, ino: u64) -> io::Result<CString> {
  let mut buf = vec![0u8; libc::PATH_MAX as usize];
  // An fsid_t: the volume's st_dev, then 0; libc keeps its fields private.
  let mut fsid = [dev, 0];
  // SAFETY: fsgetpath writes at most the buffer's size, with the trailing NUL.
  let n = unsafe { fsgetpath(buf.as_mut_ptr().cast(), buf.len(), &mut fsid, ino) };
  if n <= 0 {
    return Err(io::Error::last_os_error());
  }

  buf.truncate(n as usize - 1);
  CString::new(buf).map_err(|_| io::Error::other("fsgetpath: a NUL in the path"))
}

// MARK: Linux numbers

/// Linux's errno for a macOS error; EIO for those Linux has no word for.
pub fn errno(e: &io::Error) -> i32 {
  let Some(x) = e.raw_os_error() else { return 5 };
  match x {
    libc::EDEADLK => 35,
    1..=34 => x, // EPERM to ERANGE: the same numbers but EDEADLK
    libc::EAGAIN => 11,
    libc::EINPROGRESS => 115,
    libc::EALREADY => 114,
    libc::ENOTSOCK => 88,
    libc::EDESTADDRREQ => 89,
    libc::EMSGSIZE => 90,
    libc::EPROTOTYPE => 91,
    libc::ENOPROTOOPT => 92,
    libc::EPROTONOSUPPORT => 93,
    libc::ENOTSUP | libc::EOPNOTSUPP => 95,
    libc::EAFNOSUPPORT => 97,
    libc::EADDRINUSE => 98,
    libc::EADDRNOTAVAIL => 99,
    libc::ENETDOWN => 100,
    libc::ENETUNREACH => 101,
    libc::ECONNABORTED => 103,
    libc::ECONNRESET => 104,
    libc::ENOBUFS => 105,
    libc::EISCONN => 106,
    libc::ENOTCONN => 107,
    libc::ETIMEDOUT => 110,
    libc::ECONNREFUSED => 111,
    libc::ELOOP => 40,
    libc::ENAMETOOLONG => 36,
    libc::EHOSTUNREACH => 113,
    libc::ENOTEMPTY => 39,
    libc::EDQUOT => 122,
    libc::ESTALE => 116,
    libc::ENOLCK => 37,
    libc::ENOSYS => 38,
    libc::EOVERFLOW => 75,
    libc::ECANCELED => 125,
    libc::EILSEQ => 84,
    libc::ENOATTR | libc::ENODATA => 61,
    libc::EBADMSG => 74,
    _ => 5,
  }
}

/// macOS's open flags for Linux's (arm64 numbers). The kernel keeps O_CREAT and O_EXCL to
/// itself on OPEN; O_DIRECT is F_NOCACHE, set apart.
pub fn open_flags(linux: u32) -> i32 {
  let bits = [
    (0o100, libc::O_CREAT),
    (0o200, libc::O_EXCL),
    (0o1000, libc::O_TRUNC),
    (0o2000, libc::O_APPEND),
    (0o4000, libc::O_NONBLOCK),
    (0o10000, libc::O_DSYNC),
    (0o4010000, libc::O_SYNC),
  ];
  let f = (linux & 3) as i32; // O_RDONLY, O_WRONLY, O_RDWR
  bits.into_iter().filter(|&(l, _)| linux & l == l).fold(f, |f, (_, m)| f | m)
}

/// Linux's O_DIRECT on arm64.
pub const O_DIRECT: u32 = 0o200000;

#[cfg(test)]
mod tests {
  use std::os::fd::AsRawFd;

  use super::*;

  fn tmp() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("lodka-mac-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("sub")).unwrap();
    std::fs::write(d.join("File.txt"), b"hello").unwrap();
    std::os::unix::fs::symlink("File.txt", d.join("link")).unwrap();
    d
  }

  fn metadata(p: &std::path::Path) -> Stat {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(p).unwrap();
    Stat {
      dev: m.dev() as i32,
      ino: m.ino(),
      mode: m.mode(),
      nlink: m.nlink() as u32,
      uid: m.uid(),
      gid: m.gid(),
      rdev: m.rdev() as u32,
      size: m.size(),
      blocks: m.blocks(),
      blksize: m.blksize() as u32,
      atime: (m.atime(), m.atime_nsec() as u32),
      mtime: (m.mtime(), m.mtime_nsec() as u32),
      ctime: (m.ctime(), m.ctime_nsec() as u32),
    }
  }

  #[test]
  fn attributes_match_lstat() {
    let d = tmp();
    let dir = std::fs::File::open(&d).unwrap();
    for name in ["sub", "File.txt", "link"] {
      let (got, st) = stat_at(dir.as_raw_fd(), &CString::new(name).unwrap()).unwrap();
      assert_eq!(got, name.as_bytes());
      let want = metadata(&d.join(name));
      // A directory's blksize and blocks differ between the calls; the rest must not.
      assert_eq!(
        (st.dev, st.ino, st.mode, st.nlink, st.size),
        (want.dev, want.ino, want.mode, want.nlink, want.size),
        "{name}"
      );
      assert_eq!(
        (st.uid, st.gid, st.mtime, st.ctime),
        (want.uid, want.gid, want.mtime, want.ctime),
        "{name}"
      );
    }

    let (got, _) = stat_at(dir.as_raw_fd(), c"file.txt").unwrap();
    assert_eq!(got, b"File.txt", "case-insensitive APFS gives the stored name");
    let e = stat_at(dir.as_raw_fd(), c"nope").unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOENT));

    let mut names: Vec<_> = list(dir.as_raw_fd()).unwrap().into_iter().map(|x| x.0).collect();
    names.sort();
    assert_eq!(names, [b"File.txt".to_vec(), b"link".to_vec(), b"sub".to_vec()]);

    let p = path(metadata(&d).dev, metadata(&d.join("File.txt")).ino).unwrap();
    assert!(p.to_bytes().ends_with(b"/File.txt"));
    let _ = std::fs::remove_dir_all(&d);
  }

  #[test]
  fn linux_numbers() {
    assert_eq!(errno(&io::Error::from_raw_os_error(libc::ENOTEMPTY)), 39);
    assert_eq!(errno(&io::Error::from_raw_os_error(libc::ENOENT)), 2);
    assert_eq!(errno(&io::Error::from_raw_os_error(libc::EDEADLK)), 35);
    assert_eq!(open_flags(0o1002), libc::O_RDWR | libc::O_TRUNC);
    assert_eq!(open_flags(0o2001), libc::O_WRONLY | libc::O_APPEND);
  }
}
