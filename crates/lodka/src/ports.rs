// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The guest's TCP ports on the Mac's localhost: what a container publishes on 8080 answers at
//! localhost:8080. The guest reports the ports it listens on; each gets a listener on 127.0.0.1
//! and ::1 whose connections go into the guest over vsock, not to its address on the shared
//! network: macOS lets a process reach that address only with the Local Network permission.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{io, thread};

/// Opens a stream to a TCP port in the guest.
pub type Dial = Arc<dyn Fn(u16) -> io::Result<UnixStream> + Send + Sync>;

/// How often a relay looks again at the Unix socket it reads: under load, macOS now and then
/// doesn't wake a blocked reader when the other end writes or shuts down.
const RECHECK: Duration = Duration::from_millis(250);

/// Where each port listens on the Mac.
const LOCAL: [IpAddr; 2] = [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)];

pub struct Ports {
  dial: Dial,
  open: HashMap<SocketAddr, Fwd>,
  /// Those that couldn't be listened on, logged once.
  failed: HashSet<SocketAddr>,
}

impl Ports {
  pub fn new(dial: Dial) -> Ports {
    Ports { dial, open: HashMap::new(), failed: HashSet::new() }
  }

  /// The ports the guest listens on now. The guest tells them again every few seconds, so a port
  /// taken on the Mac gets its listener once it is free.
  pub fn update(&mut self, ports: &[u16]) {
    self.open.retain(|x, _| ports.contains(&x.port()));
    self.failed.retain(|x| ports.contains(&x.port()));
    for addr in ports.iter().flat_map(|&port| LOCAL.map(|ip| SocketAddr::new(ip, port))) {
      if self.open.contains_key(&addr) {
        continue;
      }

      match Fwd::open(addr, &self.dial) {
        Ok(x) => {
          self.failed.remove(&addr);
          self.open.insert(addr, x);
        }
        Err(e) if e.kind() == io::ErrorKind::AddrNotAvailable => {} // a Mac without IPv6
        Err(e) if self.failed.insert(addr) => eprintln!("lodka: port {addr}: {e}"),
        Err(_) => {}
      }
    }
  }
}

/// One address's listener; dropping it closes it.
struct Fwd {
  stop: Arc<AtomicBool>,
  addr: SocketAddr,
}

impl Fwd {
  fn open(addr: SocketAddr, dial: &Dial) -> io::Result<Fwd> {
    let l = TcpListener::bind(addr)?;
    let stop = Arc::new(AtomicBool::new(false));
    let (port, dial, s) = (addr.port(), dial.clone(), stop.clone());
    let run = move || accept(l, port, &dial, &s);
    thread::Builder::new().name(format!("port-{port}")).spawn(run)?;
    Ok(Fwd { stop, addr })
  }
}

impl Drop for Fwd {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Release);
    let _ = TcpStream::connect(self.addr); // wakes `accept`, which sees `stop`
  }
}

fn accept(l: TcpListener, port: u16, dial: &Dial, stop: &AtomicBool) {
  for x in l.incoming() {
    if stop.load(Ordering::Acquire) {
      return;
    }

    let Ok(x) = x else { continue };
    let dial = dial.clone();
    thread::spawn(move || match dial(port) {
      Ok(y) => relay(x, y),
      Err(e) => eprintln!("lodka: port {port}: {e}"),
    });
  }
}

/// Copies between the two connections both ways until both are done.
fn relay(a: TcpStream, b: UnixStream) {
  let _ = a.set_nodelay(true);
  let _ = b.set_read_timeout(Some(RECHECK));
  let (Ok(a2), Ok(b2)) = (a.try_clone(), b.try_clone()) else { return };
  let one = thread::spawn(move || {
    let _ = copy(&mut &a, &mut &b2);
    let _ = b2.shutdown(Shutdown::Write);
  });

  let _ = copy(&mut &b, &mut &a2);
  let _ = a2.shutdown(Shutdown::Write);
  let _ = one.join();
}

/// io::copy with a buffer of 256 KiB, not 8: a fast stream moves in fewer calls.
fn copy(from: &mut impl Read, to: &mut impl Write) -> io::Result<()> {
  let mut buf = vec![0u8; 256 * 1024];
  loop {
    match from.read(&mut buf) {
      Ok(0) => return Ok(()),
      Ok(n) => to.write_all(&buf[..n])?,
      Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
      Err(e) if e.kind() == io::ErrorKind::WouldBlock => {} // RECHECK's timeout
      Err(e) => return Err(e),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn forwards_and_closes() {
    // The guest's side: echoes four bytes after a line with the port.
    let dial: Dial = Arc::new(|_| {
      let (a, mut b) = UnixStream::pair()?;
      thread::spawn(move || {
        let mut buf = [0u8; 4];
        b.read_exact(&mut buf).unwrap();
        b.write_all(&buf).unwrap();
      });

      Ok(a)
    });

    let free = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut ports = Ports::new(dial);
    ports.update(&[free]);
    let mut c = TcpStream::connect(("127.0.0.1", free)).unwrap();
    c.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    c.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");

    ports.update(&[]);
    thread::sleep(std::time::Duration::from_millis(50));
    assert!(TcpListener::bind(("127.0.0.1", free)).is_ok());
  }

  #[test]
  fn retries_a_taken_address() {
    let dial: Dial = Arc::new(|_| Ok(UnixStream::pair()?.0));
    let taken = TcpListener::bind("[::1]:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let mut ports = Ports::new(dial);
    ports.update(&[port]);
    assert_eq!(ports.open.len(), 1);

    drop(taken);
    ports.update(&[port]);
    assert_eq!(ports.open.len(), 2);
    assert!(ports.failed.is_empty());
  }
}
