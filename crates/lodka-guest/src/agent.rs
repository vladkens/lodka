// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The guest's side of the link to lodka on the Mac: the control channel (the clock, power off;
//! the guest's address and Docker's state back), and the connections lodka opens to Docker's
//! API, joined to dockerd's socket.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use lodka_proto::{CONTROL_PORT, Ctl, DOCKER_PORT};

use crate::{log, shutdown, sys};

const DOCKER_SOCK: &str = "/run/docker.sock";

/// The current control connection and what it was last told.
struct Link {
  out: Option<UnixStream>,
  addr: Option<String>,
  docker: bool,
}

impl Link {
  const fn new(out: Option<UnixStream>) -> Link {
    Link { out, addr: None, docker: false }
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

/// Tells lodka what changed: the address, Docker coming up.
fn tell() {
  let addr = sys::ipv4("eth0").map(|x| x.to_string());
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
