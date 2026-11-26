// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The virtio block device over a raw image file: the read-only root and the data disk. Reads
//! and writes go straight between the file and guest memory (`preadv`/`pwritev`), a queue per
//! vCPU, each served on a thread of its own; a discard punches a hole, so space Docker frees in
//! the guest goes back to the Mac.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Arc;

use crate::mem::Mem;
use crate::virtio::{Chain, Device, Irq, Queue, Wake, worker};

const F_SEG_MAX: u64 = 1 << 2;
const F_RO: u64 = 1 << 5;
const F_BLK_SIZE: u64 = 1 << 6;
const F_FLUSH: u64 = 1 << 9;
const F_MQ: u64 = 1 << 12;
const F_DISCARD: u64 = 1 << 13;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const T_DISCARD: u32 = 11;

const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

const SECTOR: u64 = 512;
const SEG_MAX: u32 = 126;
/// The host file system's block: holes are punched in whole blocks only.
const HOLE: u64 = 4096;

pub struct Blk {
  disk: Arc<Disk>,
  /// One per queue.
  wakes: Vec<Arc<Wake>>,
}

/// What every queue's thread serves.
struct Disk {
  file: File,
  ro: bool,
  /// Bytes, whole sectors: requests past it fail.
  size: u64,
  id: String,
}

impl Blk {
  pub fn open(path: &Path, ro: bool, id: &str, queues: usize) -> io::Result<Blk> {
    let file = OpenOptions::new().read(true).write(!ro).open(path)?;
    let size = file.metadata()?.len() / SECTOR * SECTOR;
    let disk = Arc::new(Disk { file, ro, size, id: id.to_string() });
    let wakes = (0..queues.max(1)).map(|_| Wake::new().map(Arc::new)).collect::<io::Result<_>>()?;
    Ok(Blk { disk, wakes })
  }
}

impl Device for Blk {
  fn kind(&self) -> u32 {
    2
  }

  fn features(&self) -> u64 {
    let rw = if self.disk.ro { F_RO } else { F_DISCARD };
    F_SEG_MAX | F_BLK_SIZE | F_FLUSH | F_MQ | rw
  }

  fn queues(&self) -> Vec<u16> {
    vec![256; self.wakes.len()]
  }

  /// struct virtio_blk_config, up to the discard fields.
  fn config(&self) -> Vec<u8> {
    let mut c = vec![0u8; 48];
    c[0..8].copy_from_slice(&(self.disk.size / SECTOR).to_le_bytes());
    c[12..16].copy_from_slice(&SEG_MAX.to_le_bytes());
    c[20..24].copy_from_slice(&(SECTOR as u32).to_le_bytes());
    c[34..36].copy_from_slice(&(self.wakes.len() as u16).to_le_bytes()); // num_queues
    c[36..40].copy_from_slice(&(1u32 << 22).to_le_bytes()); // max_discard_sectors: 2 GiB
    c[40..44].copy_from_slice(&1u32.to_le_bytes()); // max_discard_seg
    c[44..48].copy_from_slice(&((HOLE / SECTOR) as u32).to_le_bytes());
    c
  }

  fn activate(&mut self, mem: Arc<Mem>, queues: Vec<Queue>, irq: Irq) -> io::Result<()> {
    for (i, mut q) in queues.into_iter().enumerate() {
      let (mem, irq, disk) = (mem.clone(), irq.clone(), self.disk.clone());
      worker(&format!("blk-{}-{i}", disk.id), self.wakes[i].clone(), move || {
        let used = drain(&mut q, &mem, &disk)?;
        if used && q.wants_irq(&mem) {
          irq.used();
        }

        Ok(())
      })?;
    }

    Ok(())
  }

  fn notify(&mut self, queue: u16) {
    if let Some(x) = self.wakes.get(queue as usize) {
      x.wake();
    }
  }
}

/// Serves the queue until it is empty; returns whether it used any buffers.
fn drain(q: &mut Queue, mem: &Mem, disk: &Disk) -> io::Result<bool> {
  let mut used = false;
  loop {
    q.set_notify(mem, false)?;
    while let Some(chain) = q.pop(mem)? {
      let len = serve(mem, &chain, disk);
      q.push(mem, chain.head, len)?;
      used = true;
    }

    q.set_notify(mem, true)?;
    if !q.pending(mem)? {
      return Ok(used);
    }
  }
}

/// Serves one request; returns how many bytes it wrote into the chain (data and status).
fn serve(mem: &Mem, chain: &Chain, disk: &Disk) -> u32 {
  let wlen = chain.write_len();
  if wlen == 0 {
    return 0;
  }

  let mut hdr = [0u8; 16];
  let (status, data) = match chain.read_at(mem, 0, &mut hdr) {
    Ok(16) => {
      let kind = u32::from_le_bytes(hdr[..4].try_into().unwrap());
      let off = u64::from_le_bytes(hdr[8..].try_into().unwrap()).saturating_mul(SECTOR);
      match request(mem, chain, disk, kind, off) {
        Ok(x) => x,
        Err(e) => {
          eprintln!("lodka: blk-{}: {e}", disk.id);
          (S_IOERR, 0)
        }
      }
    }
    _ => (S_IOERR, 0),
  };

  let _ = chain.write_at(mem, wlen - 1, &[status]);
  (data + 1) as u32
}

fn request(mem: &Mem, chain: &Chain, disk: &Disk, kind: u32, off: u64) -> io::Result<(u8, usize)> {
  let fd = disk.file.as_raw_fd();
  let inside = |len: usize| off.checked_add(len as u64).is_some_and(|x| x <= disk.size);
  let rep = match kind {
    T_IN if !inside(chain.write_len() - 1) => (S_IOERR, 0),
    T_IN => {
      let len = chain.write_len() - 1;
      let iov = chain.iovecs(mem, true, 0, len)?;
      // SAFETY: the iovecs point into guest memory, checked by `iovecs`.
      let n = unsafe { libc::preadv(fd, iov.as_ptr(), iov.len() as i32, off as i64) };
      if n < 0 {
        return Err(io::Error::last_os_error());
      }

      if (n as usize) < len {
        return Err(io::Error::other(format!("read {n} of {len} bytes at {off}")));
      }

      (S_OK, len)
    }
    T_OUT if disk.ro || !inside(chain.read_len() - 16) => (S_IOERR, 0),
    T_OUT => {
      let len = chain.read_len() - 16;
      let iov = chain.iovecs(mem, false, 16, len)?;
      // SAFETY: as for T_IN.
      let n = unsafe { libc::pwritev(fd, iov.as_ptr(), iov.len() as i32, off as i64) };
      if n < 0 {
        return Err(io::Error::last_os_error());
      }

      if (n as usize) < len {
        return Err(io::Error::other(format!("wrote {n} of {len} bytes at {off}")));
      }

      (S_OK, 0)
    }
    T_FLUSH => {
      // A barrier, not F_FULLFSYNC (File::sync_data on macOS): earlier writes reach the disk
      // first, so the guest's journal holds across a power loss, at a quarter of the cost.
      // SAFETY: fcntl on an open file.
      if unsafe { libc::fcntl(fd, libc::F_BARRIERFSYNC) } != 0 {
        return Err(io::Error::last_os_error());
      }

      (S_OK, 0)
    }
    T_GET_ID => {
      let mut buf = [0u8; 20];
      let n = disk.id.len().min(20);
      buf[..n].copy_from_slice(&disk.id.as_bytes()[..n]);
      (S_OK, chain.write_at(mem, 0, &buf)?)
    }
    T_DISCARD if !disk.ro => {
      // struct virtio_blk_discard_write_zeroes: sector, num_sectors, flags
      let mut seg = [0u8; 16];
      chain.read_at(mem, 16, &mut seg)?;
      let start = u64::from_le_bytes(seg[..8].try_into().unwrap()).saturating_mul(SECTOR);
      let len = u32::from_le_bytes(seg[8..12].try_into().unwrap()) as u64 * SECTOR;
      if start.checked_add(len).is_none_or(|x| x > disk.size) {
        return Ok((S_IOERR, 0));
      }

      punch(&disk.file, start, len)?;
      (S_OK, 0)
    }
    _ => (S_UNSUPP, 0),
  };

  Ok(rep)
}

/// Frees the whole host blocks inside `start..start + len`.
fn punch(file: &File, start: u64, len: u64) -> io::Result<()> {
  let (from, to) = (start.next_multiple_of(HOLE), (start + len) / HOLE * HOLE);
  if to <= from {
    return Ok(());
  }

  #[repr(C)]
  struct Punchhole {
    flags: u32,
    reserved: u32,
    offset: i64,
    length: i64,
  }

  let arg = Punchhole { flags: 0, reserved: 0, offset: from as i64, length: (to - from) as i64 };
  // SAFETY: F_PUNCHHOLE reads one fpunchhole_t.
  match unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &arg) } {
    0 => Ok(()),
    _ => Err(io::Error::last_os_error()),
  }
}
