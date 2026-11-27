// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! Guest memory as the host sees it, and the trait of the devices the guest reaches through
//! loads and stores in their MMIO windows.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use crate::hv;

/// Guest RAM comes in pieces this big, each a memory object of its own: a piece the guest
/// freed whole goes back to the Mac at once (`give_back`).
pub const PIECE: u64 = 2 << 20;
/// A piece given back is made again of parts this big, which go back one by one: once used,
/// the guest's free memory lies around live pages left all over it, and rarely in whole pieces.
/// Each object costs the kernel 320 bytes, and making it 1.4 µs: too many for all RAM at start.
const PART: u64 = 64 << 10;
/// The Mac's page, the least it takes back.
const MAC_PAGE: u64 = 16 << 10;

// mach/vm_statistics.h, mach/memory_object_types.h, mach/vm_prot.h, mach/vm_inherit.h.
const VM_FLAGS_FIXED: i32 = 0;
const VM_FLAGS_ANYWHERE: i32 = 1;
const VM_FLAGS_OVERWRITE: i32 = 0x4000;
const MAP_MEM_NAMED_CREATE: i32 = 0x2_0000;
const MAP_MEM_LEDGER_TAGGED: i32 = 0x2000;
const VM_PROT_RW: i32 = 3;
const VM_PROT_ALL: i32 = 7;
const VM_INHERIT_COPY: u32 = 1;

unsafe extern "C" {
  /// This task's port, which `mach_task_self()` reads.
  static mach_task_self_: u32;
  fn mach_vm_allocate(task: u32, addr: *mut u64, size: u64, flags: i32) -> i32;
  fn mach_vm_deallocate(task: u32, addr: u64, size: u64) -> i32;
  fn mach_vm_map(
    task: u32,
    addr: *mut u64,
    size: u64,
    mask: u64,
    flags: i32,
    object: u32,
    offset: u64,
    copy: i32,
    cur: i32,
    max: i32,
    inherit: u32,
  ) -> i32;
  fn mach_make_memory_entry_64(
    task: u32,
    size: *mut u64,
    offset: u64,
    perm: i32,
    entry: *mut u32,
    parent: u32,
  ) -> i32;
  fn mach_port_deallocate(task: u32, name: u32) -> i32;
}

/// A device the vCPUs reach through loads and stores in its MMIO window.
pub trait Mmio: Send {
  fn read(&mut self, off: u64, data: &mut [u8]);
  fn write(&mut self, off: u64, data: &[u8]);
}

/// The guest's RAM: one range of this process, at `base` in the guest, made of `PIECE`s.
pub struct Mem {
  ptr: *mut u8,
  pub base: u64,
  pub size: u64,
  /// Mapped into the VM, which then sees each piece made again.
  vm: AtomicBool,
  /// A piece given back never went into the VM again.
  lost: AtomicBool,
  /// The pieces made of parts.
  parts: Box<[AtomicBool]>,
}

// SAFETY: the mapping lives as long as Mem; the guest writes it concurrently anyway, so all
// access goes through raw copies and atomics, never references held across calls.
unsafe impl Send for Mem {}
unsafe impl Sync for Mem {}

impl Mem {
  /// Zeroed RAM of `size` bytes, a range of pieces that each count once in the process's
  /// footprint, as `hv_vm_allocate` makes them: one `mmap` counted a page twice once both the
  /// guest and this process touched it.
  pub fn new(base: u64, size: u64) -> io::Result<Mem> {
    let mut addr = 0;
    // SAFETY: a new range anywhere in this task (VM_FLAGS_ANYWHERE).
    let r = unsafe { mach_vm_allocate(task(), &mut addr, size, VM_FLAGS_ANYWHERE) };
    kern(r, "mach_vm_allocate")?;
    let (vm, lost) = (AtomicBool::new(false), AtomicBool::new(false));
    let parts = (0..size.div_ceil(PIECE)).map(|_| AtomicBool::new(false)).collect();
    let mem = Mem { ptr: addr as *mut u8, base, size, vm, lost, parts };
    for off in (0..size).step_by(PIECE as usize) {
      mem.fresh(off, PIECE.min(size - off))?;
    }

    Ok(mem)
  }

  /// Maps the RAM into the process's VM, at `base`.
  pub fn map_into_vm(&self) -> io::Result<()> {
    // SAFETY: the range lives as long as `self`, which the machine keeps while the VM runs.
    unsafe { hv::vm_map(self.ptr, self.base, self.size as usize)? };
    self.vm.store(true, Ordering::Relaxed);
    Ok(())
  }

  /// A new memory object of `len` zeroed bytes at offset `off`, over the one there.
  fn fresh(&self, off: u64, len: u64) -> io::Result<()> {
    // SAFETY: Mach calls on this task: an entry for a new object, mapped over our own range
    // at `off`, which the old object then leaves; the mapping keeps the entry's object.
    unsafe {
      let task = task();
      let (mut size, mut entry) = (len, 0);
      let perm = MAP_MEM_NAMED_CREATE | MAP_MEM_LEDGER_TAGGED | VM_PROT_ALL;
      kern(mach_make_memory_entry_64(task, &mut size, 0, perm, &mut entry, 0), "memory entry")?;
      let mut addr = self.ptr as u64 + off;
      let flags = VM_FLAGS_FIXED | VM_FLAGS_OVERWRITE;
      let (cur, max) = (VM_PROT_RW, VM_PROT_ALL);
      let r = mach_vm_map(task, &mut addr, len, 0, flags, entry, 0, 0, cur, max, VM_INHERIT_COPY);
      mach_port_deallocate(task, entry);
      kern(r, "mach_vm_map")
    }
  }

  /// Gives the Mac back `len` bytes at `gpa`, which the guest freed and leaves alone until
  /// told. Whole pieces, and whole parts of pieces given back before, go at once and read as
  /// zeros after; the rest, in whole Mac pages, when macOS needs memory, keeping what it holds
  /// until then. Returns the bytes gone at once.
  pub fn give_back(&self, gpa: u64, len: u64) -> io::Result<u64> {
    self.ptr(gpa, len as usize)?;
    let (start, end) = (gpa - self.base, gpa - self.base + len);
    // A part of a piece still whole stays: replaced alone, it would leave its pages with the
    // piece's object, which lives on in the rest of the piece.
    let (first, last) = (start.next_multiple_of(PART), end / PART * PART);
    let in_parts = |off: u64| {
      let piece = self.parts.get((off / PIECE) as usize);
      off.is_multiple_of(PIECE) || piece.is_some_and(|x| x.load(Ordering::Relaxed))
    };
    let first = if in_parts(first) { first } else { first.next_multiple_of(PIECE) };
    let last = if in_parts(last) { last } else { last / PIECE * PIECE };
    if first >= last {
      self.advise_free(start, end);
      return Ok(0);
    }

    // The VM keeps the old objects until unmapped, and sees the new ones once mapped again.
    let vm = self.vm.load(Ordering::Relaxed);
    if vm {
      // SAFETY: the guest leaves the range alone.
      unsafe { hv::vm_unmap(self.base + first, (last - first) as usize)? };
    }

    // On an error the old objects left stay in place, and go back into the VM as they were; a
    // piece made again only in part stays unmarked, keeping its old pages until given back whole.
    // A range ends inside a piece only if it's marked: a piece is marked once its last part is.
    let made = (first..last).step_by(PART as usize).try_for_each(|off| {
      self.fresh(off, PART)?;
      if (off + PART).is_multiple_of(PIECE) {
        self.parts[(off / PIECE) as usize].store(true, Ordering::Relaxed);
      }

      io::Result::Ok(())
    });

    if vm {
      // SAFETY: `first` is within the range.
      let host = unsafe { self.ptr.add(first as usize) };
      // SAFETY: as in `map_into_vm`.
      let mapped = unsafe { hv::vm_map(host, self.base + first, (last - first) as usize) };
      self.lost.fetch_or(mapped.is_err(), Ordering::Release);
      mapped?;
    }

    made?;
    self.advise_free(start, first);
    self.advise_free(last, end);
    Ok(last - first)
  }

  /// MADV_FREE for the Mac pages wholly in `start..end`: macOS rounds outward, and would take
  /// the neighbours too.
  fn advise_free(&self, start: u64, end: u64) {
    let (start, end) = (start.next_multiple_of(MAC_PAGE), end / MAC_PAGE * MAC_PAGE);
    if start < end {
      // SAFETY: within our range; the guest doesn't need what the pages hold.
      unsafe {
        libc::madvise(self.ptr.add(start as usize).cast(), (end - start) as usize, libc::MADV_FREE)
      };
    }
  }

  /// Whether a piece given back is out of the VM for good, as `hv_vm_map` failed.
  pub fn lost(&self) -> bool {
    self.lost.load(Ordering::Acquire)
  }

  pub fn contains(&self, gpa: u64) -> bool {
    (self.base..self.base + self.size).contains(&gpa)
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
    // SAFETY: the range made in `new`.
    unsafe { mach_vm_deallocate(task(), self.ptr as u64, self.size) };
  }
}

fn task() -> u32 {
  // SAFETY: set before main, never written again.
  unsafe { mach_task_self_ }
}

fn kern(r: i32, what: &str) -> io::Result<()> {
  match r {
    0 => Ok(()),
    x => Err(io::Error::other(format!("{what}: kern_return {x}"))),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn gives_back_whole_pieces_and_spares_neighbours() {
    let base = 0x8000_0000;
    let mem = Mem::new(base, 3 * PIECE).unwrap();
    mem.write(base, &vec![0xaa; (3 * PIECE) as usize]).unwrap();
    let byte = |off: u64| {
      let mut b = [0];
      mem.read(base + off, &mut b).unwrap();
      b[0]
    };

    // From 4 KiB into the first piece to 4 KiB into the third: the second goes whole.
    assert_eq!(mem.give_back(base + 4096, 2 * PIECE).unwrap(), PIECE);
    assert_eq!((byte(PIECE), byte(2 * PIECE - 1)), (0, 0));
    // The Mac pages the range covers only in part keep what they hold.
    assert_eq!(
      (byte(0), byte(4096), byte(2 * PIECE), byte(2 * PIECE + 4096)),
      (0xaa, 0xaa, 0xaa, 0xaa)
    );

    // Less than a piece: nothing goes at once; and what went is RAM again.
    assert_eq!(mem.give_back(base + 2 * PIECE, MAC_PAGE).unwrap(), 0);
    mem.write(base + PIECE, &vec![0xbb; PIECE as usize]).unwrap();
    assert_eq!(byte(PIECE), 0xbb);

    // The piece given back is in parts since: one goes alone. Not so in a piece still whole.
    assert_eq!(mem.give_back(base + PIECE + PART - 4096, PART + 8192).unwrap(), PART);
    assert_eq!((byte(PIECE + PART), byte(PIECE + 2 * PART - 1)), (0, 0));
    assert_eq!((byte(PIECE + PART - 1), byte(PIECE + 2 * PART)), (0xbb, 0xbb));
    assert_eq!(mem.give_back(base + PART, PART).unwrap(), 0);
    assert!(mem.give_back(base + 3 * PIECE, 1).is_err());
  }
}
