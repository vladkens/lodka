// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! Guest memory as the host sees it, and the trait of the devices the guest reaches through
//! loads and stores in their MMIO windows.

use std::io;

/// A device the vCPUs reach through loads and stores in its MMIO window.
pub trait Mmio: Send {
  fn read(&mut self, off: u64, data: &mut [u8]);
  fn write(&mut self, off: u64, data: &[u8]);
}

/// The guest's RAM: one anonymous mapping, at `base` in the guest.
pub struct Mem {
  ptr: *mut u8,
  pub base: u64,
  pub size: u64,
}

// SAFETY: the mapping lives as long as Mem; the guest writes it concurrently anyway, so all
// access goes through raw copies and atomics, never references held across calls.
unsafe impl Send for Mem {}
unsafe impl Sync for Mem {}

impl Mem {
  pub fn new(base: u64, size: u64) -> io::Result<Mem> {
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    let flags = libc::MAP_PRIVATE | libc::MAP_ANON;
    // SAFETY: a fresh anonymous mapping.
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), size as usize, prot, flags, -1, 0) };
    if ptr == libc::MAP_FAILED {
      return Err(io::Error::last_os_error());
    }

    Ok(Mem { ptr: ptr.cast(), base, size })
  }

  pub fn host(&self) -> *mut u8 {
    self.ptr
  }

  /// The host address of `len` bytes at guest address `gpa`.
  pub fn ptr(&self, gpa: u64, len: usize) -> io::Result<*mut u8> {
    let off = gpa.wrapping_sub(self.base);
    if gpa < self.base || off.checked_add(len as u64).is_none_or(|end| end > self.size) {
      let msg = format!("guest address {gpa:#x}+{len:#x} outside RAM");
      return Err(io::Error::new(io::ErrorKind::InvalidInput, msg));
    }

    // SAFETY: in bounds of the mapping, checked above.
    Ok(unsafe { self.ptr.add(off as usize) })
  }

  pub fn write(&self, gpa: u64, buf: &[u8]) -> io::Result<()> {
    let dst = self.ptr(gpa, buf.len())?;
    // SAFETY: both ranges are valid for buf.len() bytes and can't overlap.
    unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), dst, buf.len()) };
    Ok(())
  }
}

impl Drop for Mem {
  fn drop(&mut self) {
    // SAFETY: the mapping made in `new`.
    unsafe { libc::munmap(self.ptr.cast(), self.size as usize) };
  }
}
