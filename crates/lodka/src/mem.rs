// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! Guest memory as the host sees it, and the trait of the devices the guest reaches through
//! loads and stores in their MMIO windows.

use std::io;
use std::sync::atomic::AtomicU16;

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

  pub fn read(&self, gpa: u64, buf: &mut [u8]) -> io::Result<()> {
    let src = self.ptr(gpa, buf.len())?;
    // SAFETY: both ranges are valid for buf.len() bytes and can't overlap.
    unsafe { std::ptr::copy_nonoverlapping(src, buf.as_mut_ptr(), buf.len()) };
    Ok(())
  }

  pub fn write(&self, gpa: u64, buf: &[u8]) -> io::Result<()> {
    let dst = self.ptr(gpa, buf.len())?;
    // SAFETY: as in `read`.
    unsafe { std::ptr::copy_nonoverlapping(buf.as_ptr(), dst, buf.len()) };
    Ok(())
  }

  pub fn read_u16(&self, gpa: u64) -> io::Result<u16> {
    let mut buf = [0; 2];
    self.read(gpa, &mut buf)?;
    Ok(u16::from_le_bytes(buf))
  }

  pub fn read_u32(&self, gpa: u64) -> io::Result<u32> {
    let mut buf = [0; 4];
    self.read(gpa, &mut buf)?;
    Ok(u32::from_le_bytes(buf))
  }

  pub fn read_u64(&self, gpa: u64) -> io::Result<u64> {
    let mut buf = [0; 8];
    self.read(gpa, &mut buf)?;
    Ok(u64::from_le_bytes(buf))
  }

  pub fn atomic_u16(&self, gpa: u64) -> io::Result<&AtomicU16> {
    if !gpa.is_multiple_of(2) {
      return Err(io::Error::new(io::ErrorKind::InvalidInput, "unaligned ring index"));
    }

    // SAFETY: in bounds and aligned; AtomicU16 has u16's layout.
    Ok(unsafe { &*self.ptr(gpa, 2)?.cast::<AtomicU16>() })
  }
}

impl Drop for Mem {
  fn drop(&mut self) {
    // SAFETY: the mapping made in `new`.
    unsafe { libc::munmap(self.ptr.cast(), self.size as usize) };
  }
}
