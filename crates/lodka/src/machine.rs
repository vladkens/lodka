// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The virtual machine: guest memory, the GIC, devices on an MMIO bus, a device tree, and one
//! thread per vCPU that runs the guest and serves its exits (MMIO, PSCI calls, WFI, timers).
//! Boots an arm64 Linux `Image` directly, as https://docs.kernel.org/arch/arm64/booting.html
//! describes; no firmware.

use std::fs::File;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, thread};

use crate::fdt::Fdt;
use crate::hv;
use crate::mem::{Mem, Mmio};
use crate::virtio::{Device, Irq, Kick, Transport};

// Guest physical layout: the devices as QEMU's `virt` machine has them, the GIC after them, as
// its redistributors' space is larger than `virt` leaves for it.
const UART_BASE: u64 = 0x0900_0000;
const RTC_BASE: u64 = 0x0901_0000;
const VIRTIO_BASE: u64 = 0x0a00_0000;
const VIRTIO_SIZE: u64 = 0x200;
const GIC_BASE: u64 = 0x1000_0000;
pub const RAM_BASE: u64 = 0x4000_0000;
const _: () =
  assert!(RTC_BASE + 0x1000 <= VIRTIO_BASE && VIRTIO_BASE + 64 * VIRTIO_SIZE <= GIC_BASE);

const PHANDLE_GIC: u32 = 1;
const PHANDLE_CLOCK: u32 = 2;
// FDT interrupt specifiers: <kind number flags>
const SPI: u32 = 0;
const PPI: u32 = 1;
const LEVEL_HIGH: u32 = 4;
const VTIMER_INTID: u32 = 27;

/// EL1h with debug, SError, IRQ and FIQ masked, as the boot protocol wants.
const CPSR_EL1H_MASKED: u64 = 0x3c5;

pub struct Config {
  pub cpus: usize,
  pub memory: u64,
  pub kernel: PathBuf,
  pub cmdline: String,
  /// Where the guest's early console (PL011) writes.
  pub console: Arc<Mutex<File>>,
}

/// Why the VM stopped.
#[derive(Debug, Clone, PartialEq)]
pub enum Stop {
  Poweroff,
  Reboot,
  Error(String),
}

// MARK: Bus

struct Region {
  base: u64,
  size: u64,
  dev: Mutex<Box<dyn Mmio>>,
}

#[derive(Default)]
struct Bus {
  regions: Vec<Region>,
}

impl Bus {
  fn add(&mut self, base: u64, size: u64, dev: impl Mmio + 'static) {
    self.regions.push(Region { base, size, dev: Mutex::new(Box::new(dev)) });
  }

  fn find(&self, addr: u64) -> Option<&Region> {
    self.regions.iter().find(|x| addr >= x.base && addr < x.base + x.size)
  }
}

// MARK: vCPU state

#[derive(Default)]
struct State {
  /// Where each vCPU starts, set by PSCI CPU_ON: entry point and its x0.
  starts: Vec<Option<(u64, u64)>>,
  on: Vec<bool>,
  /// Bumped by every interrupt a device raises, so an idle vCPU knows to look.
  generation: u64,
  stop: Option<Stop>,
}

struct Cpus {
  state: Mutex<State>,
  cond: Condvar,
  ids: Mutex<Vec<Option<hv::Vcpu>>>,
  /// How often an idle vCPU checks the GIC for interrupts from other vCPUs (SGIs), which
  /// arrive without a kick.
  poll: Duration,
}

impl Cpus {
  /// Always bumps `generation`: a vCPU that read it before entering the guest and then idles
  /// must see the interrupt raised in between.
  fn kick(&self) {
    self.state.lock().unwrap().generation += 1;
    self.cond.notify_all();
  }

  fn stop(&self, why: Stop) {
    let mut st = self.state.lock().unwrap();
    if st.stop.is_none() {
      st.stop = Some(why);
    }

    drop(st);
    self.cond.notify_all();
    let ids: Vec<_> = self.ids.lock().unwrap().iter().flatten().copied().collect();
    hv::cpus_exit(&ids);
  }
}

/// Stops a running machine from another thread.
#[derive(Clone)]
pub struct Stopper(Arc<Cpus>);

impl Stopper {
  pub fn stop(&self, why: Stop) {
    self.0.stop(why);
  }
}

// MARK: Machine

pub struct Machine {
  pub mem: Arc<Mem>,
  cfg: Config,
  gic: hv::Gic,
  bus: Bus,
  cpus: Arc<Cpus>,
  /// Virtio devices: MMIO base and SPI intid.
  virtio: Vec<(u64, u32)>,
}

impl Machine {
  pub fn new(cfg: Config) -> io::Result<Machine> {
    hv::vm_create()?;
    let mem = Arc::new(Mem::new(RAM_BASE, cfg.memory)?);
    // SAFETY: the mapping lives in `mem`, which the machine keeps until the process exits.
    unsafe { hv::vm_map(mem.host(), RAM_BASE, cfg.memory as usize)? };
    let gic = hv::gic_create(GIC_BASE, cfg.cpus)?;
    if gic.end > RAM_BASE {
      return Err(io::Error::other(format!("the GIC's space ends at {:#x}, in RAM", gic.end)));
    }

    let poll = std::env::var("LODKA_WFI_POLL_US").ok().and_then(|x| x.parse().ok());
    let state =
      State { starts: vec![None; cfg.cpus], on: vec![false; cfg.cpus], ..Default::default() };
    let cpus = Arc::new(Cpus {
      state: Mutex::new(state),
      cond: Condvar::new(),
      ids: Mutex::new(vec![None; cfg.cpus]),
      poll: Duration::from_micros(poll.unwrap_or(1000)),
    });

    let mut bus = Bus::default();
    bus.add(UART_BASE, 0x1000, Uart { out: cfg.console.clone() });
    bus.add(RTC_BASE, 0x1000, Rtc::default());

    Ok(Machine { mem, cfg, gic, bus, cpus, virtio: Vec::new() })
  }

  pub fn kick(&self) -> Kick {
    let cpus = self.cpus.clone();
    Arc::new(move || cpus.kick())
  }

  pub fn stopper(&self) -> Stopper {
    Stopper(self.cpus.clone())
  }

  /// Adds a virtio device in the next MMIO slot, with the next free SPI.
  pub fn add(&mut self, dev: Box<dyn Device>) {
    let i = self.virtio.len() as u64;
    let base = VIRTIO_BASE + i * VIRTIO_SIZE;
    // SPIs 0-15 are left for the platform devices (UART 1, RTC 2)
    let intid = self.gic.spis.0 + 16 + i as u32;
    let irq = Irq::new(intid, self.kick());
    self.bus.add(base, VIRTIO_SIZE, Transport::new(dev, self.mem.clone(), irq));
    self.virtio.push((base, intid));
  }

  /// Loads the kernel and runs the VM until it powers off, reboots or fails.
  pub fn run(self) -> io::Result<Stop> {
    let entry = self.load_kernel()?;
    let fdt = self.fdt();
    if fdt.len() > 0x20_0000 {
      return Err(io::Error::other("device tree over 2 MiB"));
    }

    // At the top of RAM, in a 2 MiB block of its own.
    let fdt_addr = RAM_BASE + self.cfg.memory - 0x20_0000;
    self.mem.write(fdt_addr, &fdt)?;

    let ncpus = self.cfg.cpus;
    let me = Arc::new(self);
    let made = Arc::new(Barrier::new(ncpus));
    let threads = (0..ncpus)
      .map(|idx| {
        let (me, made) = (me.clone(), made.clone());
        let boot = (idx == 0).then_some((entry, fdt_addr));
        thread::Builder::new().name(format!("vcpu{idx}")).spawn(move || {
          if let Err(e) = me.cpu(idx, boot, &made) {
            me.cpus.stop(Stop::Error(format!("vcpu{idx}: {e}")));
          }
        })
      })
      .collect::<io::Result<Vec<_>>>()?;

    let mut st = me.cpus.state.lock().unwrap();
    while st.stop.is_none() {
      st = me.cpus.cond.wait(st).unwrap();
    }

    let stop = st.stop.clone().unwrap();
    drop(st);
    for x in threads {
      let _ = x.join();
    }

    Ok(stop)
  }

  /// Puts the `Image` at the start of RAM (it needs a 2 MiB aligned base plus its text offset)
  /// and returns its entry point.
  fn load_kernel(&self) -> io::Result<u64> {
    let image = fs::read(&self.cfg.kernel)?;
    if image.len() < 64 || &image[0x38..0x3c] != b"ARM\x64" {
      return Err(io::Error::other(format!("{}: not an arm64 Image", self.cfg.kernel.display())));
    }

    let text_offset = u64::from_le_bytes(image[0x08..0x10].try_into().unwrap());
    let image_size = u64::from_le_bytes(image[0x10..0x18].try_into().unwrap());
    if text_offset + image_size.max(image.len() as u64) > self.cfg.memory / 2 {
      return Err(io::Error::other("kernel does not fit in the low half of RAM"));
    }

    let addr = RAM_BASE + text_offset;
    self.mem.write(addr, &image)?;
    Ok(addr)
  }

  fn fdt(&self) -> Vec<u8> {
    // The device tree numbers SPIs from 0; their interrupt ids start at 32.
    let spi = |intid: u32| intid - 32;
    let mut f = Fdt::new();
    f.begin("");
    f.prop_str("compatible", "linux,dummy-virt");
    f.prop_u32("#address-cells", 2);
    f.prop_u32("#size-cells", 2);
    f.prop_u32("interrupt-parent", PHANDLE_GIC);

    f.begin("chosen");
    f.prop_str("bootargs", &self.cfg.cmdline);
    f.prop_str("stdout-path", &format!("/pl011@{UART_BASE:x}"));
    f.end();

    f.begin(&format!("memory@{RAM_BASE:x}"));
    f.prop_str("device_type", "memory");
    f.prop_u64s("reg", &[RAM_BASE, self.cfg.memory]);
    f.end();

    f.begin("cpus");
    f.prop_u32("#address-cells", 1);
    f.prop_u32("#size-cells", 0);
    for i in 0..self.cfg.cpus {
      f.begin(&format!("cpu@{:x}", mpidr(i)));
      f.prop_str("device_type", "cpu");
      f.prop_str("compatible", "arm,arm-v8");
      f.prop_str("enable-method", "psci");
      f.prop_u32("reg", mpidr(i) as u32);
      f.end();
    }

    f.end();

    f.begin("psci");
    f.prop_strs("compatible", &["arm,psci-0.2", "arm,psci"]);
    f.prop_str("method", "hvc");
    f.end();

    let (dist, redist) = (self.gic.dist, self.gic.redist);
    f.begin(&format!("intc@{:x}", dist.0));
    f.prop_str("compatible", "arm,gic-v3");
    f.prop_u32("#interrupt-cells", 3);
    f.prop_null("interrupt-controller");
    f.prop_u64s("reg", &[dist.0, dist.1, redist.0, redist.1]);
    f.prop_u32("phandle", PHANDLE_GIC);
    f.end();

    f.begin("timer");
    f.prop_str("compatible", "arm,armv8-timer");
    f.prop_u32s(
      "interrupts",
      &[PPI, 13, LEVEL_HIGH, PPI, 14, LEVEL_HIGH, PPI, 11, LEVEL_HIGH, PPI, 10, LEVEL_HIGH],
    );
    f.prop_null("always-on");
    f.end();

    f.begin("apb-pclk");
    f.prop_str("compatible", "fixed-clock");
    f.prop_u32("#clock-cells", 0);
    f.prop_u32("clock-frequency", 24_000_000);
    f.prop_str("clock-output-names", "clk24mhz");
    f.prop_u32("phandle", PHANDLE_CLOCK);
    f.end();

    f.begin(&format!("pl011@{UART_BASE:x}"));
    f.prop_strs("compatible", &["arm,pl011", "arm,primecell"]);
    f.prop_u64s("reg", &[UART_BASE, 0x1000]);
    f.prop_u32s("interrupts", &[SPI, 1, LEVEL_HIGH]);
    f.prop_u32s("clocks", &[PHANDLE_CLOCK, PHANDLE_CLOCK]);
    f.prop_strs("clock-names", &["uartclk", "apb_pclk"]);
    f.end();

    f.begin(&format!("pl031@{RTC_BASE:x}"));
    f.prop_strs("compatible", &["arm,pl031", "arm,primecell"]);
    f.prop_u64s("reg", &[RTC_BASE, 0x1000]);
    f.prop_u32s("interrupts", &[SPI, 2, LEVEL_HIGH]);
    f.prop_u32("clocks", PHANDLE_CLOCK);
    f.prop_str("clock-names", "apb_pclk");
    f.end();

    for &(base, intid) in &self.virtio {
      f.begin(&format!("virtio_mmio@{base:x}"));
      f.prop_str("compatible", "virtio,mmio");
      f.prop_u64s("reg", &[base, VIRTIO_SIZE]);
      f.prop_u32s("interrupts", &[SPI, spi(intid), LEVEL_HIGH]);
      f.prop_null("dma-coherent");
      f.end();
    }

    f.end();
    f.finish()
  }

  // MARK: vCPU loop

  /// A vCPU's thread. Every vCPU is made before any runs: the GIC takes its topology as final
  /// once one does.
  fn cpu(&self, idx: usize, boot: Option<(u64, u64)>, made: &Barrier) -> io::Result<()> {
    let cpu = self.make_cpu(idx);
    made.wait();
    let cpu = cpu?;

    let mut start = boot;
    loop {
      let Some((entry, x0)) = start.take().or_else(|| self.wait_start(idx)) else {
        return Ok(());
      };

      cpu.set_reg(hv::REG_PC, entry);
      cpu.set_reg(hv::REG_X0, x0);
      for r in 1..4 {
        cpu.set_reg(hv::REG_X0 + r, 0);
      }

      cpu.set_reg(hv::REG_CPSR, CPSR_EL1H_MASKED);
      self.cpus.state.lock().unwrap().on[idx] = true;
      match self.run_cpu(&cpu)? {
        Next::Off => self.cpus.state.lock().unwrap().on[idx] = false,
        Next::Exit => return Ok(()),
      }
    }
  }

  fn make_cpu(&self, idx: usize) -> io::Result<hv::Cpu> {
    let cpu = hv::Cpu::create()?;
    cpu.set_sys_reg(hv::SYS_MPIDR_EL1, mpidr(idx))?;
    // No SME: Linux would try to use it, and HVF doesn't hand it to guests by default.
    let pfr1 = cpu.sys_reg(hv::SYS_ID_AA64PFR1_EL1);
    cpu.set_sys_reg(hv::SYS_ID_AA64PFR1_EL1, pfr1 & !(0xf << 24))?;
    self.cpus.ids.lock().unwrap()[idx] = Some(cpu.id);
    Ok(cpu)
  }

  /// Waits for a PSCI CPU_ON naming this vCPU; `None` once the VM stops.
  fn wait_start(&self, idx: usize) -> Option<(u64, u64)> {
    let mut st = self.cpus.state.lock().unwrap();
    loop {
      if st.stop.is_some() {
        return None;
      }

      if let Some(x) = st.starts[idx].take() {
        return Some(x);
      }

      st = self.cpus.cond.wait(st).unwrap();
    }
  }

  fn run_cpu(&self, cpu: &hv::Cpu) -> io::Result<Next> {
    let mut vtimer_masked = false;
    loop {
      let generation = {
        let st = self.cpus.state.lock().unwrap();
        if st.stop.is_some() {
          return Ok(Next::Exit);
        }

        st.generation
      };

      if vtimer_masked && !vtimer_asserted(cpu) {
        cpu.set_vtimer_mask(false);
        vtimer_masked = false;
      }

      let exit = cpu.run()?;
      match exit.reason {
        hv::EXIT_CANCELED => continue,
        hv::EXIT_VTIMER => {
          // HVF masks the timer and leaves raising its interrupt to the GIC's owner: pend
          // the PPI and unmask once the guest has dealt with it (checked on later exits).
          cpu.gic_pend(1 << VTIMER_INTID);
          vtimer_masked = true;
          continue;
        }
        hv::EXIT_EXCEPTION => {}
        x => return Err(io::Error::other(format!("unexpected exit reason {x}"))),
      }

      let syndrome = exit.exception.syndrome;
      match syndrome >> 26 & 0x3f {
        // WFI or WFE
        0x01 => {
          advance(cpu);
          if syndrome & 1 == 0 {
            self.wfi(cpu, generation);
          }
        }

        // HVC returns to the next instruction already; SMC (trapped) does not.
        0x16 => {
          if let Some(next) = self.psci(cpu)? {
            return Ok(next);
          }
        }
        0x17 => {
          advance(cpu);
          if let Some(next) = self.psci(cpu)? {
            return Ok(next);
          }
        }

        // Trapped system registers (debug, implementation defined): reads as zero, writes are
        // ignored.
        0x18 => {
          advance(cpu);
          let rt = (syndrome >> 5 & 0x1f) as u32;
          if syndrome & 1 == 1 && rt != 31 {
            cpu.set_reg(hv::REG_X0 + rt, 0);
          }
        }

        // A data abort outside RAM: a device's registers
        0x24 => {
          self.mmio(cpu, syndrome, exit.exception.physical_address)?;
          advance(cpu);
        }
        ec => {
          let pc = cpu.reg(hv::REG_PC);
          return Err(io::Error::other(format!("unhandled exception class {ec:#x} at pc {pc:#x}")));
        }
      }
    }
  }

  fn mmio(&self, cpu: &hv::Cpu, syndrome: u64, addr: u64) -> io::Result<()> {
    if syndrome & (1 << 24) == 0 {
      let pc = cpu.reg(hv::REG_PC);
      return Err(io::Error::other(format!("MMIO at {addr:#x} without syndrome, pc {pc:#x}")));
    }

    let len = 1usize << (syndrome >> 22 & 3);
    let rt = (syndrome >> 16 & 0x1f) as u32;
    let write = syndrome & (1 << 6) != 0;
    let Some(region) = self.bus.find(addr) else {
      // Nothing there: reads as zero, writes are dropped, as on hardware with no device.
      if !write && rt != 31 {
        cpu.set_reg(hv::REG_X0 + rt, 0);
      }

      return Ok(());
    };

    let off = addr - region.base;
    let mut buf = [0u8; 8];
    if write {
      let val = if rt == 31 { 0 } else { cpu.reg(hv::REG_X0 + rt) };
      buf.copy_from_slice(&val.to_le_bytes());
      region.dev.lock().unwrap().write(off, &buf[..len]);
    } else {
      region.dev.lock().unwrap().read(off, &mut buf[..len]);
      if rt != 31 {
        cpu.set_reg(hv::REG_X0 + rt, u64::from_le_bytes(buf));
      }
    }

    Ok(())
  }

  /// PSCI 0.2 over HVC: https://developer.arm.com/documentation/den0022
  fn psci(&self, cpu: &hv::Cpu) -> io::Result<Option<Next>> {
    const NOT_SUPPORTED: u64 = -1i64 as u64;
    const INVALID_PARAMETERS: u64 = -2i64 as u64;

    let ret = match cpu.reg(hv::REG_X0) as u32 {
      0x8400_0000 => 2,                          // PSCI_VERSION: 0.2
      0x8400_0002 => return Ok(Some(Next::Off)), // CPU_OFF
      // CPU_ON
      0x8400_0003 | 0xc400_0003 => {
        let (entry, ctx) = (cpu.reg(hv::REG_X0 + 2), cpu.reg(hv::REG_X0 + 3));
        match self.cpu_index(cpu.reg(hv::REG_X0 + 1)) {
          Some(i) => {
            self.cpus.state.lock().unwrap().starts[i] = Some((entry, ctx));
            self.cpus.cond.notify_all();
            0
          }
          None => INVALID_PARAMETERS,
        }
      }

      // AFFINITY_INFO: 0 on, 1 off
      0x8400_0004 | 0xc400_0004 => {
        let st = self.cpus.state.lock().unwrap();
        match self.cpu_index(cpu.reg(hv::REG_X0 + 1)) {
          Some(i) => (!st.on[i]) as u64,
          None => INVALID_PARAMETERS,
        }
      }

      0x8400_0006 => 2, // MIGRATE_INFO_TYPE: no trusted OS
      // SYSTEM_OFF
      0x8400_0008 => {
        self.cpus.stop(Stop::Poweroff);
        return Ok(Some(Next::Exit));
      }
      // SYSTEM_RESET
      0x8400_0009 => {
        self.cpus.stop(Stop::Reboot);
        return Ok(Some(Next::Exit));
      }
      0x8400_0001 | 0xc400_0001 => 0, // CPU_SUSPEND: a WFI the guest will do anyway
      _ => NOT_SUPPORTED,
    };

    cpu.set_reg(hv::REG_X0, ret);
    Ok(None)
  }

  /// The vCPU with the affinity in a PSCI call's `target_cpu`.
  fn cpu_index(&self, target: u64) -> Option<usize> {
    (0..self.cfg.cpus).find(|&i| mpidr(i) == target & 0xff_00ff_ffff)
  }

  /// Idles a vCPU that ran WFI until an interrupt may be pending for it: a device raised one
  /// (`generation` moved), its timer fires, or the GIC has a private one (polled).
  fn wfi(&self, cpu: &hv::Cpu, generation: u64) {
    if cpu.gic_pending() != 0 {
      return;
    }

    let deadline = timer_deadline(cpu);
    let cpus = &self.cpus;
    let mut st = cpus.state.lock().unwrap();
    loop {
      if st.generation != generation || st.stop.is_some() {
        break;
      }

      let left = deadline.map(|x| x.saturating_sub(hv::now_ticks()));
      if left == Some(0) {
        break;
      }

      let wait = left.map_or(cpus.poll, |x| hv::ticks_to_duration(x).min(cpus.poll));
      st = cpus.cond.wait_timeout(st, wait).unwrap().0;
      if cpu.gic_pending() != 0 {
        break;
      }
    }
  }
}

enum Next {
  /// The vCPU turned itself off (PSCI CPU_OFF) and waits for a CPU_ON.
  Off,
  /// The VM stops.
  Exit,
}

/// Affinity of vCPU `i`: 16 to a cluster, so GICv3's 16-bit SGI target lists reach them all.
fn mpidr(i: usize) -> u64 {
  (((i / 16) << 8) | (i % 16)) as u64
}

fn advance(cpu: &hv::Cpu) {
  cpu.set_reg(hv::REG_PC, cpu.reg(hv::REG_PC) + 4);
}

/// Whether the virtual timer still asks for its interrupt: enabled, not masked, condition met.
fn vtimer_asserted(cpu: &hv::Cpu) -> bool {
  cpu.sys_reg(hv::SYS_CNTV_CTL_EL0) & 0b111 == 0b101
}

/// When the virtual timer fires, in host ticks, if it is armed.
fn timer_deadline(cpu: &hv::Cpu) -> Option<u64> {
  let ctl = cpu.sys_reg(hv::SYS_CNTV_CTL_EL0);
  (ctl & 0b11 == 0b01).then(|| cpu.sys_reg(hv::SYS_CNTV_CVAL_EL0))
}

// MARK: PL011

/// The PL011 UART, output only: the kernel's earliest messages, before virtio-console.
/// https://developer.arm.com/documentation/ddi0183
struct Uart {
  out: Arc<Mutex<File>>,
}

impl Mmio for Uart {
  fn read(&mut self, off: u64, data: &mut [u8]) {
    let val: u32 = match off {
      0x018 => 0x90, // FR: transmit FIFO empty, receive FIFO empty
      0xfe0..=0xffc => primecell_id(off, [0x11, 0x10, 0x34, 0x00]),
      _ => 0,
    };

    let n = data.len().min(4);
    data[..n].copy_from_slice(&val.to_le_bytes()[..n]);
  }

  fn write(&mut self, off: u64, data: &[u8]) {
    if off == 0 {
      let _ = self.out.lock().unwrap().write_all(&data[..1]);
    }
  }
}

/// AMBA PrimeCell peripheral id (0xfe0..0xfec) and cell id (0xff0..0xffc) bytes.
fn primecell_id(off: u64, periph: [u8; 4]) -> u32 {
  let cell = [0x0d, 0xf0, 0x05, 0xb1];
  let i = ((off - 0xfe0) / 4) as usize;
  if i < 4 { periph[i] as u32 } else { cell[i - 4] as u32 }
}

// MARK: PL031

/// The PL031 real-time clock: the guest reads the host's time at boot.
/// https://developer.arm.com/documentation/ddi0224
#[derive(Default)]
struct Rtc {
  /// Seconds the guest added by writing the load register.
  offset: i64,
}

impl Mmio for Rtc {
  fn read(&mut self, off: u64, data: &mut [u8]) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let val: u32 = match off {
      0x000 => (now + self.offset) as u32, // RTCDR
      0x00c => 1,                          // RTCCR: enabled
      0xfe0..=0xffc => primecell_id(off, [0x31, 0x10, 0x04, 0x00]),
      _ => 0,
    };

    let n = data.len().min(4);
    data[..n].copy_from_slice(&val.to_le_bytes()[..n]);
  }

  fn write(&mut self, off: u64, data: &[u8]) {
    if off == 0x008 && data.len() >= 4 {
      let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
      let load = u32::from_le_bytes(data[..4].try_into().unwrap());
      self.offset = load as i64 - now;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn mpidr_clusters_of_16() {
    assert_eq!(mpidr(0), 0);
    assert_eq!(mpidr(15), 15);
    assert_eq!(mpidr(16), 0x100);
    assert_eq!(mpidr(33), 0x201);
  }

  #[test]
  fn primecell_ids() {
    let ids: Vec<u32> =
      (0..8).map(|i| primecell_id(0xfe0 + 4 * i, [0x11, 0x10, 0x34, 0])).collect();
    assert_eq!(ids, [0x11, 0x10, 0x34, 0, 0x0d, 0xf0, 0x05, 0xb1]);
  }
}
