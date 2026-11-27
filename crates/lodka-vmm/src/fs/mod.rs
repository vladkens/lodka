// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The virtio file system device (virtio-fs): a Mac folder in the guest, mounted by its tag.
//! The guest kernel's FUSE requests are served from macOS files (`server`): FORGETs on queue 0,
//! notifications of what changed on the Mac (`watch`) on queue 1, then the request queues, one
//! per vCPU and each on a thread of its own, then the sync queues, one per vCPU too, each served
//! on its vCPU's thread by a hypercall (lodka's kernel patches, guest/patches).
//! https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html#x1-49600011

mod abi;
mod mac;
mod server;
mod watch;

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use server::Server;

use crate::mem::Mem;
use crate::virtio::{Call, Device, Irq, Queue, Wake, worker};

/// The spec's notification queue, at index 1.
const F_NOTIFICATION: u64 = 1 << 0;
/// lodka's sync queues, after the request queues.
const F_SYNC: u64 = 1 << 23;
/// A request queue's size: a 1 MiB request takes some 260 descriptors without indirect ones.
const QUEUE: u16 = 1024;
/// A sync queue holds one request of at most 128 KiB; the notification queue, the buffers the
/// driver gives it.
const SMALL: u16 = 128;
/// The largest notification is an entry's, with a 255-byte name.
const NOTE_SIZE: u32 = 512;
/// Notifications waiting for the guest's buffers; past this many, the guest drops its whole
/// cache of names instead.
const NOTES: usize = 64 * 1024;

pub struct Fs {
  tag: String,
  server: Arc<Server>,
  /// Request queues and sync queues, as many of each.
  queues: usize,
  features: u64,
  /// The FORGET queue's, then one per request queue.
  wakes: Vec<Arc<Wake>>,
  notes: Arc<Notes>,
  sync: Arc<OnceLock<Sync>>,
}

/// The sync queues, once the driver started the device.
struct Sync {
  mem: Arc<Mem>,
  first: u16,
  queues: Vec<Mutex<Queue>>,
}

/// Notifications for the guest, waiting for buffers on the notification queue.
struct Notes {
  pending: Mutex<VecDeque<Vec<u8>>>,
  wake: Arc<Wake>,
}

impl Notes {
  fn send(&self, notes: Vec<Vec<u8>>) {
    if notes.is_empty() {
      return;
    }

    let mut q = self.pending.lock().unwrap();
    if q.len() + notes.len() > NOTES {
      q.clear();
      q.push_back(server::inc_epoch());
    } else {
      q.extend(notes);
    }

    drop(q);
    self.wake.wake();
  }

  /// Fills the guest's buffers with what is waiting; true if it used any.
  fn flush(&self, q: &mut Queue, mem: &Mem) -> io::Result<bool> {
    let mut used = false;
    loop {
      let Some(note) = self.pending.lock().unwrap().pop_front() else { return Ok(used) };
      let Some(chain) = q.pop(mem)? else {
        // No buffer: the driver gives each back once handled, and notifies.
        self.pending.lock().unwrap().push_front(note);
        return Ok(used);
      };

      let n = chain.write_at(mem, 0, &note)?;
      q.push(mem, chain.head, n as u32)?;
      used = true;
    }
  }
}

impl Fs {
  /// Shares `root` under `tag` (at most 36 bytes), with `queues` request queues and as many
  /// sync queues: one each per vCPU.
  pub fn new(tag: &str, root: &Path, queues: usize) -> io::Result<Fs> {
    if tag.is_empty() || tag.len() > 36 {
      return Err(io::Error::other(format!("virtio-fs: bad tag {tag:?}")));
    }

    raise_nofile();
    let queues = queues.max(1);
    let server = Arc::new(Server::new(root)?);
    let wakes = (0..=queues).map(|_| Wake::new().map(Arc::new)).collect::<io::Result<_>>()?;
    let notes = Arc::new(Notes { pending: Mutex::default(), wake: Arc::new(Wake::new()?) });
    let (s, n) = (server.clone(), notes.clone());
    let changed = Box::new(move |path: &[u8], flags| n.send(s.changed(path, flags)));
    match watch::watch(server.root(), changed) {
      Ok(()) => server.watched(),
      Err(e) => eprintln!("lodka: {tag}: {e}; the guest's caches of it expire in a second"),
    }

    let (features, sync) = (F_NOTIFICATION | F_SYNC, Arc::default());
    Ok(Fs { tag: tag.into(), server, queues, features, wakes, notes, sync })
  }

  /// The first request queue: after the notification queue, if the driver took it.
  fn first_request(&self) -> usize {
    if self.features & F_NOTIFICATION != 0 { 2 } else { 1 }
  }
}

impl Device for Fs {
  fn kind(&self) -> u32 {
    26
  }

  fn features(&self) -> u64 {
    F_NOTIFICATION | F_SYNC
  }

  fn queues(&self) -> Vec<u16> {
    let mut x = vec![QUEUE, SMALL];
    x.extend(vec![QUEUE; self.queues]);
    x.extend(vec![SMALL; self.queues]);
    x
  }

  /// struct virtio_fs_config: the tag, NUL-padded; the request queues; the notification
  /// buffers' size; lodka's sync queues.
  fn config(&self) -> Vec<u8> {
    let mut c = vec![0u8; 48];
    c[..self.tag.len()].copy_from_slice(self.tag.as_bytes());
    c[36..40].copy_from_slice(&(self.queues as u32).to_le_bytes());
    c[40..44].copy_from_slice(&NOTE_SIZE.to_le_bytes());
    c[44..48].copy_from_slice(&(self.queues as u32).to_le_bytes());
    c
  }

  fn negotiated(&mut self, features: u64) {
    self.features = features;
  }

  fn activate(&mut self, mem: Arc<Mem>, queues: Vec<Queue>, irq: Irq) -> io::Result<()> {
    let first = self.first_request();
    let mut queues = queues.into_iter().enumerate();
    for (i, mut q) in queues.by_ref().take(first + self.queues) {
      let (mem, irq, server) = (mem.clone(), irq.clone(), self.server.clone());
      if i == 1 && first == 2 {
        let notes = self.notes.clone();
        worker(&format!("fs-{}-notes", self.tag), notes.wake.clone(), move || {
          if notes.flush(&mut q, &mem)? && q.wants_irq(&mem) {
            irq.used();
          }

          Ok(())
        })?;
        continue;
      }

      let wake = self.wakes[if i == 0 { 0 } else { i + 1 - first }].clone();
      worker(&format!("fs-{}-{i}", self.tag), wake, move || {
        if drain(&mut q, &mem, &server)? && q.wants_irq(&mem) {
          irq.used();
        }

        Ok(())
      })?;
    }

    if self.features & F_SYNC != 0 {
      // The guest has these served by the hypercall, never by a notify.
      let queues: Vec<_> = queues.take(self.queues).map(|(_, q)| q).collect();
      for q in &queues {
        q.set_notify(&mem, false)?;
      }

      let first = (first + self.queues) as u16;
      let queues = queues.into_iter().map(Mutex::new).collect();
      let _ = self.sync.set(Sync { mem, first, queues });
    }

    Ok(())
  }

  fn notify(&mut self, queue: u16) {
    let (q, first) = (queue as usize, self.first_request());
    match q {
      0 => self.wakes[0].wake(),
      1 if first == 2 => self.notes.wake.wake(),
      q if q < first + self.queues => self.wakes[q + 1 - first].wake(),
      _ => {
        serve_sync(&self.sync, &self.server, queue);
      }
    }
  }

  fn call(&self) -> Option<Call> {
    let (sync, server) = (self.sync.clone(), self.server.clone());
    Some(Arc::new(move |queue| serve_sync(&sync, &server, queue)))
  }
}

/// Serves the requests on a sync queue on the calling thread: the vCPU's, which then finds them
/// done, so no interrupt.
fn serve_sync(sync: &OnceLock<Sync>, server: &Server, queue: u16) -> bool {
  let Some(s) = sync.get() else { return false };
  let Some(q) = queue.checked_sub(s.first).and_then(|i| s.queues.get(i as usize)) else {
    return false;
  };

  let mut q = q.lock().unwrap();
  let mut serve = || -> io::Result<()> {
    while let Some(chain) = q.pop(&s.mem)? {
      let len = server.serve(&s.mem, &chain);
      q.push(&s.mem, chain.head, len)?;
    }

    Ok(())
  };
  serve().is_ok()
}

/// Serves the queue until it is empty; returns whether it used any buffers. A FORGET has no
/// reply, but its buffer still goes back: the driver frees it there, and waits for it at unmount.
fn drain(q: &mut Queue, mem: &Mem, server: &Server) -> io::Result<bool> {
  let mut used = false;
  loop {
    q.set_notify(mem, false)?;
    while let Some(chain) = q.pop(mem)? {
      let len = server.serve(mem, &chain);
      q.push(mem, chain.head, len)?;
      used = true;
    }

    q.set_notify(mem, true)?;
    if !q.pending(mem)? {
      return Ok(used);
    }
  }
}

/// Lets the process open as many files as macOS allows one: each file the guest has open is
/// one here, and the shell's default is 256.
fn raise_nofile() {
  let mut max: libc::c_int = 0;
  let mut len = size_of::<libc::c_int>();
  // SAFETY: sysctlbyname reads one int into `max`; getrlimit and setrlimit take a local rlimit.
  unsafe {
    if libc::sysctlbyname(
      c"kern.maxfilesperproc".as_ptr(),
      (&mut max as *mut libc::c_int).cast(),
      &mut len,
      std::ptr::null_mut(),
      0,
    ) != 0
    {
      return;
    }

    let mut rl: libc::rlimit = std::mem::zeroed();
    if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) != 0 {
      return;
    }

    let want = (max as libc::rlim_t).min(rl.rlim_max);
    if rl.rlim_cur < want {
      rl.rlim_cur = want;
      libc::setrlimit(libc::RLIMIT_NOFILE, &rl);
    }
  }
}
