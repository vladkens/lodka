// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The FUSE protocol as the Linux 6.18 guest speaks it: the opcodes, flags and records of the
//! requests it sends a virtio-fs server and of the replies it reads back.
//!
//! Transcribed from the kernel's `include/uapi/linux/fuse.h` (v6.18, protocol 7.45), used under
//! BSD-2-Clause:
//! <https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/plain/include/uapi/linux/fuse.h?h=v6.18>
//!
//! A request is an `InHeader` and the opcode's arguments (a record, NUL-terminated names, data);
//! a reply is an `OutHeader`, alone on an error, else with the opcode's record or data. The guest
//! and the host are both little-endian arm64, so native integers are the wire format.

pub const KERNEL_VERSION: u32 = 7;
pub const KERNEL_MINOR_VERSION: u32 = 45;
/// The node ID of the root directory.
pub const ROOT_ID: u64 = 1;

// MARK: Opcodes

pub const LOOKUP: u32 = 1;
/// No reply.
pub const FORGET: u32 = 2;
pub const GETATTR: u32 = 3;
pub const SETATTR: u32 = 4;
pub const READLINK: u32 = 5;
pub const SYMLINK: u32 = 6;
pub const MKNOD: u32 = 8;
pub const MKDIR: u32 = 9;
pub const UNLINK: u32 = 10;
pub const RMDIR: u32 = 11;
pub const RENAME: u32 = 12;
pub const LINK: u32 = 13;
pub const OPEN: u32 = 14;
pub const READ: u32 = 15;
pub const WRITE: u32 = 16;
pub const STATFS: u32 = 17;
pub const RELEASE: u32 = 18;
pub const FSYNC: u32 = 20;
pub const SETXATTR: u32 = 21;
pub const GETXATTR: u32 = 22;
pub const LISTXATTR: u32 = 23;
pub const REMOVEXATTR: u32 = 24;
pub const FLUSH: u32 = 25;
pub const INIT: u32 = 26;
pub const OPENDIR: u32 = 27;
pub const READDIR: u32 = 28;
pub const RELEASEDIR: u32 = 29;
pub const FSYNCDIR: u32 = 30;
pub const ACCESS: u32 = 34;
pub const CREATE: u32 = 35;
/// Never sent over virtio-fs.
pub const INTERRUPT: u32 = 36;
pub const DESTROY: u32 = 38;
/// No reply.
pub const BATCH_FORGET: u32 = 42;
pub const READDIRPLUS: u32 = 44;
pub const RENAME2: u32 = 45;
pub const LSEEK: u32 = 46;
pub const COPY_FILE_RANGE: u32 = 47;
pub const SYNCFS: u32 = 50;
pub const COPY_FILE_RANGE_64: u32 = 53;

// Notify codes go in `OutHeader.error` of a message whose `unique` is 0.
pub const NOTIFY_INVAL_INODE: i32 = 2;
pub const NOTIFY_INVAL_ENTRY: i32 = 3;
pub const NOTIFY_DELETE: i32 = 6;
pub const NOTIFY_INC_EPOCH: i32 = 8;

// MARK: Flags

// INIT flags: bits 0..31 travel in `InitIn.flags` and `InitOut.flags`, bits 32..63 in `flags2`.
pub const ASYNC_READ: u64 = 1 << 0;
/// The server handles `O_TRUNC` in OPEN.
pub const ATOMIC_O_TRUNC: u64 = 1 << 3;
/// Writes larger than 4 KiB.
pub const BIG_WRITES: u64 = 1 << 5;
/// Drop a file's cached pages when its mtime or size changes.
pub const AUTO_INVAL_DATA: u64 = 1 << 12;
/// READDIRPLUS instead of READDIR.
pub const DO_READDIRPLUS: u64 = 1 << 13;
/// Choose between READDIRPLUS and READDIR as the reads go.
pub const READDIRPLUS_AUTO: u64 = 1 << 14;
pub const ASYNC_DIO: u64 = 1 << 15;
/// Lookups and READDIR in one directory may run in parallel.
pub const PARALLEL_DIROPS: u64 = 1 << 18;
/// `InitOut.max_pages` is valid.
pub const MAX_PAGES: u64 = 1 << 22;
/// Cache READLINK replies.
pub const CACHE_SYMLINKS: u64 = 1 << 23;
/// The kernel mounts directories that have `ATTR_SUBMOUNT`.
pub const SUBMOUNTS: u64 = 1 << 27;
/// HANDLE_KILLPRIV by the VFS rules: suid and sgid only without CAP_FSETID, sgid only on
/// group-executable files.
pub const HANDLE_KILLPRIV_V2: u64 = 1 << 28;
/// `flags2` is valid.
pub const INIT_EXT: u64 = 1 << 30;

// `SetattrIn.valid`: the fields to set.
pub const FATTR_MODE: u32 = 1 << 0;
pub const FATTR_SIZE: u32 = 1 << 3;
pub const FATTR_ATIME: u32 = 1 << 4;
pub const FATTR_MTIME: u32 = 1 << 5;
pub const FATTR_FH: u32 = 1 << 6;
/// With FATTR_ATIME: the server's current time, not `atime`.
pub const FATTR_ATIME_NOW: u32 = 1 << 7;
/// With FATTR_MTIME: the server's current time, not `mtime`.
pub const FATTR_MTIME_NOW: u32 = 1 << 8;
/// Also clear suid and sgid.
pub const FATTR_KILL_SUIDGID: u32 = 1 << 11;

// `OpenOut.open_flags`.
/// Keep the cached data on open.
pub const FOPEN_KEEP_CACHE: u32 = 1 << 1;
/// Cache the directory's entries.
pub const FOPEN_CACHE_DIR: u32 = 1 << 3;
/// lodka's: the guest's attributes of the file are stale, fetch them again (guest/patches).
pub const FOPEN_INVAL_ATTR: u32 = 1 << 31;

// `GetattrIn.getattr_flags`.
pub const GETATTR_FH: u32 = 1 << 0;

// `WriteIn.write_flags`.
pub const WRITE_KILL_SUIDGID: u32 = 1 << 2;

// `OpenIn.open_flags`.
/// Clear suid and sgid if the file is executable.
pub const OPEN_KILL_SUIDGID: u32 = 1 << 0;

// `Attr.flags`.
/// The root of a submount.
pub const ATTR_SUBMOUNT: u32 = 1 << 0;

// MARK: Requests

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InHeader {
  /// The whole request, this header included.
  pub len: u32,
  pub opcode: u32,
  pub unique: u64,
  pub nodeid: u64,
  pub uid: u32,
  pub gid: u32,
  pub pid: u32,
  /// Extensions at the end of the request, in 8-byte units (7.38).
  pub total_extlen: u16,
  pub padding: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InitIn {
  pub major: u32,
  pub minor: u32,
  /// Bytes.
  pub max_readahead: u32,
  pub flags: u32,
  /// INIT flags 32..63, with INIT_EXT (7.36).
  pub flags2: u32,
  pub unused: [u32; 11],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ForgetIn {
  /// Lookups to drop.
  pub nlookup: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BatchForgetIn {
  /// `ForgetOne` records that follow.
  pub count: u32,
  pub dummy: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ForgetOne {
  pub nodeid: u64,
  /// Lookups to drop.
  pub nlookup: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GetattrIn {
  pub getattr_flags: u32,
  pub dummy: u32,
  pub fh: u64,
}

/// Times as in `Attr`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SetattrIn {
  pub valid: u32,
  pub padding: u32,
  pub fh: u64,
  pub size: u64,
  pub lock_owner: u64,
  pub atime: u64,
  pub mtime: u64,
  pub ctime: u64,
  pub atimensec: u32,
  pub mtimensec: u32,
  pub ctimensec: u32,
  pub mode: u32,
  pub unused4: u32,
  pub uid: u32,
  pub gid: u32,
  pub unused5: u32,
}

/// The name follows.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MknodIn {
  pub mode: u32,
  /// Encoded as `Attr.rdev`.
  pub rdev: u32,
  /// The caller's umask; `mode` is already masked unless DONT_MASK.
  pub umask: u32,
  pub padding: u32,
}

/// The name follows.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MkdirIn {
  pub mode: u32,
  /// As in `MknodIn`.
  pub umask: u32,
}

/// The old name and the new name follow.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct RenameIn {
  pub newdir: u64,
}

/// The old name and the new name follow.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Rename2In {
  pub newdir: u64,
  /// Linux `RENAME_*`: NOREPLACE 1, EXCHANGE 2, WHITEOUT 4.
  pub flags: u32,
  pub padding: u32,
}

/// The new name follows.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinkIn {
  pub oldnodeid: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenIn {
  /// Linux arm64 `O_*` flags.
  pub flags: u32,
  pub open_flags: u32,
}

/// Also TMPFILE's request; the name follows.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CreateIn {
  /// Linux arm64 `O_*` flags.
  pub flags: u32,
  pub mode: u32,
  /// As in `MknodIn`.
  pub umask: u32,
  pub open_flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ReleaseIn {
  pub fh: u64,
  /// The file's `O_*` flags.
  pub flags: u32,
  pub release_flags: u32,
  pub lock_owner: u64,
}

/// Also READDIR's and READDIRPLUS's request, with `offset` the last `Dirent.off` read.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ReadIn {
  pub fh: u64,
  pub offset: u64,
  /// Bytes to read.
  pub size: u32,
  pub read_flags: u32,
  pub lock_owner: u64,
  /// The file's `O_*` flags.
  pub flags: u32,
  pub padding: u32,
}

/// The data follows.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WriteIn {
  pub fh: u64,
  pub offset: u64,
  pub size: u32,
  pub write_flags: u32,
  pub lock_owner: u64,
  /// The file's `O_*` flags.
  pub flags: u32,
  pub padding: u32,
}

/// Also FSYNCDIR's request.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsyncIn {
  pub fh: u64,
  pub fsync_flags: u32,
  pub padding: u32,
}

/// `SetxattrIn` without SETXATTR_EXT: no `setxattr_flags`.
pub const COMPAT_SETXATTR_IN_SIZE: usize = 8;

/// The name and the value follow.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SetxattrIn {
  /// Of the value.
  pub size: u32,
  /// Linux `XATTR_CREATE` 1, `XATTR_REPLACE` 2.
  pub flags: u32,
  pub setxattr_flags: u32,
  pub padding: u32,
}

/// Also LISTXATTR's request; GETXATTR's name follows.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GetxattrIn {
  /// The reply buffer; 0 asks for the size in a `GetxattrOut`.
  pub size: u32,
  pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AccessIn {
  pub mask: u32,
  pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LseekIn {
  pub fh: u64,
  pub offset: u64,
  /// Linux `SEEK_*`: DATA 3, HOLE 4.
  pub whence: u32,
  pub padding: u32,
}

/// Also COPY_FILE_RANGE_64's request.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CopyFileRangeIn {
  pub fh_in: u64,
  pub off_in: u64,
  pub nodeid_out: u64,
  pub fh_out: u64,
  pub off_out: u64,
  pub len: u64,
  pub flags: u64,
}

// MARK: Replies

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct OutHeader {
  /// The whole reply, this header included.
  pub len: u32,
  /// 0 or a negative Linux errno; a notify code when `unique` is 0.
  pub error: i32,
  pub unique: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InitOut {
  pub major: u32,
  pub minor: u32,
  /// Bytes.
  pub max_readahead: u32,
  pub flags: u32,
  /// Background requests in flight at most.
  pub max_background: u16,
  pub congestion_threshold: u16,
  /// Bytes.
  pub max_write: u32,
  /// Timestamp granularity in nanoseconds, up to 1_000_000_000 (7.23).
  pub time_gran: u32,
  /// Pages per request at most, with MAX_PAGES (7.28).
  pub max_pages: u16,
  /// log2 of the DAX mapping alignment, with MAP_ALIGNMENT (7.31).
  pub map_alignment: u16,
  /// INIT flags 32..63, read only with INIT_EXT in `flags` (7.36).
  pub flags2: u32,
  /// With PASSTHROUGH (7.40).
  pub max_stack_depth: u32,
  /// Seconds, with REQUEST_TIMEOUT (7.43).
  pub request_timeout: u16,
  pub unused: [u16; 11],
}

/// Times are seconds since the epoch plus the `*nsec` nanoseconds.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Attr {
  pub ino: u64,
  pub size: u64,
  /// 512-byte units.
  pub blocks: u64,
  pub atime: u64,
  pub mtime: u64,
  pub ctime: u64,
  pub atimensec: u32,
  pub mtimensec: u32,
  pub ctimensec: u32,
  pub mode: u32,
  pub nlink: u32,
  pub uid: u32,
  pub gid: u32,
  /// Linux's dev_t: `minor & 0xff | major << 8 | (minor & !0xff) << 12`.
  pub rdev: u32,
  /// Preferred I/O size; 0 for the default (7.9).
  pub blksize: u32,
  /// `ATTR_*` (7.32).
  pub flags: u32,
}

/// The reply to LOOKUP, MKNOD, MKDIR, SYMLINK, LINK and, before an `OpenOut`, CREATE and
/// TMPFILE. Each one with a nonzero `nodeid` is a lookup the guest later FORGETs.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct EntryOut {
  /// 0 in a LOOKUP reply: no such entry, cached for `entry_valid`.
  pub nodeid: u64,
  /// `nodeid` and `generation` together are unique for the file system's lifetime.
  pub generation: u64,
  /// Seconds the name may be cached, plus `entry_valid_nsec`.
  pub entry_valid: u64,
  /// Seconds the attributes may be cached, plus `attr_valid_nsec`.
  pub attr_valid: u64,
  pub entry_valid_nsec: u32,
  pub attr_valid_nsec: u32,
  pub attr: Attr,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AttrOut {
  /// Seconds the attributes may be cached, plus `attr_valid_nsec`.
  pub attr_valid: u64,
  pub attr_valid_nsec: u32,
  pub dummy: u32,
  pub attr: Attr,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenOut {
  pub fh: u64,
  pub open_flags: u32,
  /// With FOPEN_PASSTHROUGH (7.40).
  pub backing_id: i32,
}

/// Also COPY_FILE_RANGE's reply.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WriteOut {
  /// Bytes written.
  pub size: u32,
  pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Kstatfs {
  /// This, `bfree` and `bavail` in `frsize` units.
  pub blocks: u64,
  pub bfree: u64,
  pub bavail: u64,
  pub files: u64,
  pub ffree: u64,
  pub bsize: u32,
  /// Name length at most.
  pub namelen: u32,
  /// Fragment size (7.4).
  pub frsize: u32,
  pub padding: u32,
  pub spare: [u32; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct StatfsOut {
  pub st: Kstatfs,
}

/// GETXATTR's and LISTXATTR's reply to a `GetxattrIn.size` of 0.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GetxattrOut {
  pub size: u32,
  pub padding: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LseekOut {
  pub offset: u64,
}

/// COPY_FILE_RANGE_64's reply (7.45).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CopyFileRangeOut {
  pub bytes_copied: u64,
}

// MARK: Notifications

// Linux 6.18's virtio-fs has no notification queue; lodka's kernel adds one (guest/patches).

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NotifyInvalInodeOut {
  pub ino: u64,
  /// Drop cached pages from here; negative drops the attributes only.
  pub off: i64,
  /// Bytes; 0 or less runs to the end.
  pub len: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NotifyInvalEntryOut {
  pub parent: u64,
  /// The name and a NUL follow.
  pub namelen: u32,
  pub flags: u32,
}

/// lodka's, in `NotifyInvalEntryOut.flags`: the contents of a file the guest has no inode for
/// changed, which only inotify hears of.
pub const INVAL_MODIFIED: u32 = 1 << 31;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct NotifyDeleteOut {
  pub parent: u64,
  pub child: u64,
  /// The name and a NUL follow.
  pub namelen: u32,
  pub padding: u32,
}

// MARK: Directory entries

/// A READDIR entry: the name follows, padded to `dirent_size`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Dirent {
  pub ino: u64,
  /// The `ReadIn.offset` that continues after this entry.
  pub off: u64,
  pub namelen: u32,
  /// `type` in fuse.h: the `DT_*` type, `(mode & S_IFMT) >> 12`.
  pub typ: u32,
}

/// A READDIRPLUS entry: the name follows, padded to `direntplus_size`. A nonzero
/// `entry_out.nodeid` is a lookup, as in `EntryOut`, except for `.` and `..`; 0 sends the name
/// alone.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Direntplus {
  pub entry_out: EntryOut,
  pub dirent: Dirent,
}

/// A `Dirent` and its name, padded to 8 bytes (`FUSE_DIRENT_SIZE`).
pub const fn dirent_size(namelen: usize) -> usize {
  rec_align(size_of::<Dirent>() + namelen)
}

/// A `Direntplus` and its name, padded to 8 bytes (`FUSE_DIRENTPLUS_SIZE`).
pub const fn direntplus_size(namelen: usize) -> usize {
  rec_align(size_of::<Direntplus>() + namelen)
}

const fn rec_align(x: usize) -> usize {
  (x + 7) & !7
}

// MARK: Pod

/// A wire record: `repr(C)` integers with no padding, so any bytes make a valid value.
///
/// # Safety
/// Implement only for such types.
pub unsafe trait Pod: Copy + Default {}

macro_rules! pod {
  ($($t:ty),* $(,)?) => {
    // SAFETY: each is a `repr(C)` struct of integers, integer arrays and such structs, with every
    // gap filled by a named field as in fuse.h, so it has no padding the guest would read
    // uninitialised and any bytes are a valid value; `tests::sizes` pins the layouts.
    $(unsafe impl Pod for $t {})*
  };
}

pod! {
  InHeader, InitIn, ForgetIn, BatchForgetIn, ForgetOne, GetattrIn, SetattrIn, MknodIn, MkdirIn,
  RenameIn, Rename2In, LinkIn, OpenIn, CreateIn, ReleaseIn, ReadIn, WriteIn, FsyncIn,
  SetxattrIn, GetxattrIn, AccessIn, LseekIn, CopyFileRangeIn,
  OutHeader, InitOut, Attr, EntryOut, AttrOut, OpenOut, WriteOut, Kstatfs, StatfsOut, GetxattrOut,
  LseekOut, CopyFileRangeOut,
  NotifyInvalInodeOut, NotifyInvalEntryOut, NotifyDeleteOut,
  Dirent, Direntplus,
}

/// Reads a `T` from the start of `buf`, at any alignment; `None` if `buf` is shorter.
pub fn read<T: Pod>(buf: &[u8]) -> Option<T> {
  if buf.len() < size_of::<T>() {
    return None;
  }

  // SAFETY: `buf` has `size_of::<T>()` bytes to read, `read_unaligned` needs no alignment, and
  // any bytes are a valid `T: Pod`.
  Some(unsafe { buf.as_ptr().cast::<T>().read_unaligned() })
}

/// The bytes of `x` as they go on the wire.
pub fn bytes<T: Pod>(x: &T) -> &[u8] {
  // SAFETY: `x` points to `size_of::<T>()` bytes, all initialised as `T: Pod` has no padding,
  // borrowed for as long as the slice.
  unsafe { std::slice::from_raw_parts(std::ptr::from_ref(x).cast::<u8>(), size_of::<T>()) }
}

#[cfg(test)]
mod tests {
  use std::mem::offset_of;

  use super::*;

  #[test]
  fn sizes() {
    let cases = [
      ("InHeader", size_of::<InHeader>(), 40),
      ("InitIn", size_of::<InitIn>(), 64),
      ("ForgetIn", size_of::<ForgetIn>(), 8),
      ("BatchForgetIn", size_of::<BatchForgetIn>(), 8),
      ("ForgetOne", size_of::<ForgetOne>(), 16),
      ("GetattrIn", size_of::<GetattrIn>(), 16),
      ("SetattrIn", size_of::<SetattrIn>(), 88),
      ("MknodIn", size_of::<MknodIn>(), 16),
      ("MkdirIn", size_of::<MkdirIn>(), 8),
      ("RenameIn", size_of::<RenameIn>(), 8),
      ("Rename2In", size_of::<Rename2In>(), 16),
      ("LinkIn", size_of::<LinkIn>(), 8),
      ("OpenIn", size_of::<OpenIn>(), 8),
      ("CreateIn", size_of::<CreateIn>(), 16),
      ("ReleaseIn", size_of::<ReleaseIn>(), 24),
      ("ReadIn", size_of::<ReadIn>(), 40),
      ("WriteIn", size_of::<WriteIn>(), 40),
      ("FsyncIn", size_of::<FsyncIn>(), 16),
      ("SetxattrIn", size_of::<SetxattrIn>(), 16),
      ("GetxattrIn", size_of::<GetxattrIn>(), 8),
      ("AccessIn", size_of::<AccessIn>(), 8),
      ("LseekIn", size_of::<LseekIn>(), 24),
      ("CopyFileRangeIn", size_of::<CopyFileRangeIn>(), 56),
      ("OutHeader", size_of::<OutHeader>(), 16),
      ("InitOut", size_of::<InitOut>(), 64),
      ("Attr", size_of::<Attr>(), 88),
      ("EntryOut", size_of::<EntryOut>(), 128),
      ("AttrOut", size_of::<AttrOut>(), 104),
      ("OpenOut", size_of::<OpenOut>(), 16),
      ("WriteOut", size_of::<WriteOut>(), 8),
      ("Kstatfs", size_of::<Kstatfs>(), 80),
      ("StatfsOut", size_of::<StatfsOut>(), 80),
      ("GetxattrOut", size_of::<GetxattrOut>(), 8),
      ("LseekOut", size_of::<LseekOut>(), 8),
      ("CopyFileRangeOut", size_of::<CopyFileRangeOut>(), 8),
      ("NotifyInvalInodeOut", size_of::<NotifyInvalInodeOut>(), 24),
      ("NotifyInvalEntryOut", size_of::<NotifyInvalEntryOut>(), 16),
      ("NotifyDeleteOut", size_of::<NotifyDeleteOut>(), 24),
      ("Dirent", size_of::<Dirent>(), 24),
      ("Direntplus", size_of::<Direntplus>(), 152),
    ];

    for (name, got, want) in cases {
      assert_eq!(got, want, "{name}");
    }
  }

  #[test]
  fn compat_sizes() {
    // SETXATTR's short form ends where the field it lacks starts.
    assert_eq!(COMPAT_SETXATTR_IN_SIZE, offset_of!(SetxattrIn, setxattr_flags));
  }

  #[test]
  fn round_trip() {
    let x = InitOut {
      major: KERNEL_VERSION,
      minor: KERNEL_MINOR_VERSION,
      flags: (ASYNC_READ | INIT_EXT) as u32,
      max_write: 1 << 20,
      max_pages: 256,
      flags2: 0x401,
      request_timeout: 30,
      ..Default::default()
    };
    let buf = bytes(&x);
    assert_eq!(buf.len(), 64);
    assert_eq!(buf[..8], [7, 0, 0, 0, 45, 0, 0, 0]);

    // One byte in, so the read is unaligned.
    let raw = [&[0][..], buf].concat();
    let y: InitOut = read(&raw[1..]).unwrap();
    assert_eq!(bytes(&y), buf);
    assert_eq!((y.minor, y.max_write, y.max_pages, y.flags2), (45, 1 << 20, 256, 0x401));
    assert!(read::<InitOut>(&buf[..63]).is_none());

    let raw: Vec<u8> = (0..152).collect();
    let d: Direntplus = read(&raw).unwrap();
    assert_eq!(bytes(&d), &raw[..]);
    assert_eq!(d.dirent.typ, u32::from_le_bytes([148, 149, 150, 151]));
  }

  #[test]
  fn dirent_sizes() {
    let cases = [(1, 32), (2, 32), (8, 32), (9, 40), (255, 280)];
    for (namelen, want) in cases {
      assert_eq!(dirent_size(namelen), want, "{namelen}");
      assert_eq!(direntplus_size(namelen), want + 128, "{namelen}");
    }
  }
}
