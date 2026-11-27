// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! Virtio over MMIO (version 2) with split virtqueues: the transport's registers, descriptor
//! chains, and the interrupt a device raises when it has used buffers.
//! https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering, fence};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::hv;
use crate::mem::{Mem, Mmio};

pub const F_VERSION_1: u64 = 1 << 32;

const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
const USED_F_NO_NOTIFY: u16 = 1;
const AVAIL_F_NO_INTERRUPT: u16 = 1;

// MARK: Queue

/// A split virtqueue, as the driver set it up through the transport.
#[derive(Debug, Clone, Default)]
pub struct Queue {
  pub max: u16,
  pub size: u16,
  pub ready: bool,
  pub desc: u64,
  pub avail: u64,
  pub used: u64,
  next_avail: u16,
  next_used: u16,
  /// Set when the driver resets the device: every copy of the queue stops using it, as the
  /// driver may free its rings and buffers.
  reset: Arc<AtomicBool>,
}

impl Queue {
  /// Whether the device may use the queue: the driver set it up and hasn't reset it since.
  fn live(&self) -> bool {
    self.ready && !self.reset.load(Ordering::Acquire)
  }

  /// Takes the next chain the driver made available.
  pub fn pop(&mut self, mem: &Mem) -> io::Result<Option<Chain>> {
    if !self.live() || self.size == 0 {
      return Ok(None);
    }

    let idx = mem.atomic_u16(self.avail + 2)?.load(Ordering::Acquire);
    if idx == self.next_avail {
      return Ok(None);
    }

    let slot = (self.next_avail % self.size) as u64;
    let head = mem.read_u16(self.avail + 4 + 2 * slot)?;
    self.next_avail = self.next_avail.wrapping_add(1);

    let mut chain = Chain { head, ..Default::default() };
    let mut at = head;
    for _ in 0..self.size {
      if at >= self.size {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "descriptor out of range"));
      }

      let d = self.desc + 16 * at as u64;
      let (addr, len) = (mem.read_u64(d)?, mem.read_u32(d + 8)?);
      let (flags, next) = (mem.read_u16(d + 12)?, mem.read_u16(d + 14)?);
      match flags & DESC_WRITE {
        0 if !chain.write.is_empty() => {
          return Err(io::Error::new(io::ErrorKind::InvalidData, "readable after writable"));
        }
        0 => chain.read.push((addr, len)),
        _ => chain.write.push((addr, len)),
      }

      if flags & DESC_NEXT == 0 {
        return Ok(Some(chain));
      }

      at = next;
    }

    Err(io::Error::new(io::ErrorKind::InvalidData, "descriptor loop"))
  }

  /// Whether the driver made a chain available that `pop` hasn't taken.
  pub fn pending(&self, mem: &Mem) -> io::Result<bool> {
    Ok(self.live() && mem.atomic_u16(self.avail + 2)?.load(Ordering::Acquire) != self.next_avail)
  }

  /// Tells the driver whether to notify when it makes chains available: not while the device
  /// works through the queue anyway, which saves a VM exit per request. Once notifications are
  /// back on, the device must check `pending`: a chain may have come in between.
  pub fn set_notify(&self, mem: &Mem, on: bool) -> io::Result<()> {
    if !self.live() {
      return Ok(());
    }

    let flags = if on { 0 } else { USED_F_NO_NOTIFY };
    mem.atomic_u16(self.used)?.store(flags, Ordering::Release);
    fence(Ordering::SeqCst);
    Ok(())
  }

  /// Whether the driver wants an interrupt for used chains; it doesn't while it polls the
  /// queue, and looks at the used ring itself before it asks for them again.
  pub fn wants_irq(&self, mem: &Mem) -> bool {
    if !self.live() {
      return false;
    }

    fence(Ordering::SeqCst);
    let flags = mem.atomic_u16(self.avail).map(|x| x.load(Ordering::Acquire));
    flags.map_or(true, |x| x & AVAIL_F_NO_INTERRUPT == 0)
  }

  /// Returns a chain to the driver with `len` bytes written into it.
  pub fn push(&mut self, mem: &Mem, head: u16, len: u32) -> io::Result<()> {
    if !self.live() {
      return Ok(());
    }

    let slot = (self.next_used % self.size) as u64;
    let mut elem = [0u8; 8];
    elem[..4].copy_from_slice(&(head as u32).to_le_bytes());
    elem[4..].copy_from_slice(&len.to_le_bytes());
    mem.write(self.used + 4 + 8 * slot, &elem)?;

    self.next_used = self.next_used.wrapping_add(1);
    fence(Ordering::Release);
    mem.atomic_u16(self.used + 2)?.store(self.next_used, Ordering::Release);
    Ok(())
  }
}

/// A request from the driver: the buffers the device reads, then those it writes.
#[derive(Debug, Default)]
pub struct Chain {
  pub head: u16,
  pub read: Vec<(u64, u32)>,
  pub write: Vec<(u64, u32)>,
}

impl Chain {
  pub fn read_len(&self) -> usize {
    self.read.iter().map(|x| x.1 as usize).sum()
  }

  pub fn write_len(&self) -> usize {
    self.write.iter().map(|x| x.1 as usize).sum()
  }

  /// Copies the readable buffers, from byte `at`, into `buf`; returns how many bytes it copied.
  pub fn read_at(&self, mem: &Mem, at: usize, buf: &mut [u8]) -> io::Result<usize> {
    let mut done = 0;
    for (ptr, len) in slices(mem, &self.read, at, buf.len())? {
      // SAFETY: `slices` checked the guest range; buf has room for done + len.
      unsafe { std::ptr::copy_nonoverlapping(ptr, buf.as_mut_ptr().add(done), len) };
      done += len;
    }

    Ok(done)
  }

  /// Copies `buf` into the writable buffers from byte `at`; returns how many bytes fit.
  pub fn write_at(&self, mem: &Mem, at: usize, buf: &[u8]) -> io::Result<usize> {
    let mut done = 0;
    for (ptr, len) in slices(mem, &self.write, at, buf.len())? {
      // SAFETY: as in `read_at`.
      unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr().add(done), ptr, len) };
      done += len;
    }

    Ok(done)
  }

  /// The readable (or writable) buffers from byte `at`, at most `len` bytes, as iovecs for
  /// `preadv`/`pwritev` straight from and into guest memory.
  pub fn iovecs(
    &self,
    mem: &Mem,
    write: bool,
    at: usize,
    len: usize,
  ) -> io::Result<Vec<libc::iovec>> {
    let segs = if write { &self.write } else { &self.read };
    let iov = slices(mem, segs, at, len)?
      .into_iter()
      .map(|(ptr, len)| libc::iovec { iov_base: ptr.cast(), iov_len: len });

    Ok(iov.collect())
  }
}

/// Host slices of `segs` from byte `skip`, `len` bytes at most.
fn slices(
  mem: &Mem,
  segs: &[(u64, u32)],
  mut skip: usize,
  mut len: usize,
) -> io::Result<Vec<(*mut u8, usize)>> {
  let mut out = Vec::new();
  for &(addr, size) in segs {
    if len == 0 {
      break;
    }

    let size = size as usize;
    if skip >= size {
      skip -= size;
      continue;
    }

    let n = (size - skip).min(len);
    out.push((mem.ptr(addr + skip as u64, n)?, n));
    skip = 0;
    len -= n;
  }

  Ok(out)
}

// MARK: Interrupts

/// What wakes vCPUs idling in WFI so they see a newly pending interrupt.
pub type Kick = Arc<dyn Fn() + Send + Sync>;

/// A device's interrupt: a level-triggered SPI, high while InterruptStatus has a bit set.
#[derive(Clone)]
pub struct Irq {
  pub intid: u32,
  status: Arc<Mutex<u32>>,
  kick: Kick,
}

impl Irq {
  pub fn new(intid: u32, kick: Kick) -> Irq {
    Irq { intid, status: Arc::new(Mutex::new(0)), kick }
  }

  /// The device put buffers in a used ring.
  pub fn used(&self) {
    let mut status = self.status.lock().unwrap();
    *status |= 1;
    hv::gic_set_spi(self.intid, true);
    drop(status);
    (self.kick)();
  }

  fn ack(&self, bits: u32) {
    let mut status = self.status.lock().unwrap();
    *status &= !bits;
    if *status == 0 {
      hv::gic_set_spi(self.intid, false);
    }
  }

  fn status(&self) -> u32 {
    *self.status.lock().unwrap()
  }
}

// MARK: Wake

/// A pipe that wakes a device's thread, which can poll it with other descriptors.
pub struct Wake {
  r: OwnedFd,
  w: OwnedFd,
}

impl Wake {
  pub fn new() -> io::Result<Wake> {
    let mut fds = [0; 2];
    // SAFETY: pipe writes two fds; both are made non-blocking and owned below.
    unsafe {
      if libc::pipe(fds.as_mut_ptr()) != 0 {
        return Err(io::Error::last_os_error());
      }

      for fd in fds {
        libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
      }

      use std::os::fd::FromRawFd;
      Ok(Wake { r: OwnedFd::from_raw_fd(fds[0]), w: OwnedFd::from_raw_fd(fds[1]) })
    }
  }

  pub fn wake(&self) {
    // SAFETY: a one-byte write to our pipe; a full pipe already means "awake".
    unsafe { libc::write(self.w.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
  }

  /// Empties the pipe, after a poll said it is readable.
  pub fn drain(&self) {
    let mut buf = [0u8; 64];
    // SAFETY: reads into a local buffer until the non-blocking pipe is empty.
    while unsafe { libc::read(self.r.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
  }

  pub fn fd(&self) -> RawFd {
    self.r.as_raw_fd()
  }

  /// Sleeps until woken, or `timeout` passes.
  pub fn wait(&self, timeout: Option<Duration>) {
    let ms = timeout.map_or(-1, |x| x.as_millis().min(i32::MAX as u128) as i32);
    let mut pfd = libc::pollfd { fd: self.fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one valid pollfd.
    unsafe { libc::poll(&mut pfd, 1, ms) };
    self.drain();
  }
}

/// Runs `serve` on a thread of its own each time `wake` fires, for as long as the process runs.
/// An error is logged and the thread waits for the next wake.
pub fn worker(
  name: &str,
  wake: Arc<Wake>,
  mut serve: impl FnMut() -> io::Result<()> + Send + 'static,
) -> io::Result<()> {
  let label = name.to_string();
  let run = move || {
    loop {
      wake.wait(None);
      if let Err(e) = serve() {
        eprintln!("lodka: {label}: {e}");
      }
    }
  };

  std::thread::Builder::new().name(name.into()).spawn(run)?;
  Ok(())
}

// MARK: Transport

/// A virtio device behind the MMIO transport.
pub trait Device: Send {
  /// The virtio device id: 1 net, 2 block, 3 console, 4 entropy, 19 vsock, 26 fs.
  fn kind(&self) -> u32;
  /// Feature bits besides VERSION_1, which the transport adds.
  fn features(&self) -> u64 {
    0
  }
  /// The largest size of each queue.
  fn queues(&self) -> Vec<u16>;
  fn config(&self) -> Vec<u8> {
    Vec::new()
  }
  /// The features the driver took, told before `activate`.
  fn negotiated(&mut self, _features: u64) {}
  /// The driver is ready: start serving the queues.
  fn activate(&mut self, mem: Arc<Mem>, queues: Vec<Queue>, irq: Irq) -> io::Result<()>;
  /// The driver made buffers available in `queue`. Runs on a vCPU thread, so it should only
  /// wake the device's own thread.
  fn notify(&mut self, queue: u16);
  /// Queues a vCPU has served on its own thread by a hypercall, outside the transport's lock.
  fn call(&self) -> Option<Call> {
    None
  }
}

/// Serves a device's queue now, on the vCPU that asks for it (`machine::HVC_QUEUE`): true if
/// the queue is one the device serves so.
pub type Call = Arc<dyn Fn(u16) -> bool + Send + Sync>;

pub struct Transport {
  dev: Box<dyn Device>,
  mem: Arc<Mem>,
  irq: Irq,
  features: u64,
  status: u32,
  dev_sel: u32,
  drv_sel: u32,
  drv_features: u64,
  sel: usize,
  queues: Vec<Queue>,
  /// Serving the driver's queues now.
  active: bool,
  /// Started once: a device's threads keep the queues it started with, so it starts only once.
  started: bool,
}

impl Transport {
  pub fn new(dev: Box<dyn Device>, mem: Arc<Mem>, irq: Irq) -> Transport {
    let features = dev.features() | F_VERSION_1;
    let queues = dev.queues().into_iter().map(|max| Queue { max, ..Default::default() }).collect();
    Transport {
      dev,
      mem,
      irq,
      features,
      status: 0,
      dev_sel: 0,
      drv_sel: 0,
      drv_features: 0,
      sel: 0,
      queues,
      active: false,
      started: false,
    }
  }

  fn queue(&self) -> Option<&Queue> {
    self.queues.get(self.sel)
  }

  fn set_status(&mut self, status: u32) {
    const DRIVER_OK: u32 = 4;
    const DEVICE_NEEDS_RESET: u32 = 0x40;

    if status == 0 {
      // A reset: the device stops using the queues, which are the driver's to set up again. The
      // guest resets a started device only as it powers off; a driver that binds to it again
      // finds it needs a reset.
      for q in &mut self.queues {
        q.reset.store(true, Ordering::Release);
        *q = Queue { max: q.max, ..Default::default() };
      }

      if self.irq.status() != 0 {
        self.irq.ack(u32::MAX);
      }

      self.active = false;
      self.status = 0;
      return;
    }

    self.status = status;
    if status & DRIVER_OK == 0 || self.active {
      return;
    }

    if self.started {
      self.status |= DEVICE_NEEDS_RESET;
      return;
    }

    (self.active, self.started) = (true, true);
    self.dev.negotiated(self.drv_features & self.features);
    let queues = self.queues.clone();
    if let Err(e) = self.dev.activate(self.mem.clone(), queues, self.irq.clone()) {
      eprintln!("lodka: virtio device {}: {e}", self.dev.kind());
      self.status |= DEVICE_NEEDS_RESET;
    }
  }
}

fn set_lo(x: &mut u64, v: u32) {
  *x = (*x & !0xffff_ffff) | v as u64;
}

fn set_hi(x: &mut u64, v: u32) {
  *x = (*x & 0xffff_ffff) | (v as u64) << 32;
}

/// Writes one of the selected queue's registers.
fn set_queue(q: &mut Queue, off: u64, val: u32) {
  match off {
    0x038 => q.size = val as u16,
    0x044 => q.ready = val == 1,
    0x080 => set_lo(&mut q.desc, val),
    0x084 => set_hi(&mut q.desc, val),
    0x090 => set_lo(&mut q.avail, val),
    0x094 => set_hi(&mut q.avail, val),
    0x0a0 => set_lo(&mut q.used, val),
    0x0a4 => set_hi(&mut q.used, val),
    _ => {}
  }
}

impl Mmio for Transport {
  fn read(&mut self, off: u64, data: &mut [u8]) {
    if off >= 0x100 {
      let cfg = self.dev.config();
      for (i, x) in data.iter_mut().enumerate() {
        *x = cfg.get(off as usize - 0x100 + i).copied().unwrap_or(0);
      }

      return;
    }

    let val: u32 = match off {
      0x000 => 0x7472_6976, // "virt"
      0x004 => 2,
      0x008 => self.dev.kind(),
      0x00c => 0x4c4f_444b, // "LODK"
      0x010 if self.dev_sel < 2 => (self.features >> (32 * self.dev_sel)) as u32,
      0x034 => self.queue().map_or(0, |q| q.max as u32),
      0x044 => self.queue().map_or(0, |q| q.ready as u32),
      0x060 => self.irq.status(),
      0x070 => self.status,
      0x0fc => 0, // ConfigGeneration
      _ => 0,
    };

    let bytes = val.to_le_bytes();
    let n = data.len().min(4);
    data[..n].copy_from_slice(&bytes[..n]);
  }

  fn write(&mut self, off: u64, data: &[u8]) {
    if off >= 0x100 {
      return; // no device has a writable config
    }

    let mut buf = [0u8; 4];
    let n = data.len().min(4);
    buf[..n].copy_from_slice(&data[..n]);
    let val = u32::from_le_bytes(buf);
    match off {
      0x014 => self.dev_sel = val,
      0x020 => match self.drv_sel {
        0 => set_lo(&mut self.drv_features, val),
        _ => set_hi(&mut self.drv_features, val),
      },
      0x024 => self.drv_sel = val,
      0x030 => self.sel = val as usize,
      0x050 if self.active => self.dev.notify(val as u16),
      0x064 => self.irq.ack(val),
      0x070 => self.set_status(val),
      0x038 | 0x044 | 0x080..=0x0a4 => {
        if let Some(q) = self.queues.get_mut(self.sel) {
          set_queue(q, off, val);
        }
      }
      _ => {}
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const BASE: u64 = 0x4000_0000;

  /// A queue of 4 at the start of RAM: descriptors, then the avail ring, then the used ring.
  fn setup() -> (Mem, Queue) {
    let mem = Mem::new(BASE, 0x10000).unwrap();
    let q = Queue {
      max: 4,
      size: 4,
      ready: true,
      desc: BASE,
      avail: BASE + 0x100,
      used: BASE + 0x200,
      ..Default::default()
    };
    (mem, q)
  }

  fn desc(mem: &Mem, i: u64, addr: u64, len: u32, flags: u16, next: u16) {
    let mut d = [0u8; 16];
    d[..8].copy_from_slice(&addr.to_le_bytes());
    d[8..12].copy_from_slice(&len.to_le_bytes());
    d[12..14].copy_from_slice(&flags.to_le_bytes());
    d[14..].copy_from_slice(&next.to_le_bytes());
    mem.write(BASE + 16 * i, &d).unwrap();
  }

  fn offer(mem: &Mem, q: &Queue, slot: u64, head: u16, idx: u16) {
    mem.write(q.avail + 4 + 2 * slot, &head.to_le_bytes()).unwrap();
    mem.write(q.avail + 2, &idx.to_le_bytes()).unwrap();
  }

  #[test]
  fn pop_and_push_a_chain() {
    let (mem, mut q) = setup();
    desc(&mem, 0, BASE + 0x1000, 6, DESC_NEXT, 1);
    desc(&mem, 1, BASE + 0x2000, 4, DESC_NEXT, 2);
    desc(&mem, 2, BASE + 0x3000, 8, DESC_WRITE, 0);
    mem.write(BASE + 0x1000, b"hello ").unwrap();
    mem.write(BASE + 0x2000, b"lodk").unwrap();
    offer(&mem, &q, 0, 0, 1);

    let chain = q.pop(&mem).unwrap().unwrap();
    assert_eq!((chain.head, chain.read_len(), chain.write_len()), (0, 10, 8));
    let mut buf = [0u8; 10];
    assert_eq!(chain.read_at(&mem, 0, &mut buf).unwrap(), 10);
    assert_eq!(&buf, b"hello lodk");
    let mut buf = [0u8; 3];
    assert_eq!(chain.read_at(&mem, 5, &mut buf).unwrap(), 3);
    assert_eq!(&buf, b" lo");
    assert_eq!(chain.write_at(&mem, 2, b"0123456789").unwrap(), 6);
    assert!(q.pop(&mem).unwrap().is_none());

    q.push(&mem, chain.head, 8).unwrap();
    assert_eq!(mem.read_u16(q.used + 2).unwrap(), 1);
    assert_eq!(mem.read_u32(q.used + 4).unwrap(), 0);
    assert_eq!(mem.read_u32(q.used + 8).unwrap(), 8);
  }

  /// Keeps the queues it is started with.
  struct Probe(Arc<Mutex<Vec<Queue>>>);

  impl Device for Probe {
    fn kind(&self) -> u32 {
      4
    }

    fn queues(&self) -> Vec<u16> {
      vec![4]
    }

    fn activate(&mut self, _mem: Arc<Mem>, queues: Vec<Queue>, _irq: Irq) -> io::Result<()> {
      self.0.lock().unwrap().extend(queues);
      Ok(())
    }

    fn notify(&mut self, _queue: u16) {}
  }

  #[test]
  fn reset_stops_a_started_device() {
    let (mem, q) = setup();
    let mem = Arc::new(mem);
    let given = Arc::new(Mutex::new(Vec::new()));
    let irq = Irq::new(32, Arc::new(|| {}));
    let mut t = Transport::new(Box::new(Probe(given.clone())), mem.clone(), irq);
    t.queues[0] = q.clone();
    t.set_status(0xf); // ACKNOWLEDGE, DRIVER, FEATURES_OK, DRIVER_OK
    assert_eq!(given.lock().unwrap().len(), 1);

    desc(&mem, 0, BASE + 0x1000, 4, 0, 0);
    offer(&mem, &q, 0, 0, 1);
    t.set_status(0);
    let mut started = given.lock().unwrap()[0].clone();
    assert!(started.pop(&mem).unwrap().is_none());
    assert!(!t.queues[0].ready);

    t.queues[0] = Queue { reset: Default::default(), ..q };
    t.set_status(0xf);
    assert_eq!(given.lock().unwrap().len(), 1);
    assert_ne!(t.status & 0x40, 0); // DEVICE_NEEDS_RESET
  }

  #[test]
  fn rejects_bad_chains() {
    let (mem, mut q) = setup();
    desc(&mem, 0, BASE + 0x1000, 4, DESC_WRITE | DESC_NEXT, 1);
    desc(&mem, 1, BASE + 0x2000, 4, 0, 0);
    offer(&mem, &q, 0, 0, 1);
    assert!(q.pop(&mem).is_err());

    desc(&mem, 2, BASE + 0x1000, 4, DESC_NEXT, 2);
    offer(&mem, &q, 1, 2, 2);
    assert!(q.pop(&mem).is_err());

    desc(&mem, 3, 0x10, 4, 0, 0);
    offer(&mem, &q, 2, 3, 3);
    let chain = q.pop(&mem).unwrap().unwrap();
    assert!(chain.read_at(&mem, 0, &mut [0u8; 4]).is_err());
  }
}
