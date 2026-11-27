// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The memory balloon (virtio-balloon) for its free page reporting only: the guest reports
//! blocks of memory it freed, and the device gives them back to the Mac (`Mem::give_back`)
//! before it returns them. The balloon itself never inflates.
//! https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html#x1-3200005

use std::io;
use std::sync::Arc;

use crate::mem::Mem;
use crate::virtio::{Device, Irq, Queue, Wake, worker};

/// VIRTIO_BALLOON_F_REPORTING.
const F_REPORTING: u64 = 1 << 5;

pub struct Balloon {
  wake: Arc<Wake>,
}

impl Balloon {
  pub fn new() -> io::Result<Balloon> {
    Ok(Balloon { wake: Arc::new(Wake::new()?) })
  }
}

impl Device for Balloon {
  fn kind(&self) -> u32 {
    5
  }

  fn features(&self) -> u64 {
    F_REPORTING
  }

  /// Inflate, deflate and reporting: Linux's virtio-mmio numbers only the queues the features
  /// give, so the reporting queue is 2 here, not the spec's 4. The driver wants 32 at least.
  fn queues(&self) -> Vec<u16> {
    vec![64; 3]
  }

  /// struct virtio_balloon_config: the pages the guest should give up, 0, and how many it has.
  fn config(&self) -> Vec<u8> {
    vec![0; 8]
  }

  fn activate(&mut self, mem: Arc<Mem>, mut queues: Vec<Queue>, irq: Irq) -> io::Result<()> {
    worker("balloon", self.wake.clone(), move || {
      let mut used = false;
      for (i, q) in queues.iter_mut().enumerate() {
        while let Some(chain) = q.pop(&mem)? {
          // A report is the free blocks, one per buffer; nothing to do on the other queues,
          // which a balloon that never inflates gets nothing on. A block not given back stays
          // the guest's, and the report goes back all the same: the guest waits for it.
          if i == 2 {
            let given = chain
              .write
              .iter()
              .try_for_each(|&(gpa, len)| mem.give_back(gpa, len as u64).map(drop));
            if let Err(e) = given {
              eprintln!("lodka: balloon: {e}");
            }
          }

          q.push(&mem, chain.head, 0)?;
          used = true;
        }
      }

      if used {
        irq.used();
      }

      Ok(())
    })
  }

  fn notify(&mut self, _queue: u16) {
    self.wake.wake();
  }
}

#[cfg(test)]
mod tests {
  use std::sync::mpsc;
  use std::time::Duration;

  use super::*;
  use crate::mem::PIECE;

  const BASE: u64 = 0x4000_0000;

  #[test]
  fn a_report_gives_its_blocks_back() {
    let mem = Arc::new(Mem::new(BASE, 3 * PIECE).unwrap());
    mem.write(BASE + PIECE, &vec![0xaa; PIECE as usize]).unwrap();
    // Three queues of 64 in the first piece; only the third, reporting, gets a chain: one
    // device-writable buffer over the second piece.
    let queues: Vec<Queue> = (0..3)
      .map(|i| {
        let mut q = Queue::default();
        (q.max, q.size, q.ready) = (64, 64, true);
        q.desc = BASE + i * 0x1000;
        (q.avail, q.used) = (q.desc + 0x400, q.desc + 0x800);
        q
      })
      .collect();

    let q = &queues[2];
    let mut d = [0u8; 16];
    d[..8].copy_from_slice(&(BASE + PIECE).to_le_bytes());
    d[8..12].copy_from_slice(&(PIECE as u32).to_le_bytes());
    d[12..14].copy_from_slice(&2u16.to_le_bytes()); // VIRTQ_DESC_F_WRITE
    mem.write(q.desc, &d).unwrap();
    mem.write(q.avail + 2, &1u16.to_le_bytes()).unwrap();
    let used = q.used;

    let (tx, rx) = mpsc::channel();
    let irq = Irq::new(32, Arc::new(move || tx.send(()).unwrap()));
    let mut b = Balloon::new().unwrap();
    assert_eq!((b.kind(), b.features(), b.queues(), b.config().len()), (5, 1 << 5, vec![64; 3], 8));
    b.activate(mem.clone(), queues, irq).unwrap();
    b.notify(2);
    rx.recv_timeout(Duration::from_secs(5)).unwrap();

    assert_eq!(mem.read_u16(used + 2).unwrap(), 1);
    let mut buf = vec![0xffu8; PIECE as usize];
    mem.read(BASE + PIECE, &mut buf).unwrap();
    assert!(buf.iter().all(|&x| x == 0));
  }
}
