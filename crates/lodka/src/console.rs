// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! Small virtio devices: the console (hvc0, the guest's output into a log file) and the
//! entropy source (/dev/hwrng, seeding the guest's random numbers at boot).

use std::fs::File;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use crate::mem::Mem;
use crate::virtio::{Device, Irq, Queue, Wake, worker};

pub struct Console {
  out: Arc<Mutex<File>>,
  wake: Arc<Wake>,
}

impl Console {
  pub fn new(out: Arc<Mutex<File>>) -> io::Result<Console> {
    Ok(Console { out, wake: Arc::new(Wake::new()?) })
  }
}

impl Device for Console {
  fn kind(&self) -> u32 {
    3
  }

  fn queues(&self) -> Vec<u16> {
    vec![64, 64] // receiveq (unused: the guest gets no input), transmitq
  }

  fn activate(&mut self, mem: Arc<Mem>, queues: Vec<Queue>, irq: Irq) -> io::Result<()> {
    let mut tx = queues[1].clone();
    let out = self.out.clone();
    worker("console", self.wake.clone(), move || {
      let mut buf = Vec::new();
      while let Some(chain) = tx.pop(&mem)? {
        buf.resize(chain.read_len(), 0);
        chain.read_at(&mem, 0, &mut buf)?;
        out.lock().unwrap().write_all(&buf)?;
        tx.push(&mem, chain.head, 0)?;
        irq.used();
      }

      Ok(())
    })
  }

  fn notify(&mut self, queue: u16) {
    if queue == 1 {
      self.wake.wake();
    }
  }
}

pub struct Rng {
  wake: Arc<Wake>,
}

impl Rng {
  pub fn new() -> io::Result<Rng> {
    Ok(Rng { wake: Arc::new(Wake::new()?) })
  }
}

impl Device for Rng {
  fn kind(&self) -> u32 {
    4
  }

  fn queues(&self) -> Vec<u16> {
    vec![64]
  }

  fn activate(&mut self, mem: Arc<Mem>, queues: Vec<Queue>, irq: Irq) -> io::Result<()> {
    let mut q = queues[0].clone();
    worker("rng", self.wake.clone(), move || {
      while let Some(chain) = q.pop(&mem)? {
        let mut buf = vec![0u8; chain.write_len().min(4096)];
        // SAFETY: fills the buffer it is given.
        unsafe { libc::arc4random_buf(buf.as_mut_ptr().cast(), buf.len()) };
        let n = chain.write_at(&mem, 0, &buf)?;
        q.push(&mem, chain.head, n as u32)?;
        irq.used();
      }

      Ok(())
    })
  }

  fn notify(&mut self, _queue: u16) {
    self.wake.wake();
  }
}
