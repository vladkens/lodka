// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The guest's side of the link to lodka on the Mac: the control channel (the clock, the SSH
//! key, power off; the guest's address, Docker's state and its listening ports back), and the
//! connections lodka opens: Docker's API, joined to dockerd's socket, and the forwarded ports.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpStream};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use lodka_proto::{CONTROL_PORT, Ctl, DOCKER_PORT, FORWARD_PORT};

use crate::{log, shutdown, sys};

const DOCKER_SOCK: &str = "/run/docker.sock";
const AUTHORIZED_KEYS: &str = "/run/lodka/authorized_keys";
/// The ports below aren't the containers': sshd's own.
const OWN_PORTS: &[u16] = &[22];
/// The ports are told again this often unchanged, so lodka tries again those it couldn't take.
const PORTS_AGAIN: Duration = Duration::from_secs(10);

/// The current control connection and what it was last told.
struct Link {
  out: Option<UnixStream>,
  addr: Option<String>,
  docker: bool,
  ports: Option<Vec<u16>>,
  ports_told: Option<Instant>,
}

impl Link {
  const fn new(out: Option<UnixStream>) -> Link {
    Link { out, addr: None, docker: false, ports: None, ports_told: None }
  }

  /// Sends a line to lodka; false when it isn't connected.
  fn send(&mut self, msg: &Ctl) -> bool {
    let Some(out) = self.out.as_mut() else { return false };
    match writeln!(out, "{msg}") {
      Ok(()) => true,
      Err(_) => {
        self.out = None;
        false
      }
    }
  }
}

static LINK: Mutex<Link> = Mutex::new(Link::new(None));

pub fn start() {
  spawn("control", control);
  spawn("report", report);
  spawn("docker", || listen(DOCKER_PORT, docker));
  spawn("forward", || listen(FORWARD_PORT, forward));
}

fn spawn(name: &str, f: fn()) {
  if let Err(e) = thread::Builder::new().name(name.into()).spawn(f) {
    log!("{name}: {e}");
  }
}

// MARK: Control

fn control() {
  loop {
    if let Err(e) = sys::vsock_connect(CONTROL_PORT).and_then(serve) {
      log!("control: {e}");
    }

    thread::sleep(Duration::from_millis(500));
  }
}

fn serve(stream: UnixStream) -> io::Result<()> {
  {
    let mut link = LINK.lock().unwrap();
    *link = Link::new(Some(stream.try_clone()?));
    link.send(&Ctl::Hello { version: env!("CARGO_PKG_VERSION").into() });
  }

  for line in BufReader::new(stream).lines() {
    let line = line?;
    match Ctl::parse(&line) {
      Some(Ctl::Time { secs, nanos }) => {
        if sys::set_time(secs, nanos)? {
          log!("clock set from the Mac's");
        }
      }
      Some(Ctl::Key(key)) => {
        std::fs::create_dir_all("/run/lodka")?;
        std::fs::write(AUTHORIZED_KEYS, format!("{key}\n"))?;
      }
      Some(Ctl::Poweroff) => shutdown(false),
      _ => log!("control: unexpected {line:?}"),
    }
  }

  LINK.lock().unwrap().out = None;
  Ok(())
}

fn report() {
  loop {
    tell();
    thread::sleep(Duration::from_secs(1));
  }
}

/// Tells lodka what changed: the address, Docker coming up, the ports.
fn tell() {
  let addr = sys::ipv4("eth0").map(|x| x.to_string());
  let ports: Vec<u16> =
    sys::listening_ports().into_iter().filter(|x| !OWN_PORTS.contains(x)).collect();
  let mut link = LINK.lock().unwrap();
  if link.out.is_none() {
    return;
  }

  if let Some(x) = &addr
    && addr != link.addr
    && link.send(&Ctl::Addr(x.clone()))
  {
    link.addr = addr;
  }

  if !link.docker && docker_up() && link.send(&Ctl::Docker) {
    link.docker = true;
  }

  let stale = link.ports_told.is_none_or(|x| x.elapsed() >= PORTS_AGAIN);
  if (stale || link.ports.as_ref() != Some(&ports)) && link.send(&Ctl::Ports(ports.clone())) {
    link.ports = Some(ports);
    link.ports_told = Some(Instant::now());
  }
}

fn docker_up() -> bool {
  let ping = || -> io::Result<bool> {
    let mut s = UnixStream::connect(DOCKER_SOCK)?;
    s.set_read_timeout(Some(Duration::from_secs(1)))?;
    s.write_all(b"GET /_ping HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n")?;
    let mut rep = Vec::new();
    s.read_to_end(&mut rep)?;
    Ok(rep.starts_with(b"HTTP/1.1 200"))
  };

  ping().unwrap_or(false)
}

// MARK: Connections

/// Serves each connection lodka opens on vsock `port` with `join`, on a thread of its own.
fn listen(port: u32, join: fn(UnixStream) -> io::Result<()>) {
  let l = match sys::VsockListener::bind(port) {
    Ok(x) => x,
    Err(e) => return log!("vsock {port}: {e}"),
  };

  loop {
    match l.accept() {
      Ok(x) => {
        thread::spawn(move || {
          if let Err(e) = join(x) {
            log!("vsock {port}: {e}");
          }
        });
      }
      Err(e) => {
        log!("vsock {port}: {e}");
        thread::sleep(Duration::from_millis(100));
      }
    }
  }
}

fn docker(x: UnixStream) -> io::Result<()> {
  splice(x, UnixStream::connect(DOCKER_SOCK)?);
  Ok(())
}

/// Joins a connection to the TCP port its first line names, on the loopback, where whatever
/// listens on every address answers too.
fn forward(mut x: UnixStream) -> io::Result<()> {
  let mut line = Vec::new();
  let mut b = [0u8; 1];
  // A byte at a time: what follows the line is the connection's own data.
  while line.len() < 6 {
    x.read_exact(&mut b)?;
    if b[0] == b'\n' {
      break;
    }

    line.push(b[0]);
  }

  let port: u16 = std::str::from_utf8(&line)
    .ok()
    .and_then(|x| x.parse().ok())
    .ok_or_else(|| io::Error::other(format!("forward: bad port {line:?}")))?;
  let to = [SocketAddr::from((Ipv4Addr::LOCALHOST, port)), (Ipv6Addr::LOCALHOST, port).into()];
  let tcp = TcpStream::connect(&to[..])?;
  tcp.set_nodelay(true)?;
  // As the vsock streams: a UnixStream only reads, writes and shuts down its descriptor.
  splice(x, UnixStream::from(OwnedFd::from(tcp)));
  Ok(())
}

/// Copies between two streams both ways until both are done.
fn splice(a: UnixStream, b: UnixStream) {
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
