// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The virtio network card on a vmnet shared network: macOS does the NAT, DHCP and DNS, and the
//! Mac reaches the guest's address directly. Since macOS 26 a process signed with
//! com.apple.security.virtualization may start one without root. Packets move between vmnet
//! and guest memory with no copy in between.

use std::ffi::{CStr, c_char, c_void};
use std::io;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use block2::RcBlock;

use crate::mem::Mem;
use crate::virtio::{Chain, Device, Irq, Queue, Wake, worker};

const F_MAC: u64 = 1 << 5;
const F_STATUS: u64 = 1 << 16;
/// struct virtio_net_hdr_v1, all zero: no offloads, one buffer per packet.
const HDR: usize = 12;
const BATCH: usize = 32;

// MARK: vmnet

const VMNET_SHARED_MODE: u32 = 1001;
const VMNET_SUCCESS: u32 = 1000;
const PACKETS_AVAILABLE: u32 = 1;

#[repr(C)]
struct PktDesc {
  size: usize,
  iov: *mut libc::iovec,
  iovcnt: u32,
  flags: u32,
}

type Handle = *mut c_void;

#[link(name = "vmnet", kind = "framework")]
unsafe extern "C" {
  fn vmnet_network_configuration_create(mode: u32, status: *mut u32) -> Handle;
  fn vmnet_network_create(config: Handle, status: *mut u32) -> Handle;
  fn vmnet_interface_start_with_network(
    network: Handle,
    desc: Handle,
    queue: Handle,
    handler: &block2::Block<dyn Fn(u32, Handle)>,
  ) -> Handle;
  fn vmnet_interface_set_event_callback(
    iface: Handle,
    mask: u32,
    queue: Handle,
    callback: &block2::Block<dyn Fn(u32, Handle)>,
  ) -> u32;
  fn vmnet_read(iface: Handle, packets: *mut PktDesc, count: *mut i32) -> u32;
  fn vmnet_write(iface: Handle, packets: *mut PktDesc, count: *mut i32) -> u32;
}

unsafe extern "C" {
  fn dispatch_queue_create(label: *const c_char, attr: Handle) -> Handle;
  fn xpc_dictionary_create(keys: *const c_void, values: *const c_void, count: usize) -> Handle;
  fn xpc_dictionary_get_string(dict: Handle, key: *const c_char) -> *const c_char;
}

/// The vmnet interface; used only from the net thread once started.
struct Iface(Handle);
// SAFETY: vmnet_read and vmnet_write may be called from any thread.
unsafe impl Send for Iface {}

pub struct Net {
  iface: Option<Iface>,
  pub mac: [u8; 6],
  wake: Arc<Wake>,
}

impl Net {
  /// Starts an interface on a new shared network. Each start gets a new MAC address and
  /// subnet, so a new address for the guest: macOS 26 ignores the interface id it is given.
  pub fn start() -> io::Result<Net> {
    let wake = Arc::new(Wake::new()?);
    let (tx, rx) = mpsc::channel();
    // SAFETY: plain vmnet and libdispatch calls; the handler runs once on `queue`, and the
    // objects it gets are valid while it runs.
    let (iface, mac) = unsafe {
      let mut status = 0;
      let config = vmnet_network_configuration_create(VMNET_SHARED_MODE, &mut status);
      if config.is_null() {
        return Err(io::Error::other(format!("vmnet configuration: status {status}")));
      }

      let network = vmnet_network_create(config, &mut status);
      if network.is_null() {
        let msg = format!(
          "vmnet network: status {status} (signed with the virtualization entitlement? macOS 26+?)"
        );
        return Err(io::Error::other(msg));
      }

      let desc = xpc_dictionary_create(std::ptr::null(), std::ptr::null(), 0);
      let queue = dispatch_queue_create(c"lodka.vmnet".as_ptr(), std::ptr::null_mut());
      let done = RcBlock::new(move |status: u32, params: Handle| {
        let mac = match params.is_null() {
          true => None,
          false => {
            let s = xpc_dictionary_get_string(params, c"vmnet_mac_address".as_ptr());
            (!s.is_null()).then(|| CStr::from_ptr(s).to_string_lossy().into_owned())
          }
        };

        let _ = tx.send((status, mac));
      });

      let iface = vmnet_interface_start_with_network(network, desc, queue, &done);
      let (status, mac) = rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| io::Error::other("vmnet interface did not start"))?;
      if iface.is_null() || status != VMNET_SUCCESS {
        return Err(io::Error::other(format!("vmnet interface: status {status}")));
      }

      let waker = wake.clone();
      let event = RcBlock::new(move |_event: u32, _info: Handle| waker.wake());
      let status = vmnet_interface_set_event_callback(iface, PACKETS_AVAILABLE, queue, &event);
      if status != VMNET_SUCCESS {
        return Err(io::Error::other(format!("vmnet events: status {status}")));
      }

      std::mem::forget(event);
      (iface, mac)
    };

    let mac = mac
      .as_deref()
      .and_then(parse_mac)
      .ok_or_else(|| io::Error::other("vmnet gave no MAC address"))?;
    Ok(Net { iface: Some(Iface(iface)), mac, wake })
  }
}

fn parse_mac(s: &str) -> Option<[u8; 6]> {
  let parts: Vec<u8> =
    s.split(':').map(|x| u8::from_str_radix(x, 16).ok()).collect::<Option<_>>()?;
  parts.try_into().ok()
}

impl Device for Net {
  fn kind(&self) -> u32 {
    1
  }

  fn features(&self) -> u64 {
    F_MAC | F_STATUS
  }

  fn queues(&self) -> Vec<u16> {
    vec![256, 256] // receiveq, transmitq
  }

  fn config(&self) -> Vec<u8> {
    let mut c = self.mac.to_vec();
    c.extend_from_slice(&1u16.to_le_bytes()); // status: link up
    c
  }

  fn activate(&mut self, mem: Arc<Mem>, queues: Vec<Queue>, irq: Irq) -> io::Result<()> {
    let iface = self.iface.take().ok_or_else(|| io::Error::other("net started twice"))?;
    let (mut rxq, mut txq) = (queues[0].clone(), queues[1].clone());
    let mut spare: Vec<Chain> = Vec::new();
    worker("net", self.wake.clone(), move || {
      let sent = transmit(&iface, &mem, &mut txq)?;
      let got = receive(&iface, &mem, &mut rxq, &mut spare)?;
      if sent && txq.wants_irq(&mem) || got && rxq.wants_irq(&mem) {
        irq.used();
      }

      Ok(())
    })
  }

  fn notify(&mut self, _queue: u16) {
    self.wake.wake();
  }
}

// MARK: Packets

/// Sends what the guest queued; returns whether it used any buffers.
fn transmit(iface: &Iface, mem: &Mem, txq: &mut Queue) -> io::Result<bool> {
  let mut used = false;
  loop {
    txq.set_notify(mem, false)?;
    while let Some(chain) = txq.pop(mem)? {
      let len = chain.read_len().saturating_sub(HDR);
      let mut iov = chain.iovecs(mem, false, HDR, len)?;
      let mut pkt =
        PktDesc { size: len, iov: iov.as_mut_ptr(), iovcnt: iov.len() as u32, flags: 0 };
      let mut count = 1;
      // SAFETY: one packet whose iovecs point into guest memory checked by `iovecs`.
      let status = unsafe { vmnet_write(iface.0, &mut pkt, &mut count) };
      if status != VMNET_SUCCESS {
        eprintln!("lodka: net: vmnet_write: status {status}");
      }

      txq.push(mem, chain.head, 0)?;
      used = true;
    }

    txq.set_notify(mem, true)?;
    if !txq.pending(mem)? {
      return Ok(used);
    }
  }
}

/// Reads packets from vmnet straight into the guest's receive buffers; returns whether it
/// filled any. Buffers taken but not filled wait in `spare` for the next packets.
fn receive(iface: &Iface, mem: &Mem, rxq: &mut Queue, spare: &mut Vec<Chain>) -> io::Result<bool> {
  let mut used = false;
  loop {
    while spare.len() < BATCH {
      match rxq.pop(mem)? {
        Some(x) => spare.push(x),
        None => break,
      }
    }

    if spare.is_empty() {
      return Ok(used); // the guest gives more buffers with a notify, which wakes us again
    }

    let mut iovs = spare
      .iter()
      .map(|x| x.iovecs(mem, true, HDR, x.write_len().saturating_sub(HDR)))
      .collect::<io::Result<Vec<_>>>()?;

    let mut pkts: Vec<PktDesc> = iovs
      .iter_mut()
      .map(|iov| {
        let size = iov.iter().map(|x| x.iov_len).sum();
        PktDesc { size, iov: iov.as_mut_ptr(), iovcnt: iov.len() as u32, flags: 0 }
      })
      .collect();

    let mut count = pkts.len() as i32;
    // SAFETY: each packet's iovecs point into guest memory checked by `iovecs`.
    let status = unsafe { vmnet_read(iface.0, pkts.as_mut_ptr(), &mut count) };
    if status != VMNET_SUCCESS {
      return Err(io::Error::other(format!("vmnet_read: status {status}")));
    }

    if count <= 0 {
      return Ok(used);
    }

    let sizes: Vec<usize> = pkts[..count as usize].iter().map(|x| x.size).collect();
    let mut hdr = [0u8; HDR];
    hdr[10] = 1; // num_buffers
    for (chain, size) in spare.drain(..count as usize).zip(sizes) {
      chain.write_at(mem, 0, &hdr)?;
      rxq.push(mem, chain.head, (HDR + size) as u32)?;
      used = true;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn macs() {
    assert_eq!(parse_mac("4a:b8:eb:16:73:63"), Some([0x4a, 0xb8, 0xeb, 0x16, 0x73, 0x63]));
    assert_eq!(parse_mac("4a:b8:eb:16:73"), None);
    assert_eq!(parse_mac("4a:b8:eb:16:73:zz"), None);
  }
}
