// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! Hypervisor.framework: the VM, its memory, vCPUs and Apple's in-kernel GICv3 (macOS 15+).
//! A process has at most one VM; a vCPU is bound to the thread that created it.

use std::ffi::c_void;
use std::io;
use std::sync::OnceLock;
use std::time::Duration;

pub type Vcpu = u64;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Exception {
  pub syndrome: u64,
  pub virtual_address: u64,
  pub physical_address: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Exit {
  pub reason: u32,
  pub exception: Exception,
}

pub const EXIT_CANCELED: u32 = 0;
pub const EXIT_EXCEPTION: u32 = 1;
pub const EXIT_VTIMER: u32 = 2;

// hv_reg_t: X0..X30 are 0..30
pub const REG_X0: u32 = 0;
pub const REG_PC: u32 = 31;
pub const REG_CPSR: u32 = 34;

// hv_sys_reg_t
pub const SYS_MPIDR_EL1: u16 = 0xc005;
pub const SYS_ID_AA64PFR1_EL1: u16 = 0xc021;
pub const SYS_SCTLR_EL1: u16 = 0xc080;
pub const SYS_CNTV_CTL_EL0: u16 = 0xdf19;
pub const SYS_CNTV_CVAL_EL0: u16 = 0xdf1a;

const MEMORY_RWX: u64 = 0b111;

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
  fn hv_vm_create(config: *mut c_void) -> i32;
  fn hv_vm_map(addr: *mut c_void, ipa: u64, size: usize, flags: u64) -> i32;
  fn hv_vcpu_create(vcpu: *mut Vcpu, exit: *mut *const Exit, config: *mut c_void) -> i32;
  fn hv_vcpu_destroy(vcpu: Vcpu) -> i32;
  fn hv_vcpu_run(vcpu: Vcpu) -> i32;
  fn hv_vcpus_exit(vcpus: *const Vcpu, count: u32) -> i32;
  fn hv_vcpu_get_reg(vcpu: Vcpu, reg: u32, value: *mut u64) -> i32;
  fn hv_vcpu_set_reg(vcpu: Vcpu, reg: u32, value: u64) -> i32;
  fn hv_vcpu_get_sys_reg(vcpu: Vcpu, reg: u16, value: *mut u64) -> i32;
  fn hv_vcpu_set_sys_reg(vcpu: Vcpu, reg: u16, value: u64) -> i32;
  fn hv_vcpu_set_vtimer_mask(vcpu: Vcpu, masked: bool) -> i32;

  fn hv_gic_config_create() -> *mut c_void;
  fn hv_gic_config_set_distributor_base(config: *mut c_void, addr: u64) -> i32;
  fn hv_gic_config_set_redistributor_base(config: *mut c_void, addr: u64) -> i32;
  fn hv_gic_create(config: *mut c_void) -> i32;
  fn hv_gic_set_spi(intid: u32, level: bool) -> i32;
  fn hv_gic_get_distributor_size(size: *mut usize) -> i32;
  fn hv_gic_get_distributor_base_alignment(align: *mut usize) -> i32;
  fn hv_gic_get_redistributor_size(size: *mut usize) -> i32;
  fn hv_gic_get_redistributor_region_size(size: *mut usize) -> i32;
  fn hv_gic_get_redistributor_base_alignment(align: *mut usize) -> i32;
  fn hv_gic_get_spi_interrupt_range(base: *mut u32, count: *mut u32) -> i32;
  fn hv_gic_set_redistributor_reg(vcpu: Vcpu, reg: u32, value: u64) -> i32;
  fn hv_gic_get_redistributor_reg(vcpu: Vcpu, reg: u32, value: *mut u64) -> i32;

  fn os_release(object: *mut c_void);
  fn mach_absolute_time() -> u64;
  fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
}

const GICR_ISPENDR0: u32 = 0x10200;

/// The host clock that drives the guest's counter (CNTVCT_EL0 with no offset).
pub fn now_ticks() -> u64 {
  // SAFETY: no preconditions.
  unsafe { mach_absolute_time() }
}

/// Host ticks (24 MHz on Apple silicon) to a duration.
pub fn ticks_to_duration(ticks: u64) -> Duration {
  static TIMEBASE: OnceLock<(u32, u32)> = OnceLock::new();
  let (numer, denom) = *TIMEBASE.get_or_init(|| {
    let mut info = [0u32; 2];
    // SAFETY: fills one mach_timebase_info_data_t: numer, denom.
    unsafe { mach_timebase_info(&mut info) };
    (info[0], info[1])
  });

  Duration::from_nanos((ticks as u128 * numer as u128 / denom as u128) as u64)
}

fn check(ret: i32, what: &str) -> io::Result<()> {
  match ret {
    0 => Ok(()),
    // HV_DENIED: the binary lacks com.apple.security.hypervisor
    x if x as u32 == 0xfae9_4007 => Err(io::Error::other(format!("{what}: denied, not signed?"))),
    x => Err(io::Error::other(format!("{what}: error {:#x}", x as u32))),
  }
}

/// Creates the process's VM with the default 36-bit (64 GiB) address space.
pub fn vm_create() -> io::Result<()> {
  // SAFETY: a null config asks for the defaults.
  check(unsafe { hv_vm_create(std::ptr::null_mut()) }, "hv_vm_create")
}

/// Maps `size` bytes at `host` into the guest at `ipa`, readable, writable and executable.
///
/// # Safety
/// `host` must stay mapped for the VM's lifetime.
pub unsafe fn vm_map(host: *mut u8, ipa: u64, size: usize) -> io::Result<()> {
  // SAFETY: the caller keeps `host` mapped for `size` bytes while the VM lives.
  check(unsafe { hv_vm_map(host.cast(), ipa, size, MEMORY_RWX) }, "hv_vm_map")
}

// MARK: GIC

/// Where the GIC's registers sit in the guest, as the device tree describes them.
#[derive(Debug, Clone, Copy)]
pub struct Gic {
  pub dist: (u64, u64),
  pub redist: (u64, u64),
  /// The end of the redistributors' space for the most vCPUs the GIC supports (32 MiB on
  /// macOS 27): no other device may sit in it.
  pub end: u64,
  /// The first SPI's interrupt id and how many there are.
  pub spis: (u32, u32),
}

/// Creates the GIC with its distributor at `base` and the redistributors of `cpus` vCPUs after
/// it. Must run after `vm_create` and before any vCPU exists.
pub fn gic_create(base: u64, cpus: usize) -> io::Result<Gic> {
  let get = |f: unsafe extern "C" fn(*mut usize) -> i32, what| {
    let mut x = 0;
    // SAFETY: the getter writes one usize.
    check(unsafe { f(&mut x) }, what).map(|_| x as u64)
  };

  let dist_size = get(hv_gic_get_distributor_size, "gic distributor size")?;
  let dist_align = get(hv_gic_get_distributor_base_alignment, "gic distributor alignment")?;
  let redist_size = get(hv_gic_get_redistributor_size, "gic redistributor size")?;
  let region = get(hv_gic_get_redistributor_region_size, "gic redistributor region size")?;
  let redist_align = get(hv_gic_get_redistributor_base_alignment, "gic redistributor alignment")?;

  let dist = base.next_multiple_of(dist_align);
  let redist = (dist + dist_size).next_multiple_of(redist_align);
  let (mut spi_base, mut spi_count) = (0, 0);
  // SAFETY: the getter writes two u32s.
  check(unsafe { hv_gic_get_spi_interrupt_range(&mut spi_base, &mut spi_count) }, "gic spis")?;

  // SAFETY: a config made here, released once the GIC exists.
  unsafe {
    let config = hv_gic_config_create();
    let res = check(hv_gic_config_set_distributor_base(config, dist), "gic distributor")
      .and_then(|_| check(hv_gic_config_set_redistributor_base(config, redist), "gic redist"))
      .and_then(|_| check(hv_gic_create(config), "hv_gic_create"));
    os_release(config);
    res?;
  }

  let end = redist + region;
  let redist = (redist, redist_size * cpus as u64);
  Ok(Gic { dist: (dist, dist_size), redist, end, spis: (spi_base, spi_count) })
}

/// Raises or lowers a shared interrupt line.
pub fn gic_set_spi(intid: u32, level: bool) {
  // SAFETY: an unknown intid only returns an error.
  let ret = unsafe { hv_gic_set_spi(intid, level) };
  if ret != 0 {
    eprintln!("lodka: hv_gic_set_spi({intid}, {level}): error {:#x}", ret as u32);
  }
}

// MARK: vCPU

/// A vCPU of the calling thread; it is destroyed when dropped, on the same thread.
pub struct Cpu {
  pub id: Vcpu,
  exit: *const Exit,
}

impl Cpu {
  pub fn create() -> io::Result<Cpu> {
    let (mut id, mut exit) = (0, std::ptr::null());
    // SAFETY: both out-pointers are valid; a null config is the default.
    check(unsafe { hv_vcpu_create(&mut id, &mut exit, std::ptr::null_mut()) }, "hv_vcpu_create")?;
    Ok(Cpu { id, exit })
  }

  /// Runs the guest until it exits to the host, and returns why.
  pub fn run(&self) -> io::Result<Exit> {
    // SAFETY: a vCPU of this thread; the exit struct is valid until the vCPU is destroyed.
    unsafe {
      check(hv_vcpu_run(self.id), "hv_vcpu_run")?;
      Ok(*self.exit)
    }
  }

  pub fn reg(&self, reg: u32) -> u64 {
    let mut x = 0;
    // SAFETY: a vCPU of this thread; a bad register only returns an error.
    unsafe { hv_vcpu_get_reg(self.id, reg, &mut x) };
    x
  }

  pub fn set_reg(&self, reg: u32, value: u64) {
    // SAFETY: as in `reg`.
    unsafe { hv_vcpu_set_reg(self.id, reg, value) };
  }

  pub fn sys_reg(&self, reg: u16) -> u64 {
    let mut x = 0;
    // SAFETY: as in `reg`.
    unsafe { hv_vcpu_get_sys_reg(self.id, reg, &mut x) };
    x
  }

  pub fn set_sys_reg(&self, reg: u16, value: u64) -> io::Result<()> {
    // SAFETY: as in `reg`.
    check(unsafe { hv_vcpu_set_sys_reg(self.id, reg, value) }, "hv_vcpu_set_sys_reg")
  }

  pub fn set_vtimer_mask(&self, masked: bool) {
    // SAFETY: as in `reg`.
    unsafe { hv_vcpu_set_vtimer_mask(self.id, masked) };
  }

  /// Marks private interrupts (SGIs and PPIs, `bits` by intid) pending in this vCPU's
  /// redistributor.
  pub fn gic_pend(&self, bits: u32) {
    // SAFETY: as in `reg`; GICR_ISPENDR0 is write-1-to-set.
    unsafe { hv_gic_set_redistributor_reg(self.id, GICR_ISPENDR0, bits as u64) };
  }

  /// The private interrupts (SGIs and PPIs) pending for this vCPU, a bit per intid.
  pub fn gic_pending(&self) -> u32 {
    let mut x = 0;
    // SAFETY: as in `reg`.
    unsafe { hv_gic_get_redistributor_reg(self.id, GICR_ISPENDR0, &mut x) };
    x as u32
  }
}

impl Drop for Cpu {
  fn drop(&mut self) {
    // SAFETY: dropped on the thread that created it, like every Cpu.
    unsafe { hv_vcpu_destroy(self.id) };
  }
}

/// Makes the given vCPUs return from `run` (with `EXIT_CANCELED`) as soon as possible; any
/// thread may call it.
pub fn cpus_exit(ids: &[Vcpu]) {
  // SAFETY: the slice is valid; unknown ids only return an error.
  unsafe { hv_vcpus_exit(ids.as_ptr(), ids.len() as u32) };
}
