// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The guest's TCP ports on the Mac's localhost: what a container publishes on 8080 answers at
//! localhost:8080. The guest reports the ports it listens on; each gets a listener on 127.0.0.1
//! and ::1 whose connections go into the guest over vsock, not to its address on the shared
//! network: macOS lets a process reach that address only with the Local Network permission.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{io, thread};

/// Opens a stream to a TCP port in the guest.
pub type Dial = Arc<dyn Fn(u16) -> io::Result<UnixStream> + Send + Sync>;

pub struct Ports {
  dial: Dial,
  open: HashMap<u16, Fwd>,
}

impl Ports {
  pub fn new(dial: Dial) -> Ports {
    Ports { dial, open: HashMap::new() }
  }

  /// The ports the guest listens on now.
  pub fn update(&mut self, ports: &[u16]) {
    self.open.retain(|port, _| ports.contains(port));
    for &port in ports {
      if self.open.contains_key(&port) {
        continue;
      }

      match Fwd::open(port, &self.dial) {
        Ok(x) => {
          self.open.insert(port, x);
        }
        Err(e) => eprintln!("lodka: port {port}: {e}"),
      }
    }
  }
}

/// One port's listeners; dropping it closes them.
struct Fwd {
  stop: Arc<AtomicBool>,
  local: Vec<SocketAddr>,
}

impl Fwd {
  fn open(port: u16, dial: &Dial) -> io::Result<Fwd> {
    let stop = Arc::new(AtomicBool::new(false));
    let mut local = Vec::new();
    // IPv4 must work; IPv6 when the Mac has it.
    for (ip, need) in
      [(IpAddr::V4(Ipv4Addr::LOCALHOST), true), (IpAddr::V6(Ipv6Addr::LOCALHOST), false)]
    {
      let l = match TcpListener::bind(SocketAddr::new(ip, port)) {
        Ok(x) => x,
        Err(e) if need => return Err(e),
        Err(_) => continue,
      };

      local.push(l.local_addr()?);
      let (stop, dial) = (stop.clone(), dial.clone());
      let run = move || accept(l, port, &dial, &stop);
      thread::Builder::new().name(format!("port-{port}")).spawn(run)?;
    }

    Ok(Fwd { stop, local })
  }
}

impl Drop for Fwd {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Release);
    for x in &self.local {
      let _ = TcpStream::connect(x); // wakes `accept`, which sees `stop`
    }
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
}
