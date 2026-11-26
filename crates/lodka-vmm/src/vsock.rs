// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The virtio socket device: stream connections between the guest (CID 3) and lodka (CID 2),
//! each joined to a Unix socket on the host. The guest dials lodka's listeners (the control
//! channel); lodka dials the guest's ports (Docker's API). One thread moves the data, with the
//! credit-based flow control the spec asks for.
//! https://docs.oasis-open.org/virtio/virtio/v1.2/cs01/virtio-v1.2-cs01.html#x1-4780006

use std::collections::{HashMap, VecDeque};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use crate::mem::Mem;
use crate::virtio::{Chain, Device, Irq, Queue, Wake};

pub const HOST_CID: u64 = 2;
pub const GUEST_CID: u64 = 3;

const HDR: usize = 44;
const STREAM: u16 = 1;
const OP_REQUEST: u16 = 1;
const OP_RESPONSE: u16 = 2;
const OP_RST: u16 = 3;
const OP_SHUTDOWN: u16 = 4;
const OP_RW: u16 = 5;
const OP_CREDIT_UPDATE: u16 = 6;
const OP_CREDIT_REQUEST: u16 = 7;
const SHUT_RCV: u32 = 1;
const SHUT_SEND: u32 = 2;
/// What each connection buffers of the guest's data before the host side reads it.
const BUF_ALLOC: u32 = 256 * 1024;
const MAX_PKT: usize = 64 * 1024;
/// The buffers of the host's Unix streams; macOS gives them 8 KiB, a wakeup each.
const SOCK_BUF: libc::c_int = 1 << 20;

/// A connected pair of Unix streams with room for a lot of data in flight.
pub fn pair() -> io::Result<(UnixStream, UnixStream)> {
  let (a, b) = UnixStream::pair()?;
  grow(&a)?;
  grow(&b)?;
  Ok((a, b))
}

fn grow(s: &UnixStream) -> io::Result<()> {
  let (size, len) = (SOCK_BUF, size_of::<libc::c_int>() as libc::socklen_t);
  for opt in [libc::SO_SNDBUF, libc::SO_RCVBUF] {
    // SAFETY: setsockopt reads one c_int.
    let rc = unsafe {
      libc::setsockopt(s.as_raw_fd(), libc::SOL_SOCKET, opt, (&raw const size).cast(), len)
    };
    if rc != 0 {
      return Err(io::Error::last_os_error());
    }
  }

  Ok(())
}

/// Makes the host end of a connection the guest opened to a port lodka listens on.
pub type Listener = Box<dyn FnMut() -> io::Result<UnixStream> + Send>;

// MARK: Header

#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Hdr {
  src_cid: u64,
  dst_cid: u64,
  src_port: u32,
  dst_port: u32,
  len: u32,
  kind: u16,
  op: u16,
  flags: u32,
  buf_alloc: u32,
  fwd_cnt: u32,
}

impl From<&[u8; HDR]> for Hdr {
  fn from(b: &[u8; HDR]) -> Hdr {
    let u16_at = |i: usize| u16::from_le_bytes(b[i..i + 2].try_into().unwrap());
    let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
    let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
    Hdr {
      src_cid: u64_at(0),
      dst_cid: u64_at(8),
      src_port: u32_at(16),
      dst_port: u32_at(20),
      len: u32_at(24),
      kind: u16_at(28),
      op: u16_at(30),
      flags: u32_at(32),
      buf_alloc: u32_at(36),
      fwd_cnt: u32_at(40),
    }
  }
}

impl From<&Hdr> for [u8; HDR] {
  fn from(h: &Hdr) -> [u8; HDR] {
    let mut b = [0u8; HDR];
    b[0..8].copy_from_slice(&h.src_cid.to_le_bytes());
    b[8..16].copy_from_slice(&h.dst_cid.to_le_bytes());
    b[16..20].copy_from_slice(&h.src_port.to_le_bytes());
    b[20..24].copy_from_slice(&h.dst_port.to_le_bytes());
    b[24..28].copy_from_slice(&h.len.to_le_bytes());
    b[28..30].copy_from_slice(&h.kind.to_le_bytes());
    b[30..32].copy_from_slice(&h.op.to_le_bytes());
    b[32..36].copy_from_slice(&h.flags.to_le_bytes());
    b[36..40].copy_from_slice(&h.buf_alloc.to_le_bytes());
    b[40..44].copy_from_slice(&h.fwd_cnt.to_le_bytes());
    b
  }
}

// MARK: Device

/// Opens connections from the host into the guest, from any thread.
#[derive(Clone)]
pub struct Connector {
  dials: Arc<Mutex<Vec<(u32, UnixStream)>>>,
  wake: Arc<Wake>,
}

impl Connector {
  /// Joins `stream` to the guest's `port`; if nothing listens there, the stream is closed.
  pub fn connect(&self, port: u32, stream: UnixStream) {
    self.dials.lock().unwrap().push((port, stream));
    self.wake.wake();
  }
}

pub struct Vsock {
  listeners: Option<HashMap<u32, Listener>>,
  dials: Arc<Mutex<Vec<(u32, UnixStream)>>>,
  wake: Arc<Wake>,
}

impl Vsock {
  pub fn new(listeners: HashMap<u32, Listener>) -> io::Result<Vsock> {
    let wake = Arc::new(Wake::new()?);
    Ok(Vsock { listeners: Some(listeners), dials: Default::default(), wake })
  }

  pub fn connector(&self) -> Connector {
    Connector { dials: self.dials.clone(), wake: self.wake.clone() }
  }
}

impl Device for Vsock {
  fn kind(&self) -> u32 {
    19
  }

  fn queues(&self) -> Vec<u16> {
    vec![256, 256, 16] // rx, tx, event
  }

  fn config(&self) -> Vec<u8> {
    GUEST_CID.to_le_bytes().to_vec()
  }

  fn activate(&mut self, mem: Arc<Mem>, queues: Vec<Queue>, irq: Irq) -> io::Result<()> {
    let listeners = self.listeners.take().ok_or_else(|| io::Error::other("vsock started twice"))?;
    let mut mux = Mux::new(mem, &queues, irq, listeners, self.dials.clone(), self.wake.clone());
    std::thread::Builder::new().name("vsock".into()).spawn(move || mux.run())?;
    Ok(())
  }

  fn notify(&mut self, _queue: u16) {
    self.wake.wake();
  }
}

// MARK: Connections

/// Keyed by (host port, guest port).
type Key = (u32, u32);

struct Conn {
  stream: UnixStream,
  connected: bool,
  peer_buf_alloc: u32,
  peer_fwd_cnt: u32,
  /// Bytes sent to the guest, and bytes of the guest's delivered to the stream (both wrap).
  tx_cnt: u32,
  fwd_cnt: u32,
  /// fwd_cnt as the guest last heard it.
  fwd_told: u32,
  /// The guest's data the stream hasn't taken yet.
  out: Vec<u8>,
  /// The stream ended: the guest was told it gets no more.
  eof: bool,
  /// The guest sends no more, or receives no more.
  peer_done_send: bool,
  peer_done_recv: bool,
}

impl Conn {
  fn new(stream: UnixStream, connected: bool) -> Conn {
    Conn {
      stream,
      connected,
      peer_buf_alloc: 0,
      peer_fwd_cnt: 0,
      tx_cnt: 0,
      fwd_cnt: 0,
      fwd_told: 0,
      out: Vec::new(),
      eof: false,
      peer_done_send: false,
      peer_done_recv: false,
    }
  }

  /// How many more bytes the guest can take.
  fn credit(&self) -> u32 {
    self.peer_buf_alloc.saturating_sub(self.tx_cnt.wrapping_sub(self.peer_fwd_cnt))
  }

  fn wants_read(&self) -> bool {
    self.connected && !self.eof && !self.peer_done_recv && self.credit() > 0
  }

  /// Writes what it can of `out` to the stream; false if the stream broke.
  fn flush(&mut self) -> bool {
    while !self.out.is_empty() {
      match self.stream.write(&self.out) {
        Ok(0) => return false,
        Ok(n) => {
          self.out.drain(..n);
          self.fwd_cnt = self.fwd_cnt.wrapping_add(n as u32);
        }
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
        Err(_) => return false,
      }
    }

    if self.peer_done_send {
      let _ = self.stream.shutdown(std::net::Shutdown::Write);
    }

    true
  }
}

struct Mux {
  mem: Arc<Mem>,
  rxq: Queue,
  txq: Queue,
  irq: Irq,
  listeners: HashMap<u32, Listener>,
  dials: Arc<Mutex<Vec<(u32, UnixStream)>>>,
  wake: Arc<Wake>,
  conns: HashMap<Key, Conn>,
  /// Packets without data waiting for a receive buffer.
  ctrl: VecDeque<Hdr>,
  /// A receive buffer taken from the guest and not filled yet.
  spare: Option<Chain>,
  next_port: u32,
  /// Chains given back on each queue since the last interrupt.
  rx_used: bool,
  tx_used: bool,
}

impl Mux {
  fn new(
    mem: Arc<Mem>,
    queues: &[Queue],
    irq: Irq,
    listeners: HashMap<u32, Listener>,
    dials: Arc<Mutex<Vec<(u32, UnixStream)>>>,
    wake: Arc<Wake>,
  ) -> Mux {
    Mux {
      mem,
      rxq: queues[0].clone(),
      txq: queues[1].clone(),
      irq,
      listeners,
      dials,
      wake,
      conns: HashMap::new(),
      ctrl: VecDeque::new(),
      spare: None,
      next_port: 1 << 20,
      rx_used: false,
      tx_used: false,
    }
  }

  fn run(&mut self) {
    loop {
      if let Err(e) = self.poll().and_then(|()| self.step()) {
        eprintln!("lodka: vsock: {e}");
      }

      let rx = std::mem::take(&mut self.rx_used) && self.rxq.wants_irq(&self.mem);
      let tx = std::mem::take(&mut self.tx_used) && self.txq.wants_irq(&self.mem);
      if rx || tx {
        self.irq.used();
      }
    }
  }

  /// Sleeps until the guest notifies, a host dials, or a stream can move data.
  fn poll(&mut self) -> io::Result<()> {
    let mut fds = vec![libc::pollfd { fd: self.wake.fd(), events: libc::POLLIN, revents: 0 }];
    let room = self.rx_has_room()?;
    for c in self.conns.values() {
      let mut events = 0;
      if room && c.wants_read() {
        events |= libc::POLLIN;
      }

      if !c.out.is_empty() {
        events |= libc::POLLOUT;
      }

      if events != 0 {
        fds.push(libc::pollfd { fd: c.stream.as_raw_fd(), events, revents: 0 });
      }
    }

    // SAFETY: valid pollfds.
    unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as u32, 1000) };
    self.wake.drain();
    Ok(())
  }

  fn rx_has_room(&mut self) -> io::Result<bool> {
    if self.spare.is_none() {
      self.spare = self.rxq.pop(&self.mem)?;
    }

    Ok(self.spare.is_some())
  }

  fn step(&mut self) -> io::Result<()> {
    let dials = std::mem::take(&mut *self.dials.lock().unwrap());
    for (port, stream) in dials {
      stream.set_nonblocking(true)?;
      grow(&stream)?;
      let local = self.next_port;
      self.next_port = self.next_port.wrapping_add(1).max(1 << 20);
      self.conns.insert((local, port), Conn::new(stream, false));
      self.queue((local, port), OP_REQUEST);
    }

    loop {
      self.txq.set_notify(&self.mem, false)?;
      while let Some(chain) = self.txq.pop(&self.mem)? {
        let res = self.on_packet(&chain);
        self.txq.push(&self.mem, chain.head, 0)?;
        self.tx_used = true;
        res?;
      }

      self.txq.set_notify(&self.mem, true)?;
      if !self.txq.pending(&self.mem)? {
        break;
      }
    }

    let keys: Vec<Key> = self.conns.keys().copied().collect();
    for key in keys {
      let Some(c) = self.conns.get_mut(&key) else { continue };
      if !c.flush() {
        self.reset(key);
        continue;
      }

      if c.fwd_cnt.wrapping_sub(c.fwd_told) >= BUF_ALLOC / 4 {
        self.queue(key, OP_CREDIT_UPDATE);
      }

      self.maybe_close(key);
    }

    self.fill_rx()
  }

  // MARK: Guest to host

  fn on_packet(&mut self, chain: &Chain) -> io::Result<()> {
    let mut buf = [0u8; HDR];
    if chain.read_at(&self.mem, 0, &mut buf)? < HDR {
      return Ok(());
    }

    let h = Hdr::from(&buf);
    let key = (h.dst_port, h.src_port);
    if h.dst_cid != HOST_CID || h.kind != STREAM {
      if h.op != OP_RST {
        self.queue(key, OP_RST);
      }

      return Ok(());
    }

    if let Some(c) = self.conns.get_mut(&key) {
      c.peer_buf_alloc = h.buf_alloc;
      c.peer_fwd_cnt = h.fwd_cnt;
    }

    match h.op {
      OP_REQUEST => {
        let stream = match self.listeners.get_mut(&h.dst_port) {
          Some(listen) if !self.conns.contains_key(&key) => listen(),
          _ => Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
        };

        match stream.and_then(|x| x.set_nonblocking(true).map(|_| x)) {
          Ok(stream) => {
            let mut c = Conn::new(stream, true);
            (c.peer_buf_alloc, c.peer_fwd_cnt) = (h.buf_alloc, h.fwd_cnt);
            self.conns.insert(key, c);
            self.queue(key, OP_RESPONSE);
          }
          Err(_) => self.queue(key, OP_RST),
        }
      }
      OP_RESPONSE => match self.conns.get_mut(&key) {
        Some(c) => c.connected = true,
        None => self.queue(key, OP_RST),
      },

      OP_RW => self.on_data(key, h.len as usize, chain)?,
      OP_CREDIT_UPDATE => {}
      OP_CREDIT_REQUEST => self.queue(key, OP_CREDIT_UPDATE),

      OP_SHUTDOWN => {
        if let Some(c) = self.conns.get_mut(&key) {
          c.peer_done_recv |= h.flags & SHUT_RCV != 0;
          c.peer_done_send |= h.flags & SHUT_SEND != 0;
          if !c.flush() {
            self.reset(key);
          }
        }
      }
      OP_RST => _ = self.conns.remove(&key),
      _ => self.queue(key, OP_RST),
    }

    Ok(())
  }

  /// Data for a connection: straight from guest memory to the stream; what the stream can't
  /// take yet waits in `out`.
  fn on_data(&mut self, key: Key, len: usize, chain: &Chain) -> io::Result<()> {
    let Some(c) = self.conns.get_mut(&key) else {
      self.queue(key, OP_RST);
      return Ok(());
    };

    let len = len.min(chain.read_len().saturating_sub(HDR));
    let mut done = 0;
    if c.out.is_empty() {
      let iov = chain.iovecs(&self.mem, false, HDR, len)?;
      // SAFETY: the iovecs point into guest memory checked by `iovecs`.
      let n = unsafe { libc::writev(c.stream.as_raw_fd(), iov.as_ptr(), iov.len() as i32) };
      match n {
        n if n >= 0 => done = n as usize,
        _ if io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock => {}
        _ => {
          self.reset(key);
          return Ok(());
        }
      }

      c.fwd_cnt = c.fwd_cnt.wrapping_add(done as u32);
    }

    if done < len {
      // Past the credit lodka gives: a guest that ignored it would grow `out` without bound.
      if c.out.len() + len - done > BUF_ALLOC as usize {
        self.reset(key);
        return Ok(());
      }

      let at = c.out.len();
      c.out.resize(at + len - done, 0);
      chain.read_at(&self.mem, HDR + done, &mut c.out[at..])?;
    }

    if !c.flush() {
      self.reset(key);
    }

    Ok(())
  }

  /// Ends a connection both sides are done with: everything delivered, nothing more to come.
  fn maybe_close(&mut self, key: Key) {
    let Some(c) = self.conns.get(&key) else { return };
    let guest_done = c.peer_done_send && c.out.is_empty();
    let host_done = c.eof || c.peer_done_recv;
    if guest_done && host_done {
      self.conns.remove(&key);
      self.queue(key, OP_RST);
    }
  }

  /// Queues a packet without data.
  fn queue(&mut self, key: Key, op: u16) {
    let h = self.hdr(key, op, 0);
    self.ctrl.push_back(h);
  }

  fn reset(&mut self, key: Key) {
    if self.conns.remove(&key).is_some() {
      self.queue(key, OP_RST);
    }
  }

  // MARK: Host to guest

  fn hdr(&mut self, key: Key, op: u16, len: u32) -> Hdr {
    let fwd_cnt = self.conns.get_mut(&key).map_or(0, |c| {
      c.fwd_told = c.fwd_cnt;
      c.fwd_cnt
    });

    let (src_port, dst_port) = key;
    Hdr {
      src_cid: HOST_CID,
      dst_cid: GUEST_CID,
      src_port,
      dst_port,
      len,
      kind: STREAM,
      op,
      flags: 0,
      buf_alloc: BUF_ALLOC,
      fwd_cnt,
    }
  }

  fn take_rx(&mut self) -> io::Result<Option<Chain>> {
    match self.spare.take() {
      Some(x) => Ok(Some(x)),
      None => self.rxq.pop(&self.mem),
    }
  }

  fn give(&mut self, chain: Chain, h: &Hdr) -> io::Result<()> {
    chain.write_at(&self.mem, 0, &<[u8; HDR]>::from(h))?;
    self.rxq.push(&self.mem, chain.head, HDR as u32 + h.len)?;
    self.rx_used = true;
    Ok(())
  }

  fn fill_rx(&mut self) -> io::Result<()> {
    // Data waits for the control packets made before it, a connection's response first.
    self.send_ctrl()?;
    if !self.ctrl.is_empty() {
      return Ok(());
    }

    let keys: Vec<Key> = self.conns.iter().filter(|x| x.1.wants_read()).map(|x| *x.0).collect();
    for key in keys {
      while let Some(c) = self.conns.get(&key).filter(|x| x.wants_read()) {
        let (credit, fd) = (c.credit() as usize, c.stream.as_raw_fd());
        let Some(chain) = self.take_rx()? else { return Ok(()) };
        let room = chain.write_len().saturating_sub(HDR).min(MAX_PKT);
        let iov = chain.iovecs(&self.mem, true, HDR, room.min(credit))?;
        // SAFETY: the iovecs point into guest memory checked by `iovecs`.
        let n = unsafe { libc::readv(fd, iov.as_ptr(), iov.len() as i32) };

        match n {
          0 => {
            self.spare = Some(chain);
            let c = self.conns.get_mut(&key).unwrap();
            c.eof = true;
            let h = Hdr { flags: SHUT_SEND, ..self.hdr(key, OP_SHUTDOWN, 0) };
            self.ctrl.push_back(h);
            self.maybe_close(key);
            break;
          }

          n if n > 0 => {
            let c = self.conns.get_mut(&key).unwrap();
            c.tx_cnt = c.tx_cnt.wrapping_add(n as u32);
            let h = self.hdr(key, OP_RW, n as u32);
            self.give(chain, &h)?;
          }
          _ => {
            self.spare = Some(chain);
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::WouldBlock && e.kind() != io::ErrorKind::Interrupted {
              self.reset(key);
            }

            break;
          }
        }
      }
    }

    // Control packets made above (shutdowns, resets) go out in this round too.
    self.send_ctrl()
  }

  /// Sends the packets without data, as long as the guest has receive buffers.
  fn send_ctrl(&mut self) -> io::Result<()> {
    while let Some(h) = self.ctrl.pop_front() {
      let Some(chain) = self.take_rx()? else {
        self.ctrl.push_front(h);
        break;
      };

      self.give(chain, &h)?;
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn header_round_trip() {
    let h = Hdr {
      src_cid: 3,
      dst_cid: 2,
      src_port: 50000,
      dst_port: 1024,
      len: 7,
      kind: STREAM,
      op: OP_RW,
      flags: 0,
      buf_alloc: 1 << 18,
      fwd_cnt: 42,
    };
    assert_eq!(Hdr::from(&<[u8; HDR]>::from(&h)), h);
  }

  #[test]
  fn credit_wraps() {
    let (a, _b) = UnixStream::pair().unwrap();
    let mut c = Conn::new(a, true);
    c.peer_buf_alloc = 1000;
    c.tx_cnt = 5u32.wrapping_sub(10);
    c.peer_fwd_cnt = 5u32.wrapping_sub(400);
    assert_eq!(c.credit(), 610);
    c.peer_fwd_cnt = c.tx_cnt;
    assert_eq!(c.credit(), 1000);
  }
}
