// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The FUSE server behind the virtio file system: the guest kernel's requests, answered from
//! the Mac folder it shares. A node is a file's `(dev, ino)` with the guest's lookup count,
//! reached again through its path from `fsgetpath` and checked to be the same file; only open
//! files and directories hold descriptors.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::abi::{self, Pod};
use super::mac::{self, Entry, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, Stat};
use crate::mem::Mem;
use crate::virtio::Chain;

// Linux's numbers, which the guest reads.
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const ENOMEM: i32 = 12;
const EACCES: i32 = 13;
const EEXIST: i32 = 17;
const E2BIG: i32 = 7;
const EINVAL: i32 = 22;
const ENFILE: i32 = 23;
const ERANGE: i32 = 34;
const EDEADLK: i32 = 35;
const ENAMETOOLONG: i32 = 36;
const ENOSYS: i32 = 38;
const ELOOP: i32 = 40;
const ENODATA: i32 = 61;
const ESTALE: i32 = 116;
/// lodka's: the guest sends the request again on a request queue (guest/patches).
const SYNC_AGAIN: i32 = 1024;

/// How long the guest may trust a name, its absence and attributes without asking again: long
/// while FSEvents tells it what changes on the Mac, else a second.
const TTL: Duration = Duration::from_secs(3600);
const UNWATCHED_TTL: Duration = Duration::from_secs(1);
/// The largest READ or WRITE: 256 guest pages.
const MAX_IO: u32 = 1 << 20;
/// The largest COPY_FILE_RANGE: a shorter copy is an answer too, and a request holds its thread
/// no longer.
const MAX_COPY: u64 = 64 << 20;
/// Linux's largest extended attribute, and list of their names.
const MAX_XATTR: usize = 64 * 1024;
/// Directories whose descriptors are kept for lookups in them.
const DIRS: usize = 256;
/// Nodes the guest may hold at once, some 150 bytes each: a guest kernel keeps far fewer inodes,
/// one that never forgets gets ENOMEM.
const NODES: usize = 4 << 20;
/// Files made on the Mac that the guest heard of before it had nodes for them, kept for when it
/// does: past this many, forgotten.
const TOLD: usize = 16 << 10;

pub struct Server {
  root: CString,
  nodes: Mutex<Nodes>,
  max_nodes: usize,
  handles: Mutex<HashMap<u64, Arc<Handle>>>,
  /// Half the descriptors the process may have: the rest stay for its disks, network and
  /// FSEvents.
  max_handles: usize,
  next_fh: AtomicU64,
  /// Descriptors of directories the guest works in, by node.
  dirs: Mutex<HashMap<u64, Arc<OwnedFd>>>,
  /// FSEvents tells the guest what changes on the Mac.
  watched: AtomicBool,
}

#[derive(Clone, Copy)]
struct Node {
  dev: i32,
  ino: u64,
  kind: u32,
  /// The guest's lookups not yet forgotten.
  refs: u64,
  /// The file's mtime and size as the guest was last told them. Unchanged at an open, its
  /// attributes and pages in the guest are good: the guest drops pages when either changes.
  seen: Option<((i64, u32), u64)>,
  /// The guest knows the name was made: it made the file, or was told that FSEvents saw it made.
  told: bool,
}

struct Nodes {
  ids: HashMap<u64, Node>,
  keys: HashMap<(i32, u64), u64>,
  next: u64,
  /// Files without a node the guest was told were made, by `(dev, ino)`: at most TOLD.
  told: HashSet<(i32, u64)>,
}

struct Handle {
  fd: OwnedFd,
  /// The node it is open for.
  node: u64,
  list: Mutex<Listing>,
}

/// A directory's entries as the guest reads them, listed from the descriptor as it goes: those
/// from index `base` on, to the end if `done`.
#[derive(Default)]
struct Listing {
  base: usize,
  entries: Vec<Entry>,
  done: bool,
  /// Where the descriptor is is unknown after an error: listed again from the start.
  lost: bool,
}

impl Listing {
  fn more(&mut self, fd: RawFd) -> Result<(), i32> {
    let r = mac::list(fd, &mut self.entries);
    self.done = !matches!(r, Ok(true));
    self.lost = r.is_err();
    r.map(|_| ()).map_err(err)
  }
}

/// A request: its header and arguments, and the chain its data and reply go through.
struct Req<'a> {
  h: abi::InHeader,
  args: &'a [u8],
  mem: &'a Mem,
  chain: &'a Chain,
}

enum Out {
  /// No reply: FORGET, INTERRUPT.
  None,
  Bytes(Vec<u8>),
  /// The reply's body is in the chain already, this long: READ.
  Written(usize),
}

type Res = Result<Out, i32>;

fn reply<T: Pod>(x: &T) -> Res {
  Ok(Out::Bytes(abi::bytes(x).to_vec()))
}

fn os() -> i32 {
  mac::errno(&io::Error::last_os_error())
}

fn err(e: io::Error) -> i32 {
  mac::errno(&e)
}

impl Server {
  pub fn new(root: &Path) -> io::Result<Server> {
    // Opened with no symlink on the way, as every node is.
    let root = CString::new(root.canonicalize()?.into_os_string().into_vec())?;
    // `open` gives Linux's errno, for the guest; errno is still macOS's.
    let fd =
      open(&root, libc::O_RDONLY | libc::O_DIRECTORY).map_err(|_| io::Error::last_os_error())?;
    let st = mac::fstat(fd.as_raw_fd())?;
    let node = Node { dev: st.dev, ino: st.ino, kind: S_IFDIR, refs: 1, seen: None, told: true };
    let nodes = Nodes {
      ids: HashMap::from([(abi::ROOT_ID, node)]),
      keys: HashMap::from([((st.dev, st.ino), abi::ROOT_ID)]),
      next: abi::ROOT_ID + 1,
      told: HashSet::new(),
    };

    // SAFETY: a plain C struct, valid zeroed.
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: getrlimit fills one rlimit.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } != 0 {
      return Err(io::Error::last_os_error());
    }

    Ok(Server {
      root,
      nodes: Mutex::new(nodes),
      max_nodes: NODES,
      handles: Mutex::default(),
      max_handles: (rl.rlim_cur / 2) as usize,
      next_fh: AtomicU64::new(1),
      dirs: Mutex::new(HashMap::from([(abi::ROOT_ID, Arc::new(fd))])),
      watched: AtomicBool::new(false),
    })
  }

  /// The folder shared, with no symlink on the way.
  pub fn root(&self) -> &CStr {
    &self.root
  }

  /// FSEvents reports the Mac's changes to the guest from now on: it may cache for long.
  pub fn watched(&self) {
    self.watched.store(true, Ordering::Relaxed);
  }

  fn ttl(&self) -> Duration {
    if self.watched.load(Ordering::Relaxed) { TTL } else { UNWATCHED_TTL }
  }

  /// The notifications for a change FSEvents saw at `path`: the guest drops what it cached of
  /// the file and of its name, and its inotify watchers hear of it. Only for what it knows.
  pub fn changed(&self, path: &[u8], flags: u32) -> Vec<Vec<u8>> {
    // kFSEventStreamEventFlag*: events were lost; a name was made, removed or renamed; the
    // contents changed.
    const LOST: u32 = 0x01 | 0x02 | 0x04;
    const CREATED: u32 = 0x100;
    const MOVED: u32 = 0x200 | 0x800;
    const NAME: u32 = CREATED | MOVED;
    const DATA: u32 = 0x1000;
    const IS_DIR: u32 = 0x20000;

    // A directory moved, maybe out of the share: a descriptor kept for lookups in it would still
    // lead there. The others are opened again as needed.
    if flags & LOST != 0 || (flags & MOVED != 0 && flags & IS_DIR != 0) {
      self.dirs.lock().unwrap().retain(|&x, _| x == abi::ROOT_ID);
    }

    if flags & LOST != 0 {
      return vec![inc_epoch()];
    }

    let path = path.strip_suffix(b"/").unwrap_or(path);
    let Some((dir, name)) = split(path) else { return Vec::new() };
    if name.is_empty() || path == self.root.as_bytes() {
      return Vec::new();
    }

    // For inotify's IN_ISDIR: the guest has no inode for a name made or removed.
    let is_dir = if flags & IS_DIR != 0 { abi::INVAL_DIR } else { 0 };
    let parent = lstat(dir).and_then(|st| self.known(&st));
    let Some(st) = lstat(path) else {
      let Some(parent) = parent else { return Vec::new() };
      let namelen = name.len() as u32;
      let x = abi::NotifyDeleteOut { parent, child: 0, namelen, padding: is_dir };
      return vec![note(abi::NOTIFY_DELETE, &[abi::bytes(&x), name, &[0]])];
    };

    let mut out = Vec::new();
    let known = self.known(&st);
    if let Some(ino) = known {
      // Contents dropped too, or only the attributes.
      let off = if flags & (DATA | NAME) != 0 && !st.is_dir() { 0 } else { -1 };
      let x = abi::NotifyInvalInodeOut { ino, off, len: 0 };
      out.push(note(abi::NOTIFY_INVAL_INODE, &[abi::bytes(&x)]));
    }

    let Some(parent) = parent else { return out };
    let mut entry = |flags| {
      let x = abi::NotifyInvalEntryOut { parent, namelen: name.len() as u32, flags };
      out.push(note(abi::NOTIFY_INVAL_ENTRY, &[abi::bytes(&x), name, &[0]]));
    };

    // FSEvents keeps a file's Created flag on its later events: a name is news once, as the
    // guest may have looked it up first, unless it was moved or is a hard link's.
    if flags & MOVED != 0
      || (flags & CREATED != 0 && (self.untold(&st) || (!st.is_dir() && st.nlink > 1)))
    {
      entry(is_dir);
    }

    // A known file's modification reaches its directory's watchers through its inode.
    if flags & DATA != 0 && known.is_none() && !st.is_dir() {
      entry(abi::INVAL_MODIFIED);
    }

    out
  }

  /// Serves one request; returns how many bytes of reply it wrote into the chain.
  pub fn serve(&self, mem: &Mem, chain: &Chain) -> u32 {
    self.serve_on(mem, chain, false)
  }

  /// Serves one request on a vCPU's thread, which must not wait long: a file still in iCloud
  /// is sent back to the guest for a request queue instead (VIRTIO_FS_SYNC_AGAIN).
  pub fn serve_vcpu(&self, mem: &Mem, chain: &Chain) -> u32 {
    self.serve_on(mem, chain, true)
  }

  fn serve_on(&self, mem: &Mem, chain: &Chain, vcpu: bool) -> u32 {
    let mut hdr = [0u8; size_of::<abi::InHeader>()];
    if chain.read_at(mem, 0, &mut hdr).ok() != Some(hdr.len()) {
      return 0;
    }

    let h: abi::InHeader = abi::read(&hdr).unwrap_or_default();
    // A WRITE's data goes from guest memory straight to the file; read only its arguments.
    let rest = chain.read_len().saturating_sub(hdr.len());
    let len = match h.opcode {
      abi::WRITE => size_of::<abi::WriteIn>(),
      _ => rest.min(256 * 1024),
    };

    let mut args = vec![0u8; len];
    if chain.read_at(mem, hdr.len(), &mut args).ok() != Some(len) {
      return self.send(mem, chain, &h, Err(EIO));
    }

    let req = Req { h, args: &args, mem, chain };
    let out = match self.handle(&req) {
      Err(EDEADLK) if vcpu => Err(SYNC_AGAIN),
      x => x,
    };

    self.send(mem, chain, &h, out)
  }

  fn send(&self, mem: &Mem, chain: &Chain, h: &abi::InHeader, out: Res) -> u32 {
    let hdr_len = size_of::<abi::OutHeader>();
    let (error, len) = match out {
      Ok(Out::None) => return 0,
      Ok(Out::Bytes(b)) if hdr_len + b.len() <= chain.write_len() => {
        match chain.write_at(mem, hdr_len, &b) {
          Ok(_) => (0, b.len()),
          Err(_) => (-EIO, 0),
        }
      }
      Ok(Out::Bytes(_)) => (-EIO, 0),
      Ok(Out::Written(n)) => (0, n),
      Err(e) => (-e, 0),
    };

    let oh = abi::OutHeader { len: (hdr_len + len) as u32, error, unique: h.unique };
    // A chain with no room for the header gets what fits: the guest reads no reply from it.
    match chain.write_at(mem, 0, abi::bytes(&oh)) {
      Ok(n) if n == hdr_len => oh.len,
      Ok(n) => n as u32,
      Err(_) => 0,
    }
  }

  fn handle(&self, r: &Req) -> Res {
    let id = r.h.nodeid;

    match r.h.opcode {
      abi::INIT => self.init(r),
      abi::DESTROY => Ok(Out::Bytes(Vec::new())),
      abi::LOOKUP => {
        let (name, _) = name(r.args, 0)?;
        self.lookup(id, name)
      }
      abi::FORGET => {
        let x: abi::ForgetIn = arg(r.args, 0)?;
        self.forget(id, x.nlookup);
        Ok(Out::None)
      }
      abi::BATCH_FORGET => {
        let x: abi::BatchForgetIn = arg(r.args, 0)?;
        let at = size_of::<abi::BatchForgetIn>();
        for i in 0..x.count as usize {
          let f: abi::ForgetOne = arg(r.args, at + i * size_of::<abi::ForgetOne>())?;
          self.forget(f.nodeid, f.nlookup);
        }

        Ok(Out::None)
      }
      abi::INTERRUPT => Ok(Out::None),
      abi::GETATTR => {
        let x: abi::GetattrIn = arg(r.args, 0)?;
        let st = match x.getattr_flags & abi::GETATTR_FH {
          0 => self.stat(id)?,
          _ => mac::fstat(self.handle_fd(x.fh)?.fd.as_raw_fd()).map_err(err)?,
        };

        self.attr_out(id, &st)
      }
      abi::SETATTR => self.setattr(r),
      abi::READLINK => {
        let (dir, name) = self.parent_of(id)?;
        let mut buf = vec![0u8; libc::PATH_MAX as usize];
        // SAFETY: readlinkat writes at most the buffer's size.
        let n = unsafe {
          libc::readlinkat(dir.as_raw_fd(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
        };
        if n < 0 {
          return Err(os());
        }

        buf.truncate(n as usize);
        Ok(Out::Bytes(buf))
      }
      abi::SYMLINK => {
        let (name, at) = name(r.args, 0)?;
        let (target, _) = cstr(r.args, at)?;
        let dir = self.dir(id)?;
        // SAFETY: both strings are NUL-terminated.
        if unsafe { libc::symlinkat(target.as_ptr(), dir.as_raw_fd(), name.as_ptr()) } != 0 {
          return Err(os());
        }

        self.entry_at(id, &dir, name, true)
      }
      abi::MKNOD => {
        let x: abi::MknodIn = arg(r.args, 0)?;
        let (name, _) = name(r.args, size_of::<abi::MknodIn>())?;
        let dir = self.dir(id)?;
        mknod(dir.as_raw_fd(), name, x.mode)?;
        self.entry_at(id, &dir, name, true)
      }
      abi::MKDIR => {
        let x: abi::MkdirIn = arg(r.args, 0)?;
        let (name, _) = name(r.args, size_of::<abi::MkdirIn>())?;
        let dir = self.dir(id)?;
        let mode = (x.mode & 0o7777) as u16;
        // SAFETY: a NUL-terminated name in an open directory.
        if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), mode) } != 0 {
          return Err(os());
        }

        // SAFETY: as for mkdirat. The mode as the guest asks, as in CREATE.
        unsafe { libc::fchmodat(dir.as_raw_fd(), name.as_ptr(), mode, libc::AT_SYMLINK_NOFOLLOW) };
        self.entry_at(id, &dir, name, true)
      }
      abi::UNLINK | abi::RMDIR => {
        let (name, _) = name(r.args, 0)?;
        let flag = if r.h.opcode == abi::RMDIR { libc::AT_REMOVEDIR } else { 0 };
        let dir = self.dir(id)?;
        // SAFETY: as for MKDIR.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), flag) } != 0 {
          return Err(os());
        }

        Ok(Out::Bytes(Vec::new()))
      }
      abi::RENAME => {
        let x: abi::RenameIn = arg(r.args, 0)?;
        self.rename(id, x.newdir, 0, r.args, size_of::<abi::RenameIn>())
      }
      abi::RENAME2 => {
        let x: abi::Rename2In = arg(r.args, 0)?;
        self.rename(id, x.newdir, x.flags, r.args, size_of::<abi::Rename2In>())
      }
      abi::LINK => {
        let x: abi::LinkIn = arg(r.args, 0)?;
        let (name, _) = name(r.args, size_of::<abi::LinkIn>())?;
        let (from, old) = self.parent_of(x.oldnodeid)?;
        let dir = self.dir(id)?;
        // SAFETY: NUL-terminated names in open directories.
        if unsafe {
          libc::linkat(from.as_raw_fd(), old.as_ptr(), dir.as_raw_fd(), name.as_ptr(), 0)
        } != 0
        {
          return Err(os());
        }

        self.entry_at(id, &dir, name, true)
      }
      abi::OPEN => {
        let x: abi::OpenIn = arg(r.args, 0)?;
        self.open(id, x.flags, x.open_flags)
      }
      abi::CREATE => self.create(r),
      abi::READ => self.read(r),
      abi::WRITE => self.write(r),
      abi::STATFS => {
        let dir = self.dir(abi::ROOT_ID)?;
        // SAFETY: a plain C struct, valid zeroed.
        let mut s: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: fstatfs fills one statfs.
        if unsafe { libc::fstatfs(dir.as_raw_fd(), &mut s) } != 0 {
          return Err(os());
        }

        let st = abi::Kstatfs {
          blocks: s.f_blocks,
          bfree: s.f_bfree,
          bavail: s.f_bavail,
          files: s.f_files,
          ffree: s.f_ffree,
          bsize: s.f_bsize,
          namelen: 255,
          frsize: s.f_bsize,
          ..Default::default()
        };
        reply(&abi::StatfsOut { st })
      }
      // Writes reach the file at once, so a close has nothing to flush: ENOSYS, and the guest
      // sends no more FLUSHes.
      abi::FLUSH => Err(ENOSYS),
      abi::SYNCFS => {
        let dir = self.dir(id)?;
        // SAFETY: an open directory on the volume; the flag waits for the writes to end.
        if unsafe { fsync_volume_np(dir.as_raw_fd(), SYNC_VOLUME_WAIT) } != 0 {
          return Err(os());
        }

        Ok(Out::Bytes(Vec::new()))
      }
      abi::RELEASE | abi::RELEASEDIR => {
        let x: abi::ReleaseIn = arg(r.args, 0)?;
        self.handles.lock().unwrap().remove(&x.fh);
        Ok(Out::Bytes(Vec::new()))
      }
      abi::FSYNC | abi::FSYNCDIR => {
        let x: abi::FsyncIn = arg(r.args, 0)?;
        let h = self.handle_fd(x.fh)?;
        // A barrier, as the data disk's flush: earlier writes reach the disk first.
        // SAFETY: fcntl and fsync on an open file.
        let ok = unsafe {
          libc::fcntl(h.fd.as_raw_fd(), libc::F_BARRIERFSYNC) == 0
            || libc::fsync(h.fd.as_raw_fd()) == 0
        };
        if !ok {
          return Err(os());
        }

        Ok(Out::Bytes(Vec::new()))
      }
      abi::OPENDIR => {
        let fd = self.open_node(id, libc::O_RDONLY | libc::O_DIRECTORY)?;
        let fh = self.add_handle(id, fd)?;
        // Kept across opens while FSEvents tells the guest of changes, which bump its version.
        let keep = if self.watched.load(Ordering::Relaxed) { abi::FOPEN_KEEP_CACHE } else { 0 };
        let open_flags = abi::FOPEN_CACHE_DIR | keep;
        reply(&abi::OpenOut { fh, open_flags, ..Default::default() })
      }
      abi::READDIR => self.readdir(r, false),
      abi::READDIRPLUS => self.readdir(r, true),
      abi::ACCESS => {
        let x: abi::AccessIn = arg(r.args, 0)?;
        let (dir, name) = self.parent_of(id)?;
        // SAFETY: a NUL-terminated name in an open directory; the mask's bits are the same on
        // both systems.
        if unsafe {
          libc::faccessat(dir.as_raw_fd(), name.as_ptr(), x.mask as i32, libc::AT_SYMLINK_NOFOLLOW)
        } != 0
        {
          return Err(os());
        }

        Ok(Out::Bytes(Vec::new()))
      }
      abi::GETXATTR => {
        let x: abi::GetxattrIn = arg(r.args, 0)?;
        let (name, _) = cstr(r.args, size_of::<abi::GetxattrIn>())?;
        if hidden(name.to_bytes()) {
          return Err(ENODATA);
        }

        self.xattr_get(id, Some(name), x.size)
      }
      abi::LISTXATTR => {
        let x: abi::GetxattrIn = arg(r.args, 0)?;
        self.xattr_get(id, None, x.size)
      }
      abi::SETXATTR => self.setxattr(r),
      abi::REMOVEXATTR => {
        let (name, _) = cstr(r.args, 0)?;
        if hidden(name.to_bytes()) {
          return Err(EPERM);
        }

        let done = |r: i32| if r == 0 { Ok(Out::Bytes(Vec::new())) } else { Err(os()) };
        self.on_file(
          id,
          // SAFETY: NUL-terminated strings.
          |fd| done(unsafe { libc::fremovexattr(fd, name.as_ptr(), 0) }),
          // SAFETY: as for a descriptor.
          |file| {
            done(unsafe { libc::removexattr(file.as_ptr(), name.as_ptr(), libc::XATTR_NOFOLLOW) })
          },
        )
      }
      abi::COPY_FILE_RANGE | abi::COPY_FILE_RANGE_64 => {
        let x: abi::CopyFileRangeIn = arg(r.args, 0)?;
        let n = self.copy(&x)?;
        match r.h.opcode {
          abi::COPY_FILE_RANGE => reply(&abi::WriteOut { size: n as u32, padding: 0 }),
          _ => reply(&abi::CopyFileRangeOut { bytes_copied: n }),
        }
      }
      abi::LSEEK => {
        let x: abi::LseekIn = arg(r.args, 0)?;
        // Linux's SEEK_DATA and SEEK_HOLE are 3 and 4, macOS's 4 and 3.
        let whence = match x.whence {
          3 => libc::SEEK_DATA,
          4 => libc::SEEK_HOLE,
          w => w as i32,
        };

        let h = self.handle_fd(x.fh)?;
        // SAFETY: lseek on an open file.
        let at = unsafe { libc::lseek(h.fd.as_raw_fd(), x.offset as i64, whence) };
        if at < 0 {
          return Err(os());
        }

        reply(&abi::LseekOut { offset: at as u64 })
      }
      _ => Err(ENOSYS),
    }
  }

  fn init(&self, r: &Req) -> Res {
    let x: abi::InitIn = arg(r.args, 0)?;
    let mut out = abi::InitOut {
      major: abi::KERNEL_VERSION,
      minor: abi::KERNEL_MINOR_VERSION,
      ..Default::default()
    };
    if x.major != abi::KERNEL_VERSION {
      return reply(&out);
    }

    let offered = x.flags as u64 | ((x.flags2 as u64) << 32);
    // Not WRITEBACK_CACHE: the guest would then trust its own size and mtime over the Mac's.
    // Not the lock flags: the driver never interrupts a lock that waits.
    let want = abi::ASYNC_READ
      | abi::ATOMIC_O_TRUNC
      | abi::AUTO_INVAL_DATA
      | abi::DO_READDIRPLUS
      | abi::READDIRPLUS_AUTO
      | abi::ASYNC_DIO
      | abi::PARALLEL_DIROPS
      | abi::MAX_PAGES
      | abi::CACHE_SYMLINKS
      | abi::SUBMOUNTS
      | abi::HANDLE_KILLPRIV_V2
      | abi::BIG_WRITES
      | abi::INIT_EXT;
    let flags = offered & want;
    out.max_readahead = x.max_readahead;
    out.flags = flags as u32;
    out.flags2 = (flags >> 32) as u32;
    out.max_background = 256;
    out.congestion_threshold = 192;
    out.max_write = MAX_IO;
    out.time_gran = 1;
    out.max_pages = (MAX_IO / 4096) as u16;
    reply(&out)
  }

  // MARK: Nodes

  fn node(&self, id: u64) -> Result<Node, i32> {
    self.nodes.lock().unwrap().ids.get(&id).copied().ok_or(ESTALE)
  }

  /// `st` if it is the node's file: a path can lead elsewhere once renamed.
  fn same(&self, id: u64, st: Stat) -> Result<Stat, i32> {
    let n = self.node(id)?;
    match (st.dev, st.ino) == (n.dev, n.ino) {
      true => Ok(st),
      false => Err(ESTALE),
    }
  }

  /// The guest looked `st` up once more: its node, made on the first lookup.
  fn add(&self, st: &Stat) -> Result<u64, i32> {
    let mut n = self.nodes.lock().unwrap();
    if let Some(&id) = n.keys.get(&(st.dev, st.ino))
      && let Some(x) = n.ids.get_mut(&id)
    {
      x.refs += 1;
      x.kind = st.kind();
      x.seen = Some((st.mtime, st.size));
      return Ok(id);
    }

    if n.ids.len() >= self.max_nodes {
      return Err(ENOMEM);
    }

    let id = n.next;
    n.next += 1;
    let seen = Some((st.mtime, st.size));
    let told = n.told.remove(&(st.dev, st.ino));
    let node = Node { dev: st.dev, ino: st.ino, kind: st.kind(), refs: 1, seen, told };
    n.ids.insert(id, node);
    n.keys.insert((st.dev, st.ino), id);
    Ok(id)
  }

  fn forget(&self, id: u64, count: u64) {
    if id == abi::ROOT_ID {
      return;
    }

    let mut n = self.nodes.lock().unwrap();
    let Some(x) = n.ids.get_mut(&id) else { return };
    x.refs = x.refs.saturating_sub(count);
    if x.refs > 0 {
      return;
    }

    let key = (x.dev, x.ino);
    n.ids.remove(&id);
    if n.keys.get(&key) == Some(&id) {
      n.keys.remove(&key);
    }

    drop(n);
    self.dirs.lock().unwrap().remove(&id);
  }

  /// A path to the node's file now; ESTALE once it is gone, from the share too.
  fn path(&self, id: u64) -> Result<CString, i32> {
    if id == abi::ROOT_ID {
      return Ok(self.root.clone());
    }

    let n = self.node(id)?;
    let path = mac::path(n.dev, n.ino).map_err(|e| match err(e) {
      ENOENT => ESTALE,
      x => x,
    })?;
    // Moved out of the share on the Mac, it is no longer the guest's.
    let root = self.root.as_bytes();
    match path.as_bytes().strip_prefix(root) {
      Some(rest) if rest.is_empty() || rest[0] == b'/' || root == b"/" => Ok(path),
      _ => Err(ESTALE),
    }
  }

  /// Whether `fd` is the node's file.
  fn check(&self, id: u64, fd: RawFd) -> Result<Stat, i32> {
    self.same(id, mac::fstat(fd).map_err(err)?)
  }

  /// The node's file opened, with `flags`: a regular file or a directory, never a FIFO, whose
  /// open waits for the other end, nor a socket or a device.
  fn open_node(&self, id: u64, flags: i32) -> Result<OwnedFd, i32> {
    // With O_NONBLOCK a FIFO opens at once, to be refused; with O_DIRECTORY none opens.
    let nonblock = if flags & libc::O_DIRECTORY == 0 { libc::O_NONBLOCK } else { 0 };
    // Truncated once known to be the node's file: a rename may have put another in its place.
    let fd = open(&self.path(id)?, (flags & !libc::O_TRUNC) | nonblock)?;
    let st = self.check(id, fd.as_raw_fd())?;
    let fd = plain(fd, &st, flags)?;
    // SAFETY: ftruncate on an open file.
    if flags & libc::O_TRUNC != 0 && unsafe { libc::ftruncate(fd.as_raw_fd(), 0) } != 0 {
      return Err(os());
    }

    Ok(fd)
  }

  /// A directory node's descriptor, kept for the next lookup in it.
  fn dir(&self, id: u64) -> Result<Arc<OwnedFd>, i32> {
    if let Some(x) = self.dirs.lock().unwrap().get(&id) {
      return Ok(x.clone());
    }

    // One the user may search but not read is opened for that alone, and not kept: macOS checks
    // a search-only descriptor's permission once, at its open, and a chmod since would not count.
    let fd = match self.open_node(id, libc::O_RDONLY | libc::O_DIRECTORY) {
      Err(EACCES) => return Ok(Arc::new(self.open_node(id, libc::O_SEARCH | libc::O_DIRECTORY)?)),
      x => Arc::new(x?),
    };

    // Kept only while FSEvents tells of the moves that drop it: else one moved out of the share
    // would still lead there.
    if !self.watched.load(Ordering::Relaxed) {
      return Ok(fd);
    }

    let mut dirs = self.dirs.lock().unwrap();
    if dirs.len() >= DIRS {
      dirs.retain(|&x, _| x == abi::ROOT_ID);
    }

    dirs.insert(id, fd.clone());
    Ok(fd)
  }

  /// The directory a node's file is in, and its name there: for what works on a symlink itself.
  fn parent_of(&self, id: u64) -> Result<(OwnedFd, CString), i32> {
    let path = self.path(id)?;
    let bytes = path.as_bytes();
    let (parent, name) = split(bytes).ok_or(EINVAL)?;
    let parent = CString::new(parent).map_err(|_| EINVAL)?;
    let name = CString::new(name).map_err(|_| EINVAL)?;
    // For lookups only: a directory the user may search but not read works too.
    let dir = open(&parent, libc::O_SEARCH)?;
    let (_, st) = mac::stat_at(dir.as_raw_fd(), &name).map_err(err)?;
    self.same(id, st)?;
    Ok((dir, name))
  }

  /// `f` on the node's name in its directory, which is this thread's working directory
  /// meanwhile: for the calls with no `*at` form. One name and no path, so nothing on the way
  /// to swap for a symlink, and the permissions are as by the path.
  fn in_dir<T>(&self, id: u64, f: impl FnOnce(&CStr) -> Result<T, i32>) -> Result<T, i32> {
    let (dir, name) = self.parent_of(id)?;
    // SAFETY: an open directory as the thread's working directory.
    if unsafe { pthread_fchdir_np(dir.as_raw_fd()) } != 0 {
      return Err(os());
    }

    let r = f(&name);
    // SAFETY: the process's working directory again.
    unsafe { pthread_fchdir_np(-1) };
    r
  }

  /// `fd` on a descriptor of the node's file, as a name may lead to another file by the time it
  /// is used: the guest's open file, or one opened for this alone. `name` on its name in its
  /// directory where none opens (`open_any`).
  fn on_file<T>(
    &self,
    id: u64,
    fd: impl FnOnce(RawFd) -> Result<T, i32>,
    name: impl FnOnce(&CStr) -> Result<T, i32>,
  ) -> Result<T, i32> {
    if let Some(h) = self.handle_of(id, false) {
      return fd(h.fd.as_raw_fd());
    }

    match self.open_any(id, self.node(id)?.kind)? {
      Some(x) => fd(x.as_raw_fd()),
      None => self.in_dir(id, name),
    }
  }

  /// The node's attributes now: by its path, or, with none left, by a file the guest has open
  /// for it, as one removed while open.
  fn stat(&self, id: u64) -> Result<Stat, i32> {
    let n = self.node(id)?;
    if n.kind == S_IFDIR {
      return mac::fstat(self.dir(id)?.as_raw_fd()).map_err(err);
    }

    let st = self.path(id).and_then(|x| mac::lstat(&x).map_err(err)).and_then(|x| self.same(id, x));
    match st {
      Err(ESTALE | ENOENT) => {
        let h = self.handle_of(id, false).ok_or(ESTALE)?;
        mac::fstat(h.fd.as_raw_fd()).map_err(err)
      }
      x => x,
    }
  }

  fn lookup(&self, parent: u64, name: &CStr) -> Res {
    let dir = self.dir(parent)?;
    match self.entry_at(parent, &dir, name, false) {
      // No such name, cached as long as a name: FSEvents tells the guest when it appears.
      Err(ENOENT) if self.watched.load(Ordering::Relaxed) => reply(&abi::EntryOut {
        entry_valid: self.ttl().as_secs(),
        entry_valid_nsec: self.ttl().subsec_nanos(),
        ..Default::default()
      }),
      x => x,
    }
  }

  /// The entry for `name` in the directory node `parent`, open as `dir`; a lookup for the guest,
  /// or the reply to its making the file (`made`).
  fn entry_at(&self, parent: u64, dir: &OwnedFd, name: &CStr, made: bool) -> Res {
    let (stored, st) = mac::stat_at(dir.as_raw_fd(), name).map_err(err)?;
    // Byte for byte: on a case- or normalization-insensitive volume another spelling finds the
    // file too, and the guest would see one inode under two names.
    if stored != name.to_bytes() {
      return Err(ENOENT);
    }

    let pdev = self.node(parent)?.dev;
    let e = self.entry(&st, pdev)?;
    if made {
      self.told(e.nodeid);
    }

    reply(&e)
  }

  fn entry(&self, st: &Stat, parent_dev: i32) -> Result<abi::EntryOut, i32> {
    let nodeid = self.add(st)?;
    let mut attr = attr(st);
    if st.is_dir() && st.dev != parent_dev {
      attr.flags |= abi::ATTR_SUBMOUNT;
    }

    Ok(abi::EntryOut {
      nodeid,
      generation: 0,
      entry_valid: self.ttl().as_secs(),
      attr_valid: self.ttl().as_secs(),
      entry_valid_nsec: self.ttl().subsec_nanos(),
      attr_valid_nsec: self.ttl().subsec_nanos(),
      attr,
    })
  }

  fn attr_out(&self, id: u64, st: &Stat) -> Res {
    if let Some(x) = self.nodes.lock().unwrap().ids.get_mut(&id) {
      x.seen = Some((st.mtime, st.size));
    }

    reply(&abi::AttrOut {
      attr_valid: self.ttl().as_secs(),
      attr_valid_nsec: self.ttl().subsec_nanos(),
      dummy: 0,
      attr: attr(st),
    })
  }

  /// The node of a file the guest knows.
  fn known(&self, st: &Stat) -> Option<u64> {
    self.nodes.lock().unwrap().keys.get(&(st.dev, st.ino)).copied()
  }

  /// The guest made the node's file: it knows the name was made.
  fn told(&self, id: u64) {
    if let Some(x) = self.nodes.lock().unwrap().ids.get_mut(&id) {
      x.told = true;
    }
  }

  /// Whether the guest is yet to hear that `st`'s name was made, which it then has. Told again
  /// while it has no node, as before.
  fn untold(&self, st: &Stat) -> bool {
    let mut n = self.nodes.lock().unwrap();
    let key = (st.dev, st.ino);
    if let Some(&id) = n.keys.get(&key) {
      return n.ids.get_mut(&id).is_none_or(|x| !std::mem::replace(&mut x.told, true));
    }

    if n.told.len() >= TOLD {
      n.told.clear();
    }

    n.told.insert(key);
    true
  }

  // MARK: Files

  /// The guest's file opened as `fd` for `node`; past `max_handles` ENFILE, as from a system
  /// with no more.
  fn add_handle(&self, node: u64, fd: OwnedFd) -> Result<u64, i32> {
    let mut handles = self.handles.lock().unwrap();
    if handles.len() >= self.max_handles {
      return Err(ENFILE);
    }

    let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
    handles.insert(fh, Arc::new(Handle { fd, node, list: Mutex::default() }));
    Ok(fh)
  }

  fn handle_fd(&self, fh: u64) -> Result<Arc<Handle>, i32> {
    self.handles.lock().unwrap().get(&fh).cloned().ok_or(EINVAL)
  }

  /// A file the guest has open for the node, if any: one open for writing, with `write`.
  fn handle_of(&self, id: u64, write: bool) -> Option<Arc<Handle>> {
    // SAFETY: fcntl on an open file.
    let writes = |x: &Handle| unsafe { libc::fcntl(x.fd.as_raw_fd(), libc::F_GETFL) } & 3 != 0;
    let handles = self.handles.lock().unwrap();
    handles.values().find(|x| x.node == id && (!write || writes(x))).cloned()
  }

  /// The open flags for the guest's file: FOPEN_KEEP_CACHE when it is as the guest last saw it,
  /// else FOPEN_INVAL_ATTR, for a change the guest has no notification of yet.
  fn opened(&self, id: u64, fd: RawFd, linux_flags: u32) -> Result<u32, i32> {
    if linux_flags & mac::O_DIRECT != 0 {
      // SAFETY: fcntl on an open file.
      unsafe { libc::fcntl(fd, libc::F_NOCACHE, 1) };
    }

    let st = mac::fstat(fd).map_err(err)?;
    let now = Some((st.mtime, st.size));
    let n = self.nodes.lock().unwrap();
    let Some(node) = n.ids.get(&id) else { return Ok(0) };
    Ok(if node.seen == now { abi::FOPEN_KEEP_CACHE } else { abi::FOPEN_INVAL_ATTR })
  }

  fn open(&self, id: u64, flags: u32, open_flags: u32) -> Res {
    // Only CREATE makes a file: here one would be made where the node's file was, if gone.
    let fd = self.open_node(id, mac::open_flags(flags) & !(libc::O_CREAT | libc::O_EXCL))?;
    if open_flags & abi::OPEN_KILL_SUIDGID != 0 {
      kill_suidgid(fd.as_raw_fd());
    }

    let open_flags = self.opened(id, fd.as_raw_fd(), flags)?;
    let fh = self.add_handle(id, fd)?;
    reply(&abi::OpenOut { fh, open_flags, ..Default::default() })
  }

  fn create(&self, r: &Req) -> Res {
    let x: abi::CreateIn = arg(r.args, 0)?;
    let (name, _) = name(r.args, size_of::<abi::CreateIn>())?;
    let dir = self.dir(r.h.nodeid)?;
    let made = match mac::stat_at(dir.as_raw_fd(), name) {
      // Another spelling of the name would be opened, and maybe truncated, in its place.
      Ok((stored, _)) if stored != name.to_bytes() => return Err(EEXIST),
      Ok(_) => false,
      Err(_) => true,
    };

    let flags = mac::open_flags(x.flags) | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // A FIFO already there opens at once, as in `open_node`; ESTALE has the guest look it up.
    let fl = flags | libc::O_NONBLOCK;
    let mode = x.mode & 0o7777;
    // SAFETY: a NUL-terminated name in an open directory.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), fl, mode) };
    if fd < 0 {
      return Err(os());
    }

    // SAFETY: a descriptor openat just returned.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // The mode as the guest asks, its umask applied: the Mac's umask would take more.
    if made {
      // SAFETY: fchmod on an open file.
      unsafe { libc::fchmod(fd.as_raw_fd(), mode as u16) };
    }

    let st = mac::fstat(fd.as_raw_fd()).map_err(err)?;
    let fd = plain(fd, &st, flags)?;
    let pdev = self.node(r.h.nodeid)?.dev;
    let entry = self.entry(&st, pdev)?;
    self.told(entry.nodeid);
    let open_flags = self.opened(entry.nodeid, fd.as_raw_fd(), x.flags)?;
    let fh = self.add_handle(entry.nodeid, fd).inspect_err(|_| self.forget(entry.nodeid, 1))?;
    let open = abi::OpenOut { fh, open_flags, ..Default::default() };
    let mut out = abi::bytes(&entry).to_vec();
    out.extend_from_slice(abi::bytes(&open));
    Ok(Out::Bytes(out))
  }

  fn read(&self, r: &Req) -> Res {
    let x: abi::ReadIn = arg(r.args, 0)?;
    let h = self.handle_fd(x.fh)?;
    let at = size_of::<abi::OutHeader>();
    let len = (x.size.min(MAX_IO) as usize).min(r.chain.write_len().saturating_sub(at));
    let mut done = 0;
    while done < len {
      let iov = r.chain.iovecs(r.mem, true, at + done, len - done).map_err(err)?;
      // SAFETY: the iovecs point into guest memory, checked by `iovecs`.
      let n = unsafe {
        libc::preadv(
          h.fd.as_raw_fd(),
          iov.as_ptr(),
          iov.len() as i32,
          (x.offset + done as u64) as i64,
        )
      };
      match n {
        0 => break,
        n if n > 0 => done += n as usize,
        _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
        _ => return Err(os()),
      }
    }

    Ok(Out::Written(done))
  }

  fn write(&self, r: &Req) -> Res {
    let x: abi::WriteIn = arg(r.args, 0)?;
    let h = self.handle_fd(x.fh)?;
    if x.write_flags & abi::WRITE_KILL_SUIDGID != 0 {
      kill_suidgid(h.fd.as_raw_fd());
    }

    let at = size_of::<abi::InHeader>() + size_of::<abi::WriteIn>();
    let len = (x.size.min(MAX_IO) as usize).min(r.chain.read_len().saturating_sub(at));
    let mut done = 0;
    while done < len {
      let iov = r.chain.iovecs(r.mem, false, at + done, len - done).map_err(err)?;
      // SAFETY: as in `read`.
      let n = unsafe {
        libc::pwritev(
          h.fd.as_raw_fd(),
          iov.as_ptr(),
          iov.len() as i32,
          (x.offset + done as u64) as i64,
        )
      };
      match n {
        n if n > 0 => done += n as usize,
        _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
        _ if done > 0 => break,
        _ => return Err(os()),
      }
    }

    reply(&abi::WriteOut { size: done as u32, padding: 0 })
  }

  /// copy_file_range: from one open file to another on the Mac, the data never in the guest.
  /// macOS clones only whole files into new ones, so this copies.
  fn copy(&self, x: &abi::CopyFileRangeIn) -> Result<u64, i32> {
    let (from, to) = (self.handle_fd(x.fh_in)?, self.handle_fd(x.fh_out)?);
    // To the source's end as it is now, which a copy into the same file would chase.
    let size = mac::fstat(from.fd.as_raw_fd()).map_err(err)?.size;
    let len = x.len.min(size.saturating_sub(x.off_in)).min(MAX_COPY);
    let mut buf = vec![0u8; len.min(MAX_IO as u64) as usize];
    let mut done = 0;
    while done < len {
      let want = (len - done).min(buf.len() as u64) as usize;
      let at = (x.off_in + done) as i64;
      // SAFETY: pread writes at most `want` bytes into the buffer.
      let n = unsafe { libc::pread(from.fd.as_raw_fd(), buf.as_mut_ptr().cast(), want, at) };
      let n = match n {
        0 => return Ok(done),
        n if n > 0 => n as usize,
        _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
        _ if done > 0 => return Ok(done),
        _ => return Err(os()),
      };

      let mut put = 0;
      while put < n {
        let at = (x.off_out + done + put as u64) as i64;
        let rest = &buf[put..n];
        // SAFETY: pwrite reads `rest`, which is in the buffer.
        let w = unsafe { libc::pwrite(to.fd.as_raw_fd(), rest.as_ptr().cast(), rest.len(), at) };
        match w {
          w if w > 0 => put += w as usize,
          _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {}
          _ if done + (put as u64) > 0 => return Ok(done + put as u64),
          _ => return Err(os()),
        }
      }

      done += n as u64;
    }

    Ok(done)
  }

  fn setattr(&self, r: &Req) -> Res {
    let x: abi::SetattrIn = arg(r.args, 0)?;
    let id = r.h.nodeid;
    let node = self.node(id)?;
    let v = x.valid;

    // By a descriptor, as a name may lead to another file by the time it is used: the guest's
    // open file, one opened to truncate, which needs write permission anyway, or one opened for
    // this alone. By the name only where none opens.
    let (handle, file);
    let fd = if v & abi::FATTR_FH != 0 {
      handle = self.handle_fd(x.fh)?;
      handle.fd.as_raw_fd()
    } else if v & abi::FATTR_SIZE != 0 && node.kind == S_IFREG {
      // No I/O through it: it may stay non-blocking. With no name left, by the guest's file open
      // for writing, as truncate on /proc/self/fd.
      match self.open_node(id, libc::O_WRONLY | libc::O_NONBLOCK) {
        Err(ESTALE) => {
          handle = self.handle_of(id, true).ok_or(ESTALE)?;
          handle.fd.as_raw_fd()
        }
        x => {
          file = x?;
          file.as_raw_fd()
        }
      }
    } else if let Some(h) = self.handle_of(id, false) {
      handle = h;
      handle.fd.as_raw_fd()
    } else if let Some(x) = self.open_any(id, node.kind)? {
      file = x;
      file.as_raw_fd()
    } else {
      return self.setattr_at(id, node.kind, &x);
    };

    // SAFETY: fchmod on an open descriptor.
    if v & abi::FATTR_MODE != 0 && unsafe { libc::fchmod(fd, (x.mode & 0o7777) as u16) } != 0 {
      return Err(os());
    }

    // SAFETY: ftruncate on an open descriptor.
    if v & abi::FATTR_SIZE != 0 && unsafe { libc::ftruncate(fd, x.size as i64) } != 0 {
      return Err(os());
    }

    if v & abi::FATTR_KILL_SUIDGID != 0 {
      kill_suidgid(fd);
    }

    // SAFETY: futimens reads two timespecs, on an open descriptor.
    if v & (abi::FATTR_ATIME | abi::FATTR_MTIME) != 0
      && unsafe { libc::futimens(fd, times(&x).as_ptr()) } != 0
    {
      return Err(os());
    }

    // FATTR_UID and FATTR_GID: the Mac user can't give a file away; chown succeeds and changes
    // nothing, as with Virtualization.framework.
    self.attr_out(id, &mac::fstat(fd).map_err(err)?)
  }

  /// The node's file opened with what access the user has, for its attributes alone; none for a
  /// file the user may neither read nor write, a symlink, a FIFO, whose open waits for the other
  /// end, or a socket.
  fn open_any(&self, id: u64, kind: u32) -> Result<Option<OwnedFd>, i32> {
    let tries: &[i32] = match kind {
      S_IFREG => &[libc::O_RDONLY | libc::O_NONBLOCK, libc::O_WRONLY | libc::O_NONBLOCK],
      S_IFDIR => &[libc::O_RDONLY | libc::O_DIRECTORY, libc::O_SEARCH | libc::O_DIRECTORY],
      _ => &[],
    };

    for &flags in tries {
      match self.open_node(id, flags) {
        Err(EACCES) => {}
        x => return x.map(Some),
      }
    }

    Ok(None)
  }

  /// SETATTR by the node's name in its directory. A symlink's mode has no meaning, a file not
  /// regular has no size here, and the owner is fixed.
  fn setattr_at(&self, id: u64, kind: u32, x: &abi::SetattrIn) -> Res {
    let v = x.valid;
    let (dir, name) = self.parent_of(id)?;
    let (at, file) = (dir.as_raw_fd(), name.as_ptr());
    let mut mode = (v & abi::FATTR_MODE != 0).then_some(x.mode & 0o7777);
    // With a chown, which changes nothing else.
    if v & abi::FATTR_KILL_SUIDGID != 0 && kind == S_IFREG {
      let now = match mode {
        Some(x) => x,
        None => self.stat(id)?.mode,
      };

      mode = Some(killed(now));
    }

    // SAFETY: a NUL-terminated name in an open directory.
    if let Some(mode) = mode
      && kind != S_IFLNK
      && unsafe { libc::fchmodat(at, file, mode as u16, libc::AT_SYMLINK_NOFOLLOW) } != 0
    {
      return Err(os());
    }

    let ts = times(x);
    // SAFETY: two timespecs and a NUL-terminated name in an open directory.
    if v & (abi::FATTR_ATIME | abi::FATTR_MTIME) != 0
      && unsafe { libc::utimensat(at, file, ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0
    {
      return Err(os());
    }

    self.attr_out(id, &self.stat(id)?)
  }

  fn rename(&self, from: u64, to: u64, flags: u32, args: &[u8], at: usize) -> Res {
    let (old, at) = name(args, at)?;
    let (new, _) = name(args, at)?;
    // Linux's RENAME_NOREPLACE and RENAME_EXCHANGE; RENAME_WHITEOUT is overlayfs' own.
    if flags & !3 != 0 {
      return Err(EINVAL);
    }

    let bits = [(1, libc::RENAME_EXCL), (2, libc::RENAME_SWAP)];
    let f = bits.into_iter().filter(|&(l, _)| flags & l != 0).fold(0, |f, (_, m)| f | m);
    let (a, b) = (self.dir(from)?, self.dir(to)?);
    // Another spelling of the new name would be replaced, unless it is the file renamed: a
    // change of case.
    if let Some(st) = other_spelling(b.as_raw_fd(), new)
      && mac::stat_at(a.as_raw_fd(), old).map_or(true, |(_, x)| (x.dev, x.ino) != (st.dev, st.ino))
    {
      return Err(EEXIST);
    }

    // SAFETY: NUL-terminated names in open directories.
    if unsafe { libc::renameatx_np(a.as_raw_fd(), old.as_ptr(), b.as_raw_fd(), new.as_ptr(), f) }
      != 0
    {
      return Err(os());
    }

    Ok(Out::Bytes(Vec::new()))
  }

  fn readdir(&self, r: &Req, plus: bool) -> Res {
    let x: abi::ReadIn = arg(r.args, 0)?;
    let h = self.handle_fd(x.fh)?;
    // Moved out of the share on the Mac while open, it is no longer the guest's to list.
    self.path(h.node)?;
    let fd = h.fd.as_raw_fd();
    let mut list = h.list.lock().unwrap();
    // An offset is the index of the next entry, `.` and `..` first. The handle keeps only what
    // the guest is about to read: from the start or from before that, it is listed again.
    let from = (x.offset as usize).saturating_sub(2);
    if x.offset == 0 || from < list.base || list.lost {
      mac::rewind(fd).map_err(err)?;
      *list = Listing::default();
    }

    let l = &mut *list;
    // What the guest read is dropped, what it skips listed and dropped.
    loop {
      let skip = (from - l.base).min(l.entries.len());
      l.entries.drain(..skip);
      l.base += skip;
      if l.base == from || l.done {
        break;
      }

      l.more(fd)?;
    }

    let me = mac::fstat(fd).map_err(err)?;
    let size = (x.size.min(MAX_IO) as usize)
      .min(r.chain.write_len().saturating_sub(size_of::<abi::OutHeader>()));
    let mut out = Vec::with_capacity(size);
    let mut i = x.offset as usize;
    loop {
      if i >= 2 && i - 2 - l.base >= l.entries.len() {
        if l.done {
          break;
        }

        l.more(fd)?;
        continue;
      }

      let (name, st): (&[u8], &Stat) = match i {
        0 => (b".", &me),
        1 => (b"..", &me),
        _ => {
          let (name, st) = &l.entries[i - 2 - l.base];
          (name, st)
        }
      };

      let need = if plus { abi::direntplus_size(name.len()) } else { abi::dirent_size(name.len()) };
      if out.len() + need > size {
        break;
      }

      if plus {
        // `.` and `..` get no entry: the kernel counts no lookup for them; nor does a file past
        // the nodes' limit.
        let e =
          if i < 2 { abi::EntryOut::default() } else { self.entry(st, me.dev).unwrap_or_default() };
        out.extend_from_slice(abi::bytes(&e));
      }

      let d = abi::Dirent {
        ino: st.ino,
        off: i as u64 + 1,
        namelen: name.len() as u32,
        typ: st.kind() >> 12,
      };
      out.extend_from_slice(abi::bytes(&d));
      out.extend_from_slice(name);
      out.resize(out.len().next_multiple_of(8), 0);
      i += 1;
    }

    Ok(Out::Bytes(out))
  }

  // MARK: Extended attributes

  /// One attribute's value (`name`), or the list of names; with `size` 0, only how long it is.
  fn xattr_get(&self, id: u64, name: Option<&CStr>, size: u32) -> Res {
    // A list goes into a full buffer: it has the com.apple.* names, which the guest's size
    // doesn't count.
    let full = size == 0 || name.is_none();
    let mut buf = vec![0u8; if full { MAX_XATTR } else { (size as usize).min(MAX_XATTR) }];
    let (ptr, len): (*mut libc::c_void, _) = (buf.as_mut_ptr().cast(), buf.len());
    let done = |n: isize| match n {
      n if n >= 0 => Ok(n as usize),
      _ if full && os() == ERANGE => Err(E2BIG), // longer than Linux allows
      _ => Err(os()),
    };
    let n = self.on_file(
      id,
      // SAFETY: the calls write at most the buffer's size; the strings are NUL-terminated.
      |fd| unsafe {
        done(match name {
          Some(x) => libc::fgetxattr(fd, x.as_ptr(), ptr, len, 0, 0),
          None => libc::flistxattr(fd, ptr.cast(), len, 0),
        })
      },
      // SAFETY: as for a descriptor.
      |file| unsafe {
        done(match name {
          Some(x) => libc::getxattr(file.as_ptr(), x.as_ptr(), ptr, len, 0, libc::XATTR_NOFOLLOW),
          None => libc::listxattr(file.as_ptr(), ptr.cast(), len, libc::XATTR_NOFOLLOW),
        })
      },
    )?;

    buf.truncate(n);
    if name.is_none() {
      let names = buf.split(|&x| x == 0).filter(|x| !x.is_empty() && !hidden(x));
      buf = names.flat_map(|x| x.iter().copied().chain([0])).collect();
    }

    if size == 0 {
      return reply(&abi::GetxattrOut { size: buf.len() as u32, padding: 0 });
    }

    if buf.len() > size as usize {
      return Err(ERANGE);
    }

    Ok(Out::Bytes(buf))
  }

  fn setxattr(&self, r: &Req) -> Res {
    // Without FUSE_SETXATTR_EXT the kernel sends the short form: size and flags.
    let x: abi::SetxattrIn = arg(&pad(r.args, size_of::<abi::SetxattrIn>()), 0)?;
    let (name, at) = cstr(r.args, abi::COMPAT_SETXATTR_IN_SIZE)?;
    let value = r.args.get(at..at + x.size as usize).ok_or(EINVAL)?;
    if hidden(name.to_bytes()) {
      return Err(EPERM);
    }

    // Linux's XATTR_CREATE and XATTR_REPLACE are 1 and 2.
    let bits = [(1, libc::XATTR_CREATE), (2, libc::XATTR_REPLACE)];
    let opts = bits.into_iter().filter(|&(l, _)| x.flags & l != 0).fold(0, |f, (_, m)| f | m);
    let (v, len) = (value.as_ptr().cast(), value.len());
    let done = |r: i32| if r == 0 { Ok(Out::Bytes(Vec::new())) } else { Err(os()) };
    self.on_file(
      r.h.nodeid,
      // SAFETY: NUL-terminated strings and a value of its length.
      |fd| done(unsafe { libc::fsetxattr(fd, name.as_ptr(), v, len, 0, opts) }),
      // SAFETY: as for a descriptor.
      |file| {
        let opts = opts | libc::XATTR_NOFOLLOW;
        done(unsafe { libc::setxattr(file.as_ptr(), name.as_ptr(), v, len, 0, opts) })
      },
    )
  }
}

// MARK: Helpers

/// A notification: a reply with no request (`unique` 0), its code in place of the error.
fn note(code: i32, parts: &[&[u8]]) -> Vec<u8> {
  let len = size_of::<abi::OutHeader>() + parts.iter().map(|x| x.len()).sum::<usize>();
  let h = abi::OutHeader { len: len as u32, error: code, unique: 0 };
  let mut v = abi::bytes(&h).to_vec();
  for x in parts {
    v.extend_from_slice(x);
  }

  v
}

/// Every name the guest cached is looked up again: for when changes went unseen.
pub fn inc_epoch() -> Vec<u8> {
  note(abi::NOTIFY_INC_EPOCH, &[])
}

/// The file `name` finds in `dir` when it is stored under another spelling: a case- or
/// normalization-insensitive volume would open or replace it for `name`.
fn other_spelling(dir: RawFd, name: &CStr) -> Option<Stat> {
  match mac::stat_at(dir, name) {
    Ok((stored, st)) if stored != name.to_bytes() => Some(st),
    _ => None,
  }
}

/// A path's directory and name: `/a/b` is `/a` and `b`, `/a` is `/` and `a`.
fn split(path: &[u8]) -> Option<(&[u8], &[u8])> {
  let cut = path.iter().rposition(|&x| x == b'/')?;
  Some((&path[..cut.max(1)], &path[cut + 1..]))
}

fn lstat(path: &[u8]) -> Option<Stat> {
  mac::lstat(&CString::new(path).ok()?).ok()
}

fn arg<T: Pod>(args: &[u8], at: usize) -> Result<T, i32> {
  args.get(at..).and_then(abi::read).ok_or(EINVAL)
}

/// `args` zero-padded to `len`, for a struct the kernel sends in a shorter form.
fn pad(args: &[u8], len: usize) -> Vec<u8> {
  let mut v = args.to_vec();
  if v.len() < len {
    v.resize(len, 0);
  }

  v
}

/// A NUL-terminated string at `at`, and where what follows it starts.
fn cstr(args: &[u8], at: usize) -> Result<(&CStr, usize), i32> {
  let rest = args.get(at..).ok_or(EINVAL)?;
  let s = CStr::from_bytes_until_nul(rest).map_err(|_| EINVAL)?;
  Ok((s, at + s.to_bytes().len() + 1))
}

/// A name in a directory: one component, never `.` or `..`.
fn name(args: &[u8], at: usize) -> Result<(&CStr, usize), i32> {
  let (s, next) = cstr(args, at)?;
  let b = s.to_bytes();
  if b.is_empty() || b == b"." || b == b".." || b.contains(&b'/') {
    return Err(EINVAL);
  }

  Ok((s, next))
}

/// An extended attribute macOS keeps to itself: the guest neither sees nor changes it.
fn hidden(name: &[u8]) -> bool {
  name.starts_with(b"com.apple.")
}

fn open(path: &CStr, flags: i32) -> Result<OwnedFd, i32> {
  // No symlink anywhere on the way: one planted in a directory between `fsgetpath` and here
  // would lead out of the share.
  let flags = flags | libc::O_NOFOLLOW_ANY | libc::O_CLOEXEC;
  // SAFETY: a NUL-terminated path.
  let fd = unsafe { libc::open(path.as_ptr(), flags) };
  if fd < 0 {
    return Err(match os() {
      ELOOP => ESTALE, // a symlink where the file was
      x => x,
    });
  }

  // SAFETY: a descriptor open just returned.
  Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// `fd`, opened as `st`, if it is a regular file or a directory; blocking again unless `flags`
/// asked otherwise, when opened with O_NONBLOCK added. Anything else the guest knows as another
/// file: ESTALE has it look the name up again.
fn plain(fd: OwnedFd, st: &Stat, flags: i32) -> Result<OwnedFd, i32> {
  if st.kind() != S_IFREG && st.kind() != S_IFDIR {
    return Err(ESTALE);
  }

  let off: libc::c_int = 0;
  // SAFETY: FIONBIO reads one int, on an open file.
  if flags & (libc::O_NONBLOCK | libc::O_DIRECTORY) == 0
    && unsafe { libc::ioctl(fd.as_raw_fd(), libc::FIONBIO, &off) } != 0
  {
    return Err(os());
  }

  Ok(fd)
}

/// Makes `name` in `dir` as MKNOD asks. The Mac user may make no devices; a regular file, a
/// FIFO and a socket's file it may.
fn mknod(dir: RawFd, name: &CStr, mode: u32) -> Result<(), i32> {
  let perm = (mode & 0o7777) as u16;
  let r = match mode & S_IFMT {
    S_IFREG => {
      let flags = libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC;
      // SAFETY: a NUL-terminated name in an open directory.
      let fd = unsafe { libc::openat(dir, name.as_ptr(), flags, perm as u32) };
      if fd >= 0 {
        // SAFETY: the descriptor openat just returned, closed once.
        unsafe { libc::close(fd) };
      }

      fd.min(0)
    }
    // SAFETY: as for S_IFREG.
    0o010000 => unsafe { libc::mkfifoat(dir, name.as_ptr(), perm) },
    0o140000 => return bind_at(dir, name, perm),
    _ => return Err(EPERM),
  };

  if r != 0 {
    return Err(os());
  }

  // SAFETY: as for S_IFREG. The mode as the guest asks, as in CREATE.
  unsafe { libc::fchmodat(dir, name.as_ptr(), perm, libc::AT_SYMLINK_NOFOLLOW) };
  Ok(())
}

unsafe extern "C" {
  /// libsystem_pthread, not in the SDK's headers: the calling thread's own working directory,
  /// the process's again for -1. macOS has no bindat().
  fn pthread_fchdir_np(fd: libc::c_int) -> libc::c_int;
  /// unistd.h: writes what the volume of `fd` holds in memory, as Linux's syncfs.
  fn fsync_volume_np(fd: libc::c_int, flags: libc::c_int) -> libc::c_int;
}

/// unistd.h: fsync_volume_np waits for the writes it starts.
const SYNC_VOLUME_WAIT: libc::c_int = 0x02;

/// A Unix socket's file, as bind() makes it: the guest's socket lives in the guest kernel, the
/// file only marks the name. Bound by the name alone from this thread's working directory, so a
/// long path doesn't matter and no symlink on the way is followed.
fn bind_at(dir: RawFd, name: &CStr, perm: u16) -> Result<(), i32> {
  let b = name.to_bytes();
  // SAFETY: sockaddr_un is integers and bytes, all zero a valid value.
  let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
  if b.len() >= addr.sun_path.len() {
    return Err(ENAMETOOLONG);
  }

  addr.sun_family = libc::AF_UNIX as u8;
  for (d, s) in addr.sun_path.iter_mut().zip(b) {
    *d = *s as libc::c_char;
  }

  // SAFETY: a fresh socket, bound once to a NUL-terminated name in this thread's working
  // directory, which goes back to the process's before returning; then closed.
  unsafe {
    let s = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
    if s < 0 {
      return Err(os());
    }

    if pthread_fchdir_np(dir) != 0 {
      let e = os();
      libc::close(s);
      return Err(e);
    }

    let len = size_of::<libc::sockaddr_un>() as libc::socklen_t;
    let r = libc::bind(s, (&addr as *const libc::sockaddr_un).cast(), len);
    let e = os();
    pthread_fchdir_np(-1);
    libc::close(s);
    if r != 0 {
      return Err(e);
    }

    libc::fchmodat(dir, name.as_ptr(), perm, libc::AT_SYMLINK_NOFOLLOW);
  }

  Ok(())
}

/// Clears setuid, and setgid on a group-executable file, as Linux does on a write by another.
fn kill_suidgid(fd: RawFd) {
  let Ok(st) = mac::fstat(fd) else { return };
  let mode = killed(st.mode);
  if mode != st.mode & 0o7777 {
    // SAFETY: fchmod on an open file.
    unsafe { libc::fchmod(fd, mode as u16) };
  }
}

/// `mode`'s permissions with setuid cleared, and setgid if group-executable.
fn killed(mode: u32) -> u32 {
  let mut mode = mode & 0o7777 & !0o4000;
  if mode & 0o010 != 0 {
    mode &= !0o2000;
  }

  mode
}

/// The times SETATTR sets: UTIME_OMIT for those it leaves, UTIME_NOW for "now".
fn times(x: &abi::SetattrIn) -> [libc::timespec; 2] {
  let one = |set: u32, now: u32, sec: u64, nsec: u32| match () {
    _ if x.valid & set == 0 => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
    _ if x.valid & now != 0 => libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_NOW },
    _ => libc::timespec { tv_sec: sec as i64, tv_nsec: nsec as i64 },
  };
  [
    one(abi::FATTR_ATIME, abi::FATTR_ATIME_NOW, x.atime, x.atimensec),
    one(abi::FATTR_MTIME, abi::FATTR_MTIME_NOW, x.mtime, x.mtimensec),
  ]
}

/// A file's attributes as the guest sees them: owned by root, whom the Mac user stands for.
fn attr(st: &Stat) -> abi::Attr {
  abi::Attr {
    ino: st.ino,
    size: st.size,
    blocks: st.blocks,
    atime: st.atime.0 as u64,
    mtime: st.mtime.0 as u64,
    ctime: st.ctime.0 as u64,
    atimensec: st.atime.1,
    mtimensec: st.mtime.1,
    ctimensec: st.ctime.1,
    mode: st.mode,
    nlink: st.nlink,
    uid: 0,
    gid: 0,
    rdev: st.rdev,
    blksize: st.blksize,
    flags: 0,
  }
}

#[cfg(test)]
mod tests {
  use std::os::unix::ffi::OsStrExt;
  use std::path::PathBuf;

  use super::*;

  const BASE: u64 = 0x4000_0000;
  const OUT: u64 = BASE + (1 << 20);

  /// A server on a temporary folder, and guest memory to pass it requests through.
  struct T {
    mem: Mem,
    s: Server,
    dir: PathBuf,
    unique: u64,
  }

  impl T {
    fn new(name: &str) -> T {
      let dir = std::env::temp_dir().join(format!("lodka-fs-{name}-{}", std::process::id()));
      let _ = std::fs::remove_dir_all(&dir);
      std::fs::create_dir_all(&dir).unwrap();
      let s = Server::new(&dir).unwrap();
      T { mem: Mem::new(BASE, 4 << 20).unwrap(), s, dir, unique: 0 }
    }

    /// Sends a request; returns the reply's error and body.
    fn call(&mut self, opcode: u32, nodeid: u64, args: &[u8], out: usize) -> (i32, Vec<u8>) {
      self.unique += 1;
      let len = size_of::<abi::InHeader>() + args.len();
      let h = abi::InHeader {
        len: len as u32,
        opcode,
        unique: self.unique,
        nodeid,
        ..Default::default()
      };
      self.mem.write(BASE, abi::bytes(&h)).unwrap();
      self.mem.write(BASE + size_of::<abi::InHeader>() as u64, args).unwrap();
      let chain =
        Chain { head: 0, read: vec![(BASE, len as u32)], write: vec![(OUT, 16 + out as u32)] };
      let n = self.s.serve(&self.mem, &chain) as usize;
      if n == 0 {
        return (0, Vec::new());
      }

      let mut rep = vec![0u8; n];
      self.mem.read(OUT, &mut rep).unwrap();
      let oh: abi::OutHeader = abi::read(&rep).unwrap();
      assert_eq!((oh.unique, oh.len as usize), (self.unique, n));
      (oh.error, rep[16..].to_vec())
    }

    fn lookup(&mut self, parent: u64, name: &str) -> Result<abi::EntryOut, i32> {
      let (e, b) = self.call(abi::LOOKUP, parent, &z(name), 256);
      match e {
        0 => Ok(abi::read(&b).unwrap()),
        e => Err(-e),
      }
    }
  }

  impl Drop for T {
    fn drop(&mut self) {
      let _ = std::fs::remove_dir_all(&self.dir);
    }
  }

  /// A name as the kernel sends it, NUL-terminated.
  fn z(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
  }

  fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
  }

  #[test]
  fn lookup_matches_names_byte_for_byte() {
    let mut t = T::new("lookup");
    std::fs::write(t.dir.join("README.md"), b"hi").unwrap();
    let e = t.lookup(abi::ROOT_ID, "README.md").unwrap();
    assert_eq!((e.attr.size, e.attr.mode & S_IFMT), (2, S_IFREG));
    assert_eq!(t.lookup(abi::ROOT_ID, "readme.md").unwrap_err(), ENOENT);
    assert_eq!(t.lookup(abi::ROOT_ID, "nope").unwrap_err(), ENOENT);
    assert_eq!(t.lookup(abi::ROOT_ID, "..").unwrap_err(), EINVAL);

    // The longest names APFS takes, 255 characters of 3 bytes each.
    let long = "語".repeat(255);
    std::fs::write(t.dir.join(&long), b"").unwrap();
    assert!(t.lookup(abi::ROOT_ID, &long).is_ok());

    // The same file gets the same node; forgotten as often as looked up, it goes.
    let again = t.lookup(abi::ROOT_ID, "README.md").unwrap();
    assert_eq!(again.nodeid, e.nodeid);
    t.call(abi::FORGET, e.nodeid, abi::bytes(&abi::ForgetIn { nlookup: 2 }), 0);
    assert!(t.s.node(e.nodeid).is_err());
  }

  #[test]
  fn create_write_read_rename() {
    let mut t = T::new("rw");
    let c = abi::CreateIn { flags: 2, mode: 0o100644, ..Default::default() }; // O_RDWR
    let (e, b) = t.call(abi::CREATE, abi::ROOT_ID, &cat(&[abi::bytes(&c), &z("a.txt")]), 256);
    assert_eq!(e, 0);
    let entry: abi::EntryOut = abi::read(&b).unwrap();
    let open: abi::OpenOut = abi::read(&b[size_of::<abi::EntryOut>()..]).unwrap();

    let w = abi::WriteIn { fh: open.fh, offset: 0, size: 5, ..Default::default() };
    let (e, b) = t.call(abi::WRITE, entry.nodeid, &cat(&[abi::bytes(&w), b"hello"]), 16);
    assert_eq!((e, abi::read::<abi::WriteOut>(&b).unwrap().size), (0, 5));

    let rd = abi::ReadIn { fh: open.fh, offset: 1, size: 100, ..Default::default() };
    let (e, b) = t.call(abi::READ, entry.nodeid, abi::bytes(&rd), 100);
    assert_eq!((e, b.as_slice()), (0, &b"ello"[..]));

    let rn = abi::RenameIn { newdir: abi::ROOT_ID };
    let (e, _) =
      t.call(abi::RENAME, abi::ROOT_ID, &cat(&[abi::bytes(&rn), &z("a.txt"), &z("A.txt")]), 0);
    assert_eq!(e, 0);
    assert_eq!(t.lookup(abi::ROOT_ID, "A.txt").unwrap().nodeid, entry.nodeid);
    assert_eq!(std::fs::read(t.dir.join("A.txt")).unwrap(), b"hello");

    // a.txt is A.txt on this volume: neither made over it nor moved onto it.
    let c = abi::CreateIn { flags: 0o1002, mode: 0o100644, ..Default::default() }; // O_TRUNC
    let (e, _) = t.call(abi::CREATE, abi::ROOT_ID, &cat(&[abi::bytes(&c), &z("a.txt")]), 256);
    assert_eq!(e, -EEXIST);
    std::fs::write(t.dir.join("b"), b"").unwrap();
    let (e, _) =
      t.call(abi::RENAME, abi::ROOT_ID, &cat(&[abi::bytes(&rn), &z("b"), &z("a.txt")]), 0);
    assert_eq!(e, -EEXIST);
    assert_eq!(std::fs::read(t.dir.join("A.txt")).unwrap(), b"hello");

    let ga = abi::GetattrIn::default();
    let (e, b) = t.call(abi::GETATTR, entry.nodeid, abi::bytes(&ga), 256);
    assert_eq!((e, abi::read::<abi::AttrOut>(&b).unwrap().attr.size), (0, 5));
    t.call(
      abi::RELEASE,
      entry.nodeid,
      abi::bytes(&abi::ReleaseIn { fh: open.fh, ..Default::default() }),
      0,
    );
  }

  #[test]
  fn readdirplus_lists_with_attributes() {
    let mut t = T::new("readdir");
    std::fs::create_dir(t.dir.join("sub")).unwrap();
    std::fs::write(t.dir.join("f"), b"12345").unwrap();
    std::os::unix::fs::symlink("f", t.dir.join("l")).unwrap();
    let (e, b) = t.call(abi::OPENDIR, abi::ROOT_ID, abi::bytes(&abi::OpenIn::default()), 64);
    assert_eq!(e, 0);
    let fh = abi::read::<abi::OpenOut>(&b).unwrap().fh;

    let rd = abi::ReadIn { fh, offset: 0, size: 4096, ..Default::default() };
    let (e, b) = t.call(abi::READDIRPLUS, abi::ROOT_ID, abi::bytes(&rd), 4096);
    assert_eq!(e, 0);
    let mut at = 0;
    let mut got = Vec::new();
    while at < b.len() {
      let entry: abi::EntryOut = abi::read(&b[at..]).unwrap();
      let d: abi::Dirent = abi::read(&b[at + size_of::<abi::EntryOut>()..]).unwrap();
      let from = at + size_of::<abi::EntryOut>() + size_of::<abi::Dirent>();
      let name = String::from_utf8(b[from..from + d.namelen as usize].to_vec()).unwrap();
      got.push((name, entry.attr.size, entry.nodeid != 0));
      at += abi::direntplus_size(d.namelen as usize);
    }

    got.sort();
    let names: Vec<_> = got.iter().map(|x| x.0.as_str()).collect();
    assert_eq!(names, [".", "..", "f", "l", "sub"]);
    assert_eq!(got[2], ("f".into(), 5, true));
    assert!(!got[0].2, "no lookup for `.`");

    // Read on from the last offset: nothing left.
    let rd = abi::ReadIn { fh, offset: 5, size: 4096, ..Default::default() };
    let (e, b) = t.call(abi::READDIR, abi::ROOT_ID, abi::bytes(&rd), 4096);
    assert_eq!((e, b.len()), (0, 0));
  }

  #[test]
  fn mkdir_symlink_unlink_setattr() {
    let mut t = T::new("ops");
    let m = abi::MkdirIn { mode: 0o755, umask: 0 };
    let (e, b) = t.call(abi::MKDIR, abi::ROOT_ID, &cat(&[abi::bytes(&m), &z("d")]), 256);
    assert_eq!(e, 0);
    let d: abi::EntryOut = abi::read(&b).unwrap();
    assert_eq!(d.attr.mode & S_IFMT, S_IFDIR);

    let (e, b) = t.call(abi::SYMLINK, d.nodeid, &cat(&[&z("link"), &z("../x")]), 256);
    assert_eq!(e, 0);
    let l: abi::EntryOut = abi::read(&b).unwrap();
    let (e, b) = t.call(abi::READLINK, l.nodeid, &[], 256);
    assert_eq!((e, b.as_slice()), (0, &b"../x"[..]));

    std::fs::write(t.dir.join("d/f"), b"0123456789").unwrap();
    let f = t.lookup(d.nodeid, "f").unwrap();
    let sa = abi::SetattrIn {
      valid: abi::FATTR_SIZE | abi::FATTR_MODE,
      size: 3,
      mode: 0o600,
      ..Default::default()
    };
    let (e, b) = t.call(abi::SETATTR, f.nodeid, abi::bytes(&sa), 256);
    assert_eq!(e, 0);
    let a: abi::AttrOut = abi::read(&b).unwrap();
    assert_eq!((a.attr.size, a.attr.mode & 0o7777), (3, 0o600));

    let (e, _) = t.call(abi::UNLINK, d.nodeid, &z("f"), 0);
    assert_eq!(e, 0);
    // The file is gone, so its node is stale.
    let (e, _) = t.call(abi::GETATTR, f.nodeid, abi::bytes(&abi::GetattrIn::default()), 256);
    assert_eq!(e, -ESTALE);
    let (e, _) = t.call(abi::RMDIR, abi::ROOT_ID, &z("d"), 0);
    assert_eq!(e, -39, "ENOTEMPTY in Linux's numbers");
  }

  #[test]
  fn xattrs_hide_apple_names() {
    let mut t = T::new("xattr");
    std::fs::write(t.dir.join("f"), b"").unwrap();
    let f = t.lookup(abi::ROOT_ID, "f").unwrap();
    let sx = abi::SetxattrIn { size: 3, flags: 0, ..Default::default() };
    let args = cat(&[&abi::bytes(&sx)[..abi::COMPAT_SETXATTR_IN_SIZE], &z("user.a"), b"abc"]);
    assert_eq!(t.call(abi::SETXATTR, f.nodeid, &args, 0).0, 0);
    let p = CString::new(t.dir.join("f").as_os_str().as_bytes()).unwrap();
    // SAFETY: NUL-terminated strings and a 1-byte value.
    unsafe {
      libc::setxattr(p.as_ptr(), c"com.apple.test".as_ptr(), b"x".as_ptr().cast(), 1, 0, 0)
    };

    // The size the guest is told, then a list of that size.
    let gx = abi::GetxattrIn { size: 0, padding: 0 };
    let (e, b) = t.call(abi::LISTXATTR, f.nodeid, abi::bytes(&gx), 100);
    assert_eq!((e, abi::read::<abi::GetxattrOut>(&b).unwrap().size), (0, 7));
    let gx = abi::GetxattrIn { size: 7, padding: 0 };
    let (e, b) = t.call(abi::LISTXATTR, f.nodeid, abi::bytes(&gx), 100);
    assert_eq!((e, b.as_slice()), (0, &b"user.a\0"[..]));
    let gx = abi::GetxattrIn { size: 100, padding: 0 };
    let (e, b) = t.call(abi::GETXATTR, f.nodeid, &cat(&[abi::bytes(&gx), &z("user.a")]), 100);
    assert_eq!((e, b.as_slice()), (0, &b"abc"[..]));
    let (e, _) =
      t.call(abi::GETXATTR, f.nodeid, &cat(&[abi::bytes(&gx), &z("com.apple.test")]), 100);
    assert_eq!(e, -ENODATA);
  }

  #[test]
  fn mac_changes_become_notifications() {
    let mut t = T::new("changed");
    std::fs::write(t.dir.join("f"), b"1").unwrap();
    let f = t.lookup(abi::ROOT_ID, "f").unwrap();
    let root = t.dir.canonicalize().unwrap();
    let path = |name: &str| root.join(name).as_os_str().as_bytes().to_vec();
    let code =
      |x: &[u8]| abi::read::<abi::OutHeader>(x).map(|h| (h.error, h.len as usize, h.unique));

    // Contents changed: the file's node, its pages too.
    let n = t.s.changed(&path("f"), 0x1000);
    assert_eq!(n.len(), 1);
    assert_eq!(code(&n[0]), Some((abi::NOTIFY_INVAL_INODE, 16 + 24, 0)));
    let x: abi::NotifyInvalInodeOut = abi::read(&n[0][16..]).unwrap();
    assert_eq!((x.ino, x.off), (f.nodeid, 0));

    // A name made in a directory the guest knows; one in a directory it doesn't, nothing.
    std::fs::write(t.dir.join("g"), b"").unwrap();
    let n = t.s.changed(&path("g"), 0x100);
    assert_eq!(code(&n[0]).unwrap().0, abi::NOTIFY_INVAL_ENTRY);
    let x: abi::NotifyInvalEntryOut = abi::read(&n[0][16..]).unwrap();
    assert_eq!((x.parent, x.namelen, &n[0][32..]), (abi::ROOT_ID, 1, &b"g\0"[..]));

    // Its contents changed, not looked up yet: only inotify hears; made and written, both.
    let n = t.s.changed(&path("g"), 0x1000);
    let x: abi::NotifyInvalEntryOut = abi::read(&n[0][16..]).unwrap();
    assert_eq!((n.len(), x.flags), (1, abi::INVAL_MODIFIED));
    assert_eq!(t.s.changed(&path("g"), 0x1100).len(), 2);
    t.lookup(abi::ROOT_ID, "g").unwrap();
    let n = t.s.changed(&path("g"), 0x1100);
    assert_eq!((n.len(), code(&n[0]).unwrap().0), (1, abi::NOTIFY_INVAL_INODE));
    assert_eq!(t.s.changed(&path("g"), 0x800).len(), 2);
    std::fs::create_dir(t.dir.join("d")).unwrap();
    std::fs::write(t.dir.join("d/h"), b"").unwrap();
    assert!(t.s.changed(&path("d/h"), 0x100).is_empty());

    // A name removed; events lost.
    std::fs::remove_file(t.dir.join("f")).unwrap();
    let n = t.s.changed(&path("f"), 0x200);
    assert_eq!(code(&n[0]).unwrap().0, abi::NOTIFY_DELETE);
    assert_eq!(code(&t.s.changed(&path("f"), 0x1)[0]).unwrap(), (abi::NOTIFY_INC_EPOCH, 16, 0));
  }

  #[test]
  fn opens_tell_what_changed_on_the_mac() {
    let mut t = T::new("opens");
    std::fs::write(t.dir.join("f"), b"1").unwrap();
    let f = t.lookup(abi::ROOT_ID, "f").unwrap().nodeid;
    let open = |t: &mut T| {
      let (_, b) = t.call(abi::OPEN, f, abi::bytes(&abi::OpenIn { flags: 0, open_flags: 0 }), 64);
      abi::read::<abi::OpenOut>(&b).unwrap().open_flags
    };

    // As looked up, the guest's caches are good; changed since, its attributes are stale until
    // it gets them again.
    assert_eq!(open(&mut t), abi::FOPEN_KEEP_CACHE);
    std::fs::write(t.dir.join("f"), b"22").unwrap();
    assert_eq!(open(&mut t), abi::FOPEN_INVAL_ATTR);
    t.call(abi::GETATTR, f, abi::bytes(&abi::GetattrIn::default()), 128);
    assert_eq!(open(&mut t), abi::FOPEN_KEEP_CACHE);
  }

  #[test]
  fn copy_file_range_copies_on_the_mac() {
    let mut t = T::new("copy");
    std::fs::write(t.dir.join("a"), b"0123456789").unwrap();
    std::fs::write(t.dir.join("b"), b"").unwrap();
    let mut fh = Vec::new();
    for name in ["a", "b"] {
      let e = t.lookup(abi::ROOT_ID, name).unwrap();
      let (_, b) =
        t.call(abi::OPEN, e.nodeid, abi::bytes(&abi::OpenIn { flags: 2, open_flags: 0 }), 64);
      fh.push((e.nodeid, abi::read::<abi::OpenOut>(&b).unwrap().fh));
    }

    let x = abi::CopyFileRangeIn {
      fh_in: fh[0].1,
      off_in: 2,
      nodeid_out: fh[1].0,
      fh_out: fh[1].1,
      off_out: 1,
      len: 100,
      flags: 0,
    };
    let (e, b) = t.call(abi::COPY_FILE_RANGE_64, fh[0].0, abi::bytes(&x), 64);
    assert_eq!((e, abi::read::<abi::CopyFileRangeOut>(&b).unwrap().bytes_copied), (0, 8));
    assert_eq!(std::fs::read(t.dir.join("b")).unwrap(), b"\x0023456789");
  }

  fn cpath(p: &Path) -> CString {
    CString::new(p.as_os_str().as_bytes()).unwrap()
  }

  fn xattr(p: &Path, name: &CStr) -> Option<Vec<u8>> {
    let mut b = vec![0u8; 64];
    let opts = libc::XATTR_NOFOLLOW;
    // SAFETY: getxattr writes at most the buffer's size; NUL-terminated strings.
    let n = unsafe {
      libc::getxattr(cpath(p).as_ptr(), name.as_ptr(), b.as_mut_ptr().cast(), b.len(), 0, opts)
    };
    b.truncate(usize::try_from(n).ok()?);
    Some(b)
  }

  fn set_xattr(p: &Path, name: &CStr, value: &[u8]) {
    let (v, len) = (value.as_ptr().cast(), value.len());
    // SAFETY: NUL-terminated strings and a value of its length.
    let r = unsafe { libc::setxattr(cpath(p).as_ptr(), name.as_ptr(), v, len, 0, 0) };
    assert_eq!(r, 0);
  }

  fn open_fh(t: &mut T, op: u32, node: u64, flags: u32) -> Result<u64, i32> {
    let (e, b) = t.call(op, node, abi::bytes(&abi::OpenIn { flags, open_flags: 0 }), 64);
    match e {
      0 => Ok(abi::read::<abi::OpenOut>(&b).unwrap().fh),
      e => Err(-e),
    }
  }

  /// A Mac folder beside the share.
  fn outside(t: &T) -> PathBuf {
    let d = t.dir.with_extension("out");
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
  }

  /// A directory and a symlink out of the share trade names over and over, as the guest's
  /// RENAME_EXCHANGE would: the extended attributes and access of a file in it are never
  /// those of the file outside.
  #[test]
  fn xattrs_and_access_stay_in_the_share() {
    use std::os::unix::fs::PermissionsExt;

    let mut t = T::new("xrace");
    let out = outside(&t);
    for d in [out.clone(), t.dir.join("d")] {
      std::fs::create_dir_all(&d).unwrap();
      std::fs::write(d.join("x"), b"").unwrap();
      set_xattr(&d.join("x"), c"user.k", b"k");
    }

    // Unreadable in the share: a read that works was answered outside.
    let mode = std::fs::Permissions::from_mode(0o200);
    std::fs::set_permissions(t.dir.join("d/x"), mode).unwrap();
    std::os::unix::fs::symlink(&out, t.dir.join("s")).unwrap();
    let d = t.lookup(abi::ROOT_ID, "d").unwrap().nodeid;
    let x = t.lookup(d, "x").unwrap().nodeid;

    let get = cat(&[abi::bytes(&abi::GetxattrIn { size: 64, padding: 0 }), &z("user.k")]);
    let sx = abi::SetxattrIn { size: 1, ..Default::default() };
    let set = cat(&[&abi::bytes(&sx)[..abi::COMPAT_SETXATTR_IN_SIZE], &z("user.set"), b"1"]);
    let readable = abi::AccessIn { mask: 4, padding: 0 }; // R_OK

    let (a, b) = (cpath(&t.dir.join("d")), cpath(&t.dir.join("s")));
    let stop = AtomicBool::new(false);
    let mut left = Vec::new();
    std::thread::scope(|s| {
      s.spawn(|| {
        while !stop.load(Ordering::Relaxed) {
          // SAFETY: NUL-terminated paths.
          unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_SWAP) };
        }
      });

      for _ in 0..2000 {
        if t.call(abi::GETXATTR, x, &get, 64) == (0, b"k".to_vec()) {
          left.push("GETXATTR");
        }

        t.call(abi::SETXATTR, x, &set, 0);
        if xattr(&out.join("x"), c"user.set").is_some() {
          left.push("SETXATTR");
        }

        t.call(abi::REMOVEXATTR, x, &z("user.k"), 0);
        if xattr(&out.join("x"), c"user.k").is_none() {
          left.push("REMOVEXATTR");
        }

        if t.call(abi::ACCESS, x, abi::bytes(&readable), 0).0 == 0 {
          left.push("ACCESS");
        }

        if !left.is_empty() {
          break;
        }
      }

      stop.store(true, Ordering::Relaxed);
    });

    let _ = std::fs::remove_dir_all(&out);
    assert!(left.is_empty(), "{left:?} reached the file outside the share");
  }

  /// Extended attributes by name in the directory: a symlink's own, each with the permissions
  /// it has by its path, a FIFO's without opening it.
  #[test]
  fn xattrs_keep_their_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let mut t = T::new("xperm");
    std::fs::write(t.dir.join("f"), b"").unwrap();
    std::os::unix::fs::symlink("f", t.dir.join("l")).unwrap();
    std::fs::create_dir(t.dir.join("x")).unwrap();
    std::fs::write(t.dir.join("x/f"), b"").unwrap();
    set_xattr(&t.dir.join("x/f"), c"user.k", b"x");
    std::fs::write(t.dir.join("u"), b"").unwrap();
    set_xattr(&t.dir.join("u"), c"user.k", b"u");
    let mk = abi::MknodIn { mode: 0o010644, ..Default::default() };
    assert_eq!(t.call(abi::MKNOD, abi::ROOT_ID, &cat(&[abi::bytes(&mk), &z("p")]), 256).0, 0);

    let [l, u, p, dir] = ["l", "u", "p", "x"].map(|x| t.lookup(abi::ROOT_ID, x).unwrap().nodeid);
    let x = t.lookup(dir, "f").unwrap().nodeid;
    let mode = |p: &Path, m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m));
    mode(&t.dir.join("u"), 0o000).unwrap();
    mode(&t.dir.join("x"), 0o111).unwrap();

    let sx = abi::SetxattrIn { size: 1, ..Default::default() };
    let set = cat(&[&abi::bytes(&sx)[..abi::COMPAT_SETXATTR_IN_SIZE], &z("user.k"), b"l"]);
    assert_eq!(t.call(abi::SETXATTR, l, &set, 0).0, 0);
    assert_eq!(xattr(&t.dir.join("l"), c"user.k"), Some(b"l".to_vec()), "the link's own");
    assert_eq!(xattr(&t.dir.join("f"), c"user.k"), None);

    let get = cat(&[abi::bytes(&abi::GetxattrIn { size: 64, padding: 0 }), &z("user.k")]);
    assert_eq!(t.call(abi::GETXATTR, l, &get, 64), (0, b"l".to_vec()));
    assert_eq!(t.call(abi::GETXATTR, x, &get, 64), (0, b"x".to_vec()), "a search-only dir");
    assert_eq!(t.call(abi::GETXATTR, u, &get, 64).0, -13, "EACCES, as by the path");
    let list = abi::GetxattrIn { size: 64, padding: 0 };
    assert_eq!(t.call(abi::LISTXATTR, p, abi::bytes(&list), 64), (0, Vec::new()));
    mode(&t.dir.join("x"), 0o755).unwrap();
  }

  /// A FIFO the guest made: OPEN, chmod and CREATE of its name answer at once, never waiting
  /// on the Mac for its other end.
  #[test]
  fn fifos_never_wait() {
    use std::os::unix::fs::OpenOptionsExt;

    let mut t = T::new("fifo");
    let mk = abi::MknodIn { mode: 0o010644, ..Default::default() };
    let (e, b) = t.call(abi::MKNOD, abi::ROOT_ID, &cat(&[abi::bytes(&mk), &z("p")]), 256);
    assert_eq!(e, 0);
    let p = abi::read::<abi::EntryOut>(&b).unwrap().nodeid;

    let fifo = t.dir.join("p");
    let (done, waited) = (AtomicBool::new(false), AtomicBool::new(false));
    let mut got = Vec::new();
    std::thread::scope(|s| {
      // A request still waiting after a second is let go: the Mac opens both ends.
      s.spawn(|| {
        let start = std::time::Instant::now();
        while !done.load(Ordering::Relaxed) {
          if start.elapsed() > Duration::from_secs(1) {
            waited.store(true, Ordering::Relaxed);
            let mut o = std::fs::OpenOptions::new();
            drop(o.read(true).write(true).custom_flags(libc::O_NONBLOCK).open(&fifo));
          }

          std::thread::sleep(Duration::from_millis(10));
        }
      });

      got.push(open_fh(&mut t, abi::OPEN, p, 0).map(|_| 0));
      let sa = abi::SetattrIn { valid: abi::FATTR_MODE, mode: 0o600, ..Default::default() };
      let (e, b) = t.call(abi::SETATTR, p, abi::bytes(&sa), 128);
      got.push(Ok(abi::read::<abi::AttrOut>(&b).map_or(e, |x| x.attr.mode as i32 & 0o7777)));
      let c = abi::CreateIn { flags: 0, mode: 0o100644, ..Default::default() };
      let (e, _) = t.call(abi::CREATE, abi::ROOT_ID, &cat(&[abi::bytes(&c), &z("p")]), 256);
      got.push(Err(-e));
      done.store(true, Ordering::Relaxed);
    });

    assert!(!waited.load(Ordering::Relaxed), "a request waited for the FIFO's other end");
    // ESTALE: the guest looks it up again and opens the FIFO in its own kernel.
    assert_eq!(got, [Err(ESTALE), Ok(0o600), Err(ESTALE)]);
  }

  #[test]
  fn handles_and_listings_are_bounded() {
    let mut t = T::new("bounds");
    t.s.max_handles = 2;
    for i in 0..1000 {
      std::fs::write(t.dir.join(format!("f{i:04}")), b"").unwrap();
    }

    let a = open_fh(&mut t, abi::OPENDIR, abi::ROOT_ID, 0).unwrap();
    let b = open_fh(&mut t, abi::OPENDIR, abi::ROOT_ID, 0).unwrap();
    assert_eq!(open_fh(&mut t, abi::OPENDIR, abi::ROOT_ID, 0), Err(ENFILE));
    let rl = abi::ReleaseIn { fh: b, ..Default::default() };
    t.call(abi::RELEASEDIR, abi::ROOT_ID, abi::bytes(&rl), 0);

    // Read in small pieces, as the guest does, from where the last one ended.
    let read = |t: &mut T, offset| {
      let rd = abi::ReadIn { fh: a, offset, size: 512, ..Default::default() };
      let (e, b) = t.call(abi::READDIR, abi::ROOT_ID, abi::bytes(&rd), 512);
      assert_eq!(e, 0);
      let (mut at, mut names) = (0, Vec::new());
      while at < b.len() {
        let d: abi::Dirent = abi::read(&b[at..]).unwrap();
        let from = at + size_of::<abi::Dirent>();
        names
          .push((d.off, String::from_utf8(b[from..from + d.namelen as usize].to_vec()).unwrap()));
        at += abi::dirent_size(d.namelen as usize);
      }

      let kept = t.s.handles.lock().unwrap()[&a].list.lock().unwrap().entries.len();
      assert!(kept < 500, "a handle keeps {kept} entries");
      names
    };

    let mut all = Vec::new();
    loop {
      let names = read(&mut t, all.last().map_or(0, |x: &(u64, String)| x.0));
      if names.is_empty() {
        break;
      }

      all.extend(names);
    }

    let mut names: Vec<_> = all.iter().map(|x| x.1.clone()).collect();
    names.sort();
    names.dedup();
    assert_eq!((all.len(), names.len()), (1002, 1002));
    // Back to an earlier offset: listed again, the same entries.
    assert_eq!(read(&mut t, 700)[0], all[700]);
    assert_eq!(open_fh(&mut t, abi::OPENDIR, abi::ROOT_ID, 0).map(|_| ()), Ok(()));
  }

  #[test]
  fn nodes_are_bounded() {
    let mut t = T::new("nodes");
    t.s.max_nodes = 3; // the root and two
    for name in ["a", "b", "c"] {
      std::fs::write(t.dir.join(name), b"").unwrap();
    }

    let a = t.lookup(abi::ROOT_ID, "a").unwrap().nodeid;
    t.lookup(abi::ROOT_ID, "b").unwrap();
    assert_eq!(t.lookup(abi::ROOT_ID, "c").unwrap_err(), ENOMEM);
    assert_eq!(t.lookup(abi::ROOT_ID, "a").unwrap().nodeid, a, "a known file still");
    t.call(abi::FORGET, a, abi::bytes(&abi::ForgetIn { nlookup: 2 }), 0);
    assert!(t.lookup(abi::ROOT_ID, "c").is_ok());
  }

  /// A copy of a file into itself past its end: to where the source ended, not chasing it.
  #[test]
  fn copy_file_range_stops_at_the_source_end() {
    let mut t = T::new("copy-self");
    std::fs::write(t.dir.join("a"), vec![7u8; 1 << 20]).unwrap();
    let a = t.lookup(abi::ROOT_ID, "a").unwrap().nodeid;
    let fh = open_fh(&mut t, abi::OPEN, a, 2).unwrap();
    let x = abi::CopyFileRangeIn {
      fh_in: fh,
      off_in: 0,
      nodeid_out: a,
      fh_out: fh,
      off_out: 1 << 20,
      len: 8 << 20,
      flags: 0,
    };
    let (e, b) = t.call(abi::COPY_FILE_RANGE_64, a, abi::bytes(&x), 64);
    assert_eq!((e, abi::read::<abi::CopyFileRangeOut>(&b).unwrap().bytes_copied), (0, 1 << 20));
    assert_eq!(std::fs::metadata(t.dir.join("a")).unwrap().len(), 2 << 20);
  }

  /// OPEN with O_CREAT and O_EXCL, which only CREATE takes: an open of the file there.
  #[test]
  fn open_makes_no_file() {
    let mut t = T::new("open-excl");
    std::fs::write(t.dir.join("f"), b"").unwrap();
    let f = t.lookup(abi::ROOT_ID, "f").unwrap().nodeid;
    assert!(open_fh(&mut t, abi::OPEN, f, 0o300).is_ok());
  }

  /// A chain with no room for the reply's header: the used length is what was written.
  #[test]
  fn replies_fit_the_chain() {
    let t = T::new("short");
    let h =
      abi::InHeader { len: 40, opcode: abi::STATFS, unique: 1, nodeid: 1, ..Default::default() };
    t.mem.write(BASE, abi::bytes(&h)).unwrap();
    for (room, want) in [(0, 0), (8, 8), (16 + 80, 16 + 80)] {
      let write = if room == 0 { vec![] } else { vec![(OUT, room)] };
      let chain = Chain { head: 0, read: vec![(BASE, 40)], write };
      assert_eq!(t.s.serve(&t.mem, &chain), want);
    }
  }

  /// A folder the user moves out of the share on the Mac: the guest's nodes in it are gone.
  #[test]
  fn nodes_moved_out_of_the_share_are_gone() {
    let mut t = T::new("moved");
    let out = outside(&t);
    std::fs::create_dir(t.dir.join("d")).unwrap();
    std::fs::write(t.dir.join("d/f"), b"").unwrap();
    let d = t.lookup(abi::ROOT_ID, "d").unwrap().nodeid;
    let f = t.lookup(d, "f").unwrap().nodeid;
    std::fs::rename(t.dir.join("d"), out.join("d")).unwrap();
    assert_eq!(open_fh(&mut t, abi::OPEN, f, 0), Err(ESTALE));

    // A lookup in it would still find it by the descriptor kept, until FSEvents tells.
    let path = t.dir.canonicalize().unwrap().join("d");
    t.s.changed(path.as_os_str().as_bytes(), 0x800 | 0x20000); // renamed, a directory
    let m = abi::MkdirIn { mode: 0o755, umask: 0 };
    assert_eq!(t.call(abi::MKDIR, d, &cat(&[abi::bytes(&m), &z("e")]), 256).0, -ESTALE);
    assert!(!out.join("d/e").exists());
    let _ = std::fs::remove_dir_all(&out);
  }

  /// An O_TRUNC open while the node's name is swapped with another file's: only the node's file
  /// is truncated.
  #[test]
  fn opens_truncate_only_their_file() {
    use std::os::unix::fs::MetadataExt;

    let mut t = T::new("trunc");
    std::fs::write(t.dir.join("a"), b"a").unwrap();
    std::fs::write(t.dir.join("b"), b"bbb").unwrap();
    let a = t.lookup(abi::ROOT_ID, "a").unwrap().nodeid;
    let b = std::fs::metadata(t.dir.join("b")).unwrap().ino();

    let stop = Arc::new(AtomicBool::new(false));
    let swap = {
      let (stop, dir) = (stop.clone(), t.dir.clone());
      std::thread::spawn(move || {
        let (x, y) = (CString::new("a").unwrap(), CString::new("b").unwrap());
        let d = std::fs::File::open(&dir).unwrap();
        while !stop.load(Ordering::Relaxed) {
          // SAFETY: NUL-terminated names in an open directory.
          unsafe { libc::renameatx_np(d.as_raw_fd(), x.as_ptr(), d.as_raw_fd(), y.as_ptr(), 2) };
        }
      })
    };

    for _ in 0..2000 {
      if let Ok(fh) = open_fh(&mut t, abi::OPEN, a, 0o1001) {
        let rl = abi::ReleaseIn { fh, ..Default::default() };
        t.call(abi::RELEASE, a, abi::bytes(&rl), 0);
      }
    }

    stop.store(true, Ordering::Relaxed);
    swap.join().unwrap();
    let b = ["a", "b"]
      .map(|x| t.dir.join(x))
      .into_iter()
      .find(|x| std::fs::metadata(x).unwrap().ino() == b);
    assert_eq!(std::fs::read(b.unwrap()).unwrap(), b"bbb");
  }

  /// A file removed while open, as a temporary file: its attributes still come from it.
  #[test]
  fn open_files_outlive_their_names() {
    let mut t = T::new("unlinked");
    std::fs::write(t.dir.join("f"), b"12345").unwrap();
    let f = t.lookup(abi::ROOT_ID, "f").unwrap().nodeid;
    let ro = open_fh(&mut t, abi::OPEN, f, 0).unwrap();
    let fh = open_fh(&mut t, abi::OPEN, f, 2).unwrap();
    assert_eq!(t.call(abi::UNLINK, abi::ROOT_ID, &z("f"), 0).0, 0);
    let (e, b) = t.call(abi::GETATTR, f, abi::bytes(&abi::GetattrIn::default()), 256);
    assert_eq!((e, abi::read::<abi::AttrOut>(&b).unwrap().attr.size), (0, 5));

    // fchmod sends no handle either.
    let sa = abi::SetattrIn { valid: abi::FATTR_MODE, mode: 0o640, ..Default::default() };
    let (e, b) = t.call(abi::SETATTR, f, abi::bytes(&sa), 256);
    assert_eq!((e, abi::read::<abi::AttrOut>(&b).unwrap().attr.mode & 0o7777), (0, 0o640));

    // Nor truncate on /proc/self/fd, done by the file open for writing.
    let sa = abi::SetattrIn { valid: abi::FATTR_SIZE, size: 2, ..Default::default() };
    let (e, b) = t.call(abi::SETATTR, f, abi::bytes(&sa), 256);
    assert_eq!((e, abi::read::<abi::AttrOut>(&b).unwrap().attr.size), (0, 2));

    for fh in [ro, fh] {
      let rl = abi::ReleaseIn { fh, ..Default::default() };
      t.call(abi::RELEASE, f, abi::bytes(&rl), 0);
    }

    let (e, _) = t.call(abi::GETATTR, f, abi::bytes(&abi::GetattrIn::default()), 256);
    assert_eq!(e, -ESTALE);
  }

  /// The owner changes a mode whatever it is, as the guest's kernel allows; a directory the user
  /// may only search is looked in.
  #[test]
  fn permissions_are_the_owners() {
    use std::os::unix::fs::PermissionsExt;

    let mut t = T::new("perms");
    std::fs::create_dir(t.dir.join("d")).unwrap();
    std::fs::write(t.dir.join("d/f"), b"hi").unwrap();
    let d = t.lookup(abi::ROOT_ID, "d").unwrap().nodeid;
    let f = t.lookup(d, "f").unwrap().nodeid;
    for (node, mode) in [(f, 0o000), (f, 0o200), (f, 0o600), (d, 0o000), (d, 0o111)] {
      let sa = abi::SetattrIn { valid: abi::FATTR_MODE, mode, ..Default::default() };
      let (e, b) = t.call(abi::SETATTR, node, abi::bytes(&sa), 256);
      assert_eq!((e, abi::read::<abi::AttrOut>(&b).unwrap().attr.mode & 0o7777), (0, mode));
    }

    t.s.dirs.lock().unwrap().retain(|&x, _| x == abi::ROOT_ID);
    let g = t.lookup(d, "f").unwrap().nodeid;
    let fh = open_fh(&mut t, abi::OPEN, g, 0).unwrap();
    let rd = abi::ReadIn { fh, offset: 0, size: 16, ..Default::default() };
    assert_eq!(t.call(abi::READ, g, abi::bytes(&rd), 16), (0, b"hi".to_vec()));
    assert_eq!(open_fh(&mut t, abi::OPENDIR, d, 0), Err(EACCES));
    std::fs::set_permissions(t.dir.join("d"), std::fs::Permissions::from_mode(0o755)).unwrap();
  }

  /// What the guest makes has the mode it asks, its umask applied: not the Mac's umask too.
  #[test]
  fn modes_are_the_guests() {
    use std::os::unix::fs::MetadataExt;

    let mut t = T::new("umask");
    let c = abi::CreateIn { flags: 0o102, mode: 0o100666, ..Default::default() }; // O_CREAT|O_RDWR
    assert_eq!(t.call(abi::CREATE, abi::ROOT_ID, &cat(&[abi::bytes(&c), &z("f")]), 256).0, 0);
    let m = abi::MkdirIn { mode: 0o777, umask: 0 };
    assert_eq!(t.call(abi::MKDIR, abi::ROOT_ID, &cat(&[abi::bytes(&m), &z("d")]), 256).0, 0);
    let n = abi::MknodIn { mode: 0o010666, ..Default::default() };
    assert_eq!(t.call(abi::MKNOD, abi::ROOT_ID, &cat(&[abi::bytes(&n), &z("p")]), 256).0, 0);
    for (x, mode) in [("f", 0o666), ("d", 0o777), ("p", 0o666)] {
      assert_eq!(std::fs::symlink_metadata(t.dir.join(x)).unwrap().mode() & 0o7777, mode, "{x}");
    }

    // A file already there keeps its mode.
    let c = abi::CreateIn { flags: 2, mode: 0o100600, ..Default::default() };
    assert_eq!(t.call(abi::CREATE, abi::ROOT_ID, &cat(&[abi::bytes(&c), &z("f")]), 256).0, 0);
    assert_eq!(std::fs::metadata(t.dir.join("f")).unwrap().mode() & 0o7777, 0o666);
  }

  /// A hard link made on the Mac is a new name; a directory made or removed is told as one.
  #[test]
  fn notifications_name_links_and_directories() {
    let mut t = T::new("names");
    std::fs::write(t.dir.join("f"), b"").unwrap();
    t.lookup(abi::ROOT_ID, "f").unwrap();
    let root = t.dir.canonicalize().unwrap();
    let path = |name: &str| root.join(name).as_os_str().as_bytes().to_vec();
    let entries = |n: &[Vec<u8>]| {
      let x = n
        .iter()
        .filter(|x| abi::read::<abi::OutHeader>(x).unwrap().error == abi::NOTIFY_INVAL_ENTRY);
      x.map(|x| abi::read::<abi::NotifyInvalEntryOut>(&x[16..]).unwrap().flags).collect::<Vec<_>>()
    };

    std::fs::hard_link(t.dir.join("f"), t.dir.join("g")).unwrap();
    assert_eq!(entries(&t.s.changed(&path("g"), 0x100)), [0]);
    std::fs::create_dir(t.dir.join("d")).unwrap();
    assert_eq!(entries(&t.s.changed(&path("d"), 0x100 | 0x20000)), [abi::INVAL_DIR]);
    std::fs::remove_dir(t.dir.join("d")).unwrap();
    let n = t.s.changed(&path("d"), 0x200 | 0x20000);
    assert_eq!(abi::read::<abi::NotifyDeleteOut>(&n[0][16..]).unwrap().padding, abi::INVAL_DIR);
  }

  /// A folder's search permission counts at each lookup, also after a chmod on the Mac.
  #[test]
  fn folders_ask_search_permission_each_time() {
    use std::os::unix::fs::PermissionsExt;

    let mut t = T::new("search");
    t.s.watched();
    let mode = |t: &T, x: &str, m| {
      std::fs::set_permissions(t.dir.join(x), std::fs::Permissions::from_mode(m)).unwrap()
    };
    for x in ["d", "e"] {
      std::fs::create_dir(t.dir.join(x)).unwrap();
      std::fs::write(t.dir.join(x).join("f"), b"").unwrap();
    }

    let d = t.lookup(abi::ROOT_ID, "d").unwrap().nodeid;
    t.lookup(d, "f").unwrap();
    mode(&t, "d", 0o600);
    assert_eq!(t.call(abi::UNLINK, d, &z("f"), 0).0, -EACCES);

    // Searched but not read: looked in, then not once the search is taken away.
    mode(&t, "e", 0o111);
    let e = t.lookup(abi::ROOT_ID, "e").unwrap().nodeid;
    assert!(t.lookup(e, "f").is_ok());
    mode(&t, "e", 0o600);
    assert_eq!(t.lookup(e, "f").unwrap_err(), EACCES);
    mode(&t, "d", 0o755);
    mode(&t, "e", 0o755);
  }

  /// SETATTR while the node's name is swapped with another file's: only the node's file changes.
  #[test]
  fn setattr_changes_only_its_file() {
    use std::os::unix::fs::MetadataExt;

    let mut t = T::new("setattr-race");
    std::fs::write(t.dir.join("a"), b"").unwrap();
    std::fs::write(t.dir.join("b"), b"").unwrap();
    let a = t.lookup(abi::ROOT_ID, "a").unwrap().nodeid;
    let b = std::fs::metadata(t.dir.join("b")).unwrap().ino();
    let before = std::fs::metadata(t.dir.join("b")).unwrap().mode() & 0o7777;

    let stop = Arc::new(AtomicBool::new(false));
    let swap = {
      let (stop, dir) = (stop.clone(), t.dir.clone());
      std::thread::spawn(move || {
        let (x, y) = (CString::new("a").unwrap(), CString::new("b").unwrap());
        let d = std::fs::File::open(&dir).unwrap();
        while !stop.load(Ordering::Relaxed) {
          // SAFETY: NUL-terminated names in an open directory.
          unsafe { libc::renameatx_np(d.as_raw_fd(), x.as_ptr(), d.as_raw_fd(), y.as_ptr(), 2) };
        }
      })
    };

    for i in 0..2000 {
      let mode = if i % 2 == 0 { 0o600 } else { 0o640 };
      let sa = abi::SetattrIn { valid: abi::FATTR_MODE, mode, ..Default::default() };
      t.call(abi::SETATTR, a, abi::bytes(&sa), 256);
    }

    stop.store(true, Ordering::Relaxed);
    swap.join().unwrap();
    let mode = ["a", "b"].into_iter().map(|x| std::fs::metadata(t.dir.join(x)).unwrap());
    let mode = mode.filter(|x| x.ino() == b).map(|x| x.mode() & 0o7777).next();
    assert_eq!(mode, Some(before));
  }

  /// A file made on the Mac and looked up before FSEvents tells of it is still news for its
  /// directory, once; one the guest made is not.
  #[test]
  fn made_names_are_news_once() {
    let mut t = T::new("news");
    let root = t.dir.canonicalize().unwrap();
    let path = |name: &str| root.join(name).as_os_str().as_bytes().to_vec();
    let entries = |n: &[Vec<u8>]| {
      let code = |x: &Vec<u8>| abi::read::<abi::OutHeader>(x).unwrap().error;
      n.iter().filter(|x| code(x) == abi::NOTIFY_INVAL_ENTRY).count()
    };

    std::fs::write(t.dir.join("h"), b"").unwrap();
    t.lookup(abi::ROOT_ID, "h").unwrap();
    assert_eq!(entries(&t.s.changed(&path("h"), 0x100)), 1);
    assert_eq!(entries(&t.s.changed(&path("h"), 0x1100)), 0);

    let c = abi::CreateIn { flags: 0o102, mode: 0o100644, ..Default::default() }; // O_CREAT|O_RDWR
    assert_eq!(t.call(abi::CREATE, abi::ROOT_ID, &cat(&[abi::bytes(&c), &z("g")]), 256).0, 0);
    assert_eq!(entries(&t.s.changed(&path("g"), 0x100)), 0);
    let m = abi::MkdirIn { mode: 0o755, umask: 0 };
    assert_eq!(t.call(abi::MKDIR, abi::ROOT_ID, &cat(&[abi::bytes(&m), &z("d")]), 256).0, 0);
    assert_eq!(entries(&t.s.changed(&path("d"), 0x100 | 0x20000)), 0);
    assert_eq!(t.call(abi::SYNCFS, abi::ROOT_ID, &[0; 8], 0).0, 0);
  }

  /// A directory the guest has open and the Mac moves out of the share is not listed after.
  #[test]
  fn open_folders_moved_out_are_not_listed() {
    let mut t = T::new("moved-open");
    let out = outside(&t);
    std::fs::create_dir(t.dir.join("d")).unwrap();
    let d = t.lookup(abi::ROOT_ID, "d").unwrap().nodeid;
    let fh = open_fh(&mut t, abi::OPENDIR, d, 0).unwrap();
    std::fs::rename(t.dir.join("d"), out.join("d")).unwrap();
    std::fs::write(out.join("d/new"), b"").unwrap();

    let rd = abi::ReadIn { fh, size: 4096, ..Default::default() };
    assert_eq!(t.call(abi::READDIRPLUS, d, abi::bytes(&rd), 4096).0, -ESTALE);
    let _ = std::fs::remove_dir_all(&out);
  }

  /// With no FSEvents to tell of moves, no descriptor is kept for a folder: one moved out of the
  /// share is not worked in.
  #[test]
  fn unwatched_folders_moved_out_are_gone() {
    let mut t = T::new("moved-unwatched");
    let out = outside(&t);
    std::fs::create_dir(t.dir.join("d")).unwrap();
    let d = t.lookup(abi::ROOT_ID, "d").unwrap().nodeid;
    let c = abi::CreateIn { flags: 0o102, mode: 0o100644, ..Default::default() }; // O_CREAT|O_RDWR
    assert_eq!(t.call(abi::CREATE, d, &cat(&[abi::bytes(&c), &z("a")]), 256).0, 0);
    std::fs::rename(t.dir.join("d"), out.join("d")).unwrap();
    assert_eq!(t.call(abi::CREATE, d, &cat(&[abi::bytes(&c), &z("b")]), 256).0, -ESTALE);
    assert!(!out.join("d/b").exists());
    let _ = std::fs::remove_dir_all(&out);
  }

  /// A file removed while open keeps its extended attributes for the guest.
  #[test]
  fn open_files_keep_their_xattrs() {
    let mut t = T::new("unlinked-xattr");
    std::fs::write(t.dir.join("f"), b"").unwrap();
    set_xattr(&t.dir.join("f"), c"user.k", b"value");
    let f = t.lookup(abi::ROOT_ID, "f").unwrap().nodeid;
    let fh = open_fh(&mut t, abi::OPEN, f, 2).unwrap();
    assert_eq!(t.call(abi::UNLINK, abi::ROOT_ID, &z("f"), 0).0, 0);

    let gx = abi::GetxattrIn { size: 64, padding: 0 };
    let args = cat(&[abi::bytes(&gx), &z("user.k")]);
    assert_eq!(t.call(abi::GETXATTR, f, &args, 64), (0, b"value".to_vec()));
    let rl = abi::ReleaseIn { fh, ..Default::default() };
    t.call(abi::RELEASE, f, abi::bytes(&rl), 0);
  }

  /// A WRITE longer than the guest's kernel sends writes no more than MAX_IO.
  #[test]
  fn writes_are_bounded() {
    let mut t = T::new("write-size");
    std::fs::write(t.dir.join("f"), b"").unwrap();
    let f = t.lookup(abi::ROOT_ID, "f").unwrap().nodeid;
    let fh = open_fh(&mut t, abi::OPEN, f, 2).unwrap();
    let x = abi::WriteIn { fh, size: MAX_IO + 1, ..Default::default() };
    let args = cat(&[abi::bytes(&x), &vec![b'X'; x.size as usize]]);
    let (e, b) = t.call(abi::WRITE, f, &args, 16);
    assert_eq!((e, abi::read::<abi::WriteOut>(&b).unwrap().size), (0, MAX_IO));
    assert_eq!(std::fs::metadata(t.dir.join("f")).unwrap().len(), MAX_IO as u64);
  }
}
