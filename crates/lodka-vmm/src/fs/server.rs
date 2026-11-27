// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The FUSE server behind the virtio file system: the guest kernel's requests, answered from
//! the Mac folder it shares. A node is a file's `(dev, ino)` with the guest's lookup count,
//! reached again through its path from `fsgetpath` and checked to be the same file; only open
//! files and directories hold descriptors.

use std::collections::HashMap;
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

const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const EINVAL: i32 = 22;
const ENOSYS: i32 = 38;
const ENODATA: i32 = 61;
const ESTALE: i32 = 116;

/// How long the guest may trust a name, its absence and attributes without asking again: long
/// while FSEvents tells it what changes on the Mac, else a second.
const TTL: Duration = Duration::from_secs(3600);
const UNWATCHED_TTL: Duration = Duration::from_secs(1);
/// The largest READ or WRITE: 256 guest pages.
const MAX_IO: u32 = 1 << 20;
/// Directories whose descriptors are kept for lookups in them.
const DIRS: usize = 256;

pub struct Server {
  root: CString,
  nodes: Mutex<Nodes>,
  handles: Mutex<HashMap<u64, Arc<Handle>>>,
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
}

struct Nodes {
  ids: HashMap<u64, Node>,
  keys: HashMap<(i32, u64), u64>,
  next: u64,
}

struct Handle {
  fd: OwnedFd,
  /// A directory's entries, listed when the guest reads it from the start.
  list: Mutex<Option<Arc<Vec<Entry>>>>,
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
    let fd =
      open(&root, libc::O_RDONLY | libc::O_DIRECTORY).map_err(io::Error::from_raw_os_error)?;
    let st = mac::fstat(fd.as_raw_fd())?;
    let node = Node { dev: st.dev, ino: st.ino, kind: S_IFDIR, refs: 1, seen: None };
    let nodes = Nodes {
      ids: HashMap::from([(abi::ROOT_ID, node)]),
      keys: HashMap::from([((st.dev, st.ino), abi::ROOT_ID)]),
      next: abi::ROOT_ID + 1,
    };

    Ok(Server {
      root,
      nodes: Mutex::new(nodes),
      handles: Mutex::default(),
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
    if flags & LOST != 0 {
      return vec![inc_epoch()];
    }

    let path = path.strip_suffix(b"/").unwrap_or(path);
    let Some(cut) = path.iter().rposition(|&x| x == b'/') else { return Vec::new() };
    let (dir, name) = (&path[..cut.max(1)], &path[cut + 1..]);
    if name.is_empty() || path == self.root.as_bytes() {
      return Vec::new();
    }

    let mut out = Vec::new();
    let parent = lstat(dir).and_then(|st| self.known(&st));
    let Some(st) = lstat(path) else {
      if let Some(parent) = parent {
        let x = abi::NotifyDeleteOut { parent, child: 0, namelen: name.len() as u32, padding: 0 };
        out.push(note(abi::NOTIFY_DELETE, &[abi::bytes(&x), name, &[0]]));
      }

      return out;
    };

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
    // FSEvents keeps a file's Created flag on its later events: a name for a node the guest
    // has is not new, unless it was moved.
    if flags & MOVED != 0 || (flags & CREATED != 0 && known.is_none()) {
      entry(0);
    }

    // A known file's modification reaches its directory's watchers through its inode.
    if flags & DATA != 0 && known.is_none() && !st.is_dir() {
      entry(abi::INVAL_MODIFIED);
    }

    out
  }

  /// Serves one request; returns how many bytes of reply it wrote into the chain.
  pub fn serve(&self, mem: &Mem, chain: &Chain) -> u32 {
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
    let out = self.handle(&req);
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
    match chain.write_at(mem, 0, abi::bytes(&oh)) {
      Ok(_) => oh.len,
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

        self.entry_at(id, &dir, name)
      }
      abi::MKNOD => {
        let x: abi::MknodIn = arg(r.args, 0)?;
        let (name, _) = name(r.args, size_of::<abi::MknodIn>())?;
        let dir = self.dir(id)?;
        mknod(dir.as_raw_fd(), name, x.mode)?;
        self.entry_at(id, &dir, name)
      }
      abi::MKDIR => {
        let x: abi::MkdirIn = arg(r.args, 0)?;
        let (name, _) = name(r.args, size_of::<abi::MkdirIn>())?;
        let dir = self.dir(id)?;
        // SAFETY: a NUL-terminated name in an open directory.
        if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), (x.mode & 0o7777) as u16) } != 0 {
          return Err(os());
        }

        self.entry_at(id, &dir, name)
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

        self.entry_at(id, &dir, name)
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
        // SAFETY: fstatfs fills a zeroed statfs.
        let mut s: libc::statfs = unsafe { std::mem::zeroed() };
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
      abi::FLUSH | abi::SYNCFS => Ok(Out::Bytes(Vec::new())),
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
        let fd = open(&self.path(id)?, libc::O_RDONLY | libc::O_DIRECTORY)?;
        self.check(id, fd.as_raw_fd())?;
        let fh = self.add_handle(fd);
        reply(&abi::OpenOut { fh, open_flags: abi::FOPEN_CACHE_DIR, ..Default::default() })
      }
      abi::READDIR => self.readdir(r, false),
      abi::READDIRPLUS => self.readdir(r, true),
      abi::ACCESS => {
        let x: abi::AccessIn = arg(r.args, 0)?;
        let path = self.path(id)?;
        // SAFETY: a NUL-terminated path; the mask's bits are the same on both systems.
        if unsafe {
          libc::faccessat(libc::AT_FDCWD, path.as_ptr(), x.mask as i32, libc::AT_SYMLINK_NOFOLLOW)
        } != 0
        {
          return Err(os());
        }

        Ok(Out::Bytes(Vec::new()))
      }
      abi::GETXATTR => {
        let x: abi::GetxattrIn = arg(r.args, 0)?;
        let (name, _) = cstr(r.args, size_of::<abi::GetxattrIn>())?;
        if hidden(name) {
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
        if hidden(name) {
          return Err(EPERM);
        }

        let path = self.path(id)?;
        // SAFETY: NUL-terminated strings.
        if unsafe { libc::removexattr(path.as_ptr(), name.as_ptr(), libc::XATTR_NOFOLLOW) } != 0 {
          return Err(os());
        }

        Ok(Out::Bytes(Vec::new()))
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

  /// The guest looked `st` up once more: its node, made on the first lookup.
  fn add(&self, st: &Stat) -> u64 {
    let mut n = self.nodes.lock().unwrap();
    if let Some(&id) = n.keys.get(&(st.dev, st.ino))
      && let Some(x) = n.ids.get_mut(&id)
    {
      x.refs += 1;
      x.kind = st.kind();
      x.seen = Some((st.mtime, st.size));
      return id;
    }

    let id = n.next;
    n.next += 1;
    let seen = Some((st.mtime, st.size));
    let node = Node { dev: st.dev, ino: st.ino, kind: st.kind(), refs: 1, seen };
    n.ids.insert(id, node);
    n.keys.insert((st.dev, st.ino), id);
    id
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

  /// A path to the node's file now; ESTALE once it is gone.
  fn path(&self, id: u64) -> Result<CString, i32> {
    if id == abi::ROOT_ID {
      return Ok(self.root.clone());
    }

    let n = self.node(id)?;
    mac::path(n.dev, n.ino).map_err(|e| match err(e) {
      ENOENT => ESTALE,
      x => x,
    })
  }

  /// Whether `fd` is the node's file: a path can lead elsewhere once renamed.
  fn check(&self, id: u64, fd: RawFd) -> Result<Stat, i32> {
    let n = self.node(id)?;
    let st = mac::fstat(fd).map_err(err)?;
    match (st.dev, st.ino) == (n.dev, n.ino) {
      true => Ok(st),
      false => Err(ESTALE),
    }
  }

  /// The node's file opened, with `flags`.
  fn open_node(&self, id: u64, flags: i32) -> Result<OwnedFd, i32> {
    let fd = open(&self.path(id)?, flags)?;
    self.check(id, fd.as_raw_fd())?;
    Ok(fd)
  }

  /// A directory node's descriptor, kept for the next lookup in it.
  fn dir(&self, id: u64) -> Result<Arc<OwnedFd>, i32> {
    if let Some(x) = self.dirs.lock().unwrap().get(&id) {
      return Ok(x.clone());
    }

    let fd = Arc::new(self.open_node(id, libc::O_RDONLY | libc::O_DIRECTORY)?);
    let mut dirs = self.dirs.lock().unwrap();
    if dirs.len() >= DIRS {
      let root = dirs.remove(&abi::ROOT_ID);
      dirs.clear();
      dirs.extend(root.map(|x| (abi::ROOT_ID, x)));
    }

    dirs.insert(id, fd.clone());
    Ok(fd)
  }

  /// The directory a node's file is in, and its name there: for what works on a symlink itself.
  fn parent_of(&self, id: u64) -> Result<(OwnedFd, CString), i32> {
    let path = self.path(id)?;
    let bytes = path.as_bytes();
    let cut = bytes.iter().rposition(|&x| x == b'/').ok_or(EINVAL)?;
    let (parent, name) = (&bytes[..cut.max(1)], &bytes[cut + 1..]);
    let parent = CString::new(parent).map_err(|_| EINVAL)?;
    let name = CString::new(name).map_err(|_| EINVAL)?;
    let dir = open(&parent, libc::O_RDONLY | libc::O_DIRECTORY)?;
    let (_, st) = mac::stat_at(dir.as_raw_fd(), &name).map_err(err)?;
    let n = self.node(id)?;
    match (st.dev, st.ino) == (n.dev, n.ino) {
      true => Ok((dir, name)),
      false => Err(ESTALE),
    }
  }

  /// The node's attributes now.
  fn stat(&self, id: u64) -> Result<Stat, i32> {
    let n = self.node(id)?;
    if n.kind == S_IFDIR {
      return mac::fstat(self.dir(id)?.as_raw_fd()).map_err(err);
    }

    let path = self.path(id)?;
    // SAFETY: lstat fills a zeroed stat.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::lstat(path.as_ptr(), &mut st) } != 0 {
      return Err(os());
    }

    let st = Stat::from_stat(&st);
    match (st.dev, st.ino) == (n.dev, n.ino) {
      true => Ok(st),
      false => Err(ESTALE),
    }
  }

  fn lookup(&self, parent: u64, name: &CStr) -> Res {
    let dir = self.dir(parent)?;
    match self.entry_at(parent, &dir, name) {
      // No such name, cached as long as a name: FSEvents tells the guest when it appears.
      Err(ENOENT) if self.watched.load(Ordering::Relaxed) => reply(&abi::EntryOut {
        entry_valid: self.ttl().as_secs(),
        entry_valid_nsec: self.ttl().subsec_nanos(),
        ..Default::default()
      }),
      x => x,
    }
  }

  /// The entry for `name` in the directory node `parent`, open as `dir`; a lookup for the guest.
  fn entry_at(&self, parent: u64, dir: &OwnedFd, name: &CStr) -> Res {
    let (stored, st) = mac::stat_at(dir.as_raw_fd(), name).map_err(err)?;
    // Byte for byte: on a case- or normalization-insensitive volume another spelling finds the
    // file too, and the guest would see one inode under two names.
    if stored != name.to_bytes() {
      return Err(ENOENT);
    }

    let pdev = self.node(parent)?.dev;
    reply(&self.entry(&st, pdev))
  }

  fn entry(&self, st: &Stat, parent_dev: i32) -> abi::EntryOut {
    let nodeid = self.add(st);
    let mut attr = attr(st);
    if st.is_dir() && st.dev != parent_dev {
      attr.flags |= abi::ATTR_SUBMOUNT;
    }

    abi::EntryOut {
      nodeid,
      generation: 0,
      entry_valid: self.ttl().as_secs(),
      attr_valid: self.ttl().as_secs(),
      entry_valid_nsec: self.ttl().subsec_nanos(),
      attr_valid_nsec: self.ttl().subsec_nanos(),
      attr,
    }
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

  // MARK: Files

  fn add_handle(&self, fd: OwnedFd) -> u64 {
    let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
    let h = Arc::new(Handle { fd, list: Mutex::new(None) });
    self.handles.lock().unwrap().insert(fh, h);
    fh
  }

  fn handle_fd(&self, fh: u64) -> Result<Arc<Handle>, i32> {
    self.handles.lock().unwrap().get(&fh).cloned().ok_or(EINVAL)
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
    let fd = self.open_node(id, mac::open_flags(flags))?;
    if open_flags & abi::OPEN_KILL_SUIDGID != 0 {
      kill_suidgid(fd.as_raw_fd());
    }

    let open_flags = self.opened(id, fd.as_raw_fd(), flags)?;
    let fh = self.add_handle(fd);
    reply(&abi::OpenOut { fh, open_flags, ..Default::default() })
  }

  fn create(&self, r: &Req) -> Res {
    let x: abi::CreateIn = arg(r.args, 0)?;
    let (name, _) = name(r.args, size_of::<abi::CreateIn>())?;
    let dir = self.dir(r.h.nodeid)?;
    let flags = mac::open_flags(x.flags) | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: a NUL-terminated name in an open directory.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, x.mode & 0o7777) };
    if fd < 0 {
      return Err(os());
    }

    // SAFETY: a descriptor openat just returned.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let st = mac::fstat(fd.as_raw_fd()).map_err(err)?;
    let pdev = self.node(r.h.nodeid)?.dev;
    let entry = self.entry(&st, pdev);
    let open_flags = self.opened(entry.nodeid, fd.as_raw_fd(), x.flags)?;
    let fh = self.add_handle(fd);
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
    let len = (x.size as usize).min(r.chain.read_len().saturating_sub(at));
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
    let mut buf = vec![0u8; MAX_IO as usize];
    let mut done = 0;
    while done < x.len {
      let want = (x.len - done).min(buf.len() as u64) as usize;
      let at = (x.off_in + done) as i64;
      // SAFETY: pread writes at most `want` bytes into the buffer.
      let n = unsafe { libc::pread(from.fd.as_raw_fd(), buf.as_mut_ptr().cast(), want, at) };
      if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
        continue;
      }

      if n <= 0 {
        return if n == 0 || done > 0 { Ok(done) } else { Err(os()) };
      }

      let mut put = 0;
      while put < n as usize {
        let at = (x.off_out + done + put as u64) as i64;
        let rest = &buf[put..n as usize];
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
    if node.kind == S_IFLNK {
      // Only times reach a symlink itself; its mode has no meaning and its owner is fixed.
      if v & (abi::FATTR_ATIME | abi::FATTR_MTIME) != 0 {
        let (dir, name) = self.parent_of(id)?;
        let ts = times(&x);
        // SAFETY: two timespecs and a NUL-terminated name in an open directory.
        if unsafe {
          libc::utimensat(dir.as_raw_fd(), name.as_ptr(), ts.as_ptr(), libc::AT_SYMLINK_NOFOLLOW)
        } != 0
        {
          return Err(os());
        }
      }

      return self.attr_out(id, &self.stat(id)?);
    }

    let (handle, dir, file);
    let fd = if v & abi::FATTR_FH != 0 {
      handle = self.handle_fd(x.fh)?;
      handle.fd.as_raw_fd()
    } else if node.kind == S_IFDIR {
      dir = self.dir(id)?;
      dir.as_raw_fd()
    } else {
      let flags = if v & abi::FATTR_SIZE != 0 { libc::O_WRONLY } else { libc::O_RDONLY };
      file = self.open_node(id, flags)?;
      file.as_raw_fd()
    };

    // SAFETY (all four): calls on an open descriptor.
    if v & abi::FATTR_MODE != 0 && unsafe { libc::fchmod(fd, (x.mode & 0o7777) as u16) } != 0 {
      return Err(os());
    }

    if v & abi::FATTR_SIZE != 0 && unsafe { libc::ftruncate(fd, x.size as i64) } != 0 {
      return Err(os());
    }

    if v & abi::FATTR_KILL_SUIDGID != 0 {
      kill_suidgid(fd);
    }

    if v & (abi::FATTR_ATIME | abi::FATTR_MTIME) != 0
      && unsafe { libc::futimens(fd, times(&x).as_ptr()) } != 0
    {
      return Err(os());
    }

    // FATTR_UID and FATTR_GID: the Mac user can't give a file away; chown succeeds and changes
    // nothing, as with Virtualization.framework.
    self.attr_out(id, &mac::fstat(fd).map_err(err)?)
  }

  fn rename(&self, from: u64, to: u64, flags: u32, args: &[u8], at: usize) -> Res {
    let (old, at) = name(args, at)?;
    let (new, _) = name(args, at)?;
    // Linux's RENAME_NOREPLACE and RENAME_EXCHANGE; RENAME_WHITEOUT is overlayfs' own.
    let mut f = 0;
    for (l, m) in [(1, libc::RENAME_EXCL), (2, libc::RENAME_SWAP)] {
      if flags & l != 0 {
        f |= m;
      }
    }

    if flags & !3 != 0 {
      return Err(EINVAL);
    }

    let (a, b) = (self.dir(from)?, self.dir(to)?);
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
    let list = {
      let mut l = h.list.lock().unwrap();
      if x.offset == 0 || l.is_none() {
        *l = Some(Arc::new(mac::list(h.fd.as_raw_fd()).map_err(err)?));
      }

      l.clone().unwrap_or_default()
    };

    let me = mac::fstat(h.fd.as_raw_fd()).map_err(err)?;
    let size =
      (x.size as usize).min(r.chain.write_len().saturating_sub(size_of::<abi::OutHeader>()));
    let mut out = Vec::with_capacity(size);
    // `.` and `..` first, then the listing: an offset is the index of the next entry.
    for i in x.offset as usize..list.len() + 2 {
      let (name, st): (&[u8], &Stat) = match i {
        0 => (b".", &me),
        1 => (b"..", &me),
        _ => (&list[i - 2].0, &list[i - 2].1),
      };

      let need = if plus { abi::direntplus_size(name.len()) } else { abi::dirent_size(name.len()) };
      if out.len() + need > size {
        break;
      }

      if plus {
        // `.` and `..` get no entry: the kernel counts no lookup for them.
        let e = if i < 2 { abi::EntryOut::default() } else { self.entry(st, me.dev) };
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
    }

    Ok(Out::Bytes(out))
  }

  // MARK: Extended attributes

  /// One attribute's value (`name`), or the list of names; with `size` 0, only how long it is.
  fn xattr_get(&self, id: u64, name: Option<&CStr>, size: u32) -> Res {
    let path = self.path(id)?;
    let mut buf = vec![0u8; if size == 0 { 64 * 1024 } else { size as usize }];
    // SAFETY: the calls write at most the buffer's size; the strings are NUL-terminated.
    let n = unsafe {
      match name {
        Some(x) => libc::getxattr(
          path.as_ptr(),
          x.as_ptr(),
          buf.as_mut_ptr().cast(),
          buf.len(),
          0,
          libc::XATTR_NOFOLLOW,
        ),
        None => {
          libc::listxattr(path.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), libc::XATTR_NOFOLLOW)
        }
      }
    };
    if n < 0 {
      return Err(match os() {
        34 if size == 0 => 7, // ERANGE with no size given: longer than Linux allows, E2BIG
        x => x,
      });
    }

    buf.truncate(n as usize);
    if name.is_none() {
      let names: Vec<&[u8]> = buf.split(|&x| x == 0).filter(|x| !x.is_empty()).collect();
      let mut kept = Vec::new();
      for x in names.into_iter().filter(|x| !x.starts_with(b"com.apple.")) {
        kept.extend_from_slice(x);
        kept.push(0);
      }

      buf = kept;
    }

    if size == 0 {
      return reply(&abi::GetxattrOut { size: buf.len() as u32, padding: 0 });
    }

    if buf.len() > size as usize {
      return Err(34); // ERANGE
    }

    Ok(Out::Bytes(buf))
  }

  fn setxattr(&self, r: &Req) -> Res {
    // Without FUSE_SETXATTR_EXT the kernel sends the short form: size and flags.
    let x: abi::SetxattrIn = arg(&pad(r.args, size_of::<abi::SetxattrIn>()), 0)?;
    let (name, at) = cstr(r.args, abi::COMPAT_SETXATTR_IN_SIZE)?;
    let value = r.args.get(at..at + x.size as usize).ok_or(EINVAL)?;
    if hidden(name) {
      return Err(EPERM);
    }

    // Linux's XATTR_CREATE and XATTR_REPLACE are 1 and 2.
    let mut opts = libc::XATTR_NOFOLLOW;
    for (l, m) in [(1, libc::XATTR_CREATE), (2, libc::XATTR_REPLACE)] {
      if x.flags & l != 0 {
        opts |= m;
      }
    }

    let path = self.path(r.h.nodeid)?;
    // SAFETY: NUL-terminated strings and a value of its length.
    if unsafe {
      libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0, opts)
    } != 0
    {
      return Err(os());
    }

    Ok(Out::Bytes(Vec::new()))
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

fn lstat(path: &[u8]) -> Option<Stat> {
  let path = CString::new(path).ok()?;
  // SAFETY: lstat fills a zeroed stat from a NUL-terminated path.
  let mut st: libc::stat = unsafe { std::mem::zeroed() };
  (unsafe { libc::lstat(path.as_ptr(), &mut st) } == 0).then(|| Stat::from_stat(&st))
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

fn hidden(name: &CStr) -> bool {
  name.to_bytes().starts_with(b"com.apple.")
}

fn open(path: &CStr, flags: i32) -> Result<OwnedFd, i32> {
  // No symlink anywhere on the way: one planted in a directory between `fsgetpath` and here
  // would lead out of the share.
  let flags = flags | libc::O_NOFOLLOW_ANY | libc::O_CLOEXEC;
  // SAFETY: a NUL-terminated path.
  let fd = unsafe { libc::open(path.as_ptr(), flags) };
  if fd < 0 {
    return Err(match os() {
      40 => ESTALE, // ELOOP: a symlink where the file was
      x => x,
    });
  }

  // SAFETY: a descriptor open just returned.
  Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Makes `name` in `dir` as MKNOD asks. The Mac user may make no devices; a regular file, a
/// FIFO and a socket's file it may.
fn mknod(dir: RawFd, name: &CStr, mode: u32) -> Result<(), i32> {
  let perm = (mode & 0o7777) as u16;
  // SAFETY: (all) a NUL-terminated name in an open directory.
  let r = match mode & S_IFMT {
    S_IFREG => {
      let fd = unsafe {
        libc::openat(
          dir,
          name.as_ptr(),
          libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_CLOEXEC,
          perm as u32,
        )
      };
      if fd >= 0 {
        unsafe { libc::close(fd) };
      }

      fd.min(0)
    }
    0o010000 => unsafe { libc::mkfifoat(dir, name.as_ptr(), perm) },
    0o140000 => return bind_at(dir, name, perm),
    _ => return Err(EPERM),
  };

  if r != 0 {
    return Err(os());
  }

  Ok(())
}

unsafe extern "C" {
  /// libsystem_pthread, not in the SDK's headers: the calling thread's own working directory,
  /// the process's again for -1. macOS has no bindat().
  fn pthread_fchdir_np(fd: libc::c_int) -> libc::c_int;
}

/// A Unix socket's file, as bind() makes it: the guest's socket lives in the guest kernel, the
/// file only marks the name. Bound by the name alone from this thread's working directory, so a
/// long path doesn't matter and no symlink on the way is followed.
fn bind_at(dir: RawFd, name: &CStr, perm: u16) -> Result<(), i32> {
  let b = name.to_bytes();
  // SAFETY: sockaddr_un is integers and bytes, all zero a valid value.
  let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
  if b.len() >= addr.sun_path.len() {
    return Err(36); // ENAMETOOLONG
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
  let mut mode = st.mode & 0o7777;
  mode &= !0o4000;
  if mode & 0o010 != 0 {
    mode &= !0o2000;
  }

  if mode != st.mode & 0o7777 {
    // SAFETY: fchmod on an open file.
    unsafe { libc::fchmod(fd, mode as u16) };
  }
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

    let gx = abi::GetxattrIn { size: 100, padding: 0 };
    let (e, b) = t.call(abi::LISTXATTR, f.nodeid, abi::bytes(&gx), 100);
    assert_eq!((e, b.as_slice()), (0, &b"user.a\0"[..]));
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
}
